//! Non-Volatile Storage (NVS)
use core::ptr;

extern crate alloc;
use alloc::sync::Arc;

use ::log::*;

use embedded_svc::storage::{RawStorage, StorageBase};

use crate::sys::*;

use crate::handle::RawHandle;
use crate::private::cstr::*;
use crate::private::mutex;

static DEFAULT_TAKEN: mutex::Mutex<bool> = mutex::Mutex::new(false);
static NONDEFAULT_LOCKED: mutex::Mutex<alloc::collections::BTreeSet<CString>> =
    mutex::Mutex::new(alloc::collections::BTreeSet::new());

pub type EspDefaultNvsPartition = EspNvsPartition<NvsDefault>;
pub type EspCustomNvsPartition = EspNvsPartition<NvsCustom>;
pub type EspEncryptedNvsPartition = EspNvsPartition<NvsEncrypted>;

pub trait NvsPartitionId {
    fn is_default(&self) -> bool {
        self.name().to_bytes().is_empty()
    }

    fn name(&self) -> &CStr;
}

pub struct NvsDefault(());

#[repr(u32)]
#[allow(non_upper_case_globals)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvsDataType {
    U8 = nvs_type_t_NVS_TYPE_U8,
    I8 = nvs_type_t_NVS_TYPE_I8,
    U16 = nvs_type_t_NVS_TYPE_U16,
    I16 = nvs_type_t_NVS_TYPE_I16,
    U32 = nvs_type_t_NVS_TYPE_U32,
    I32 = nvs_type_t_NVS_TYPE_I32,
    U64 = nvs_type_t_NVS_TYPE_U64,
    I64 = nvs_type_t_NVS_TYPE_I64,
    Str = nvs_type_t_NVS_TYPE_STR,
    Blob = nvs_type_t_NVS_TYPE_BLOB,
    Any = nvs_type_t_NVS_TYPE_ANY,
}

#[allow(non_upper_case_globals)]
impl From<nvs_type_t> for NvsDataType {
    fn from(nvs_type: nvs_type_t) -> Self {
        match nvs_type {
            nvs_type_t_NVS_TYPE_U8 => NvsDataType::U8,
            nvs_type_t_NVS_TYPE_I8 => NvsDataType::I8,
            nvs_type_t_NVS_TYPE_U16 => NvsDataType::U16,
            nvs_type_t_NVS_TYPE_I16 => NvsDataType::I16,
            nvs_type_t_NVS_TYPE_U32 => NvsDataType::U32,
            nvs_type_t_NVS_TYPE_I32 => NvsDataType::I32,
            nvs_type_t_NVS_TYPE_U64 => NvsDataType::U64,
            nvs_type_t_NVS_TYPE_I64 => NvsDataType::I64,
            nvs_type_t_NVS_TYPE_STR => NvsDataType::Str,
            nvs_type_t_NVS_TYPE_BLOB => NvsDataType::Blob,
            _ => NvsDataType::Any,
        }
    }
}

impl NvsDefault {
    fn new(reinit: bool) -> Result<Self, EspError> {
        let mut taken = DEFAULT_TAKEN.lock();

        if *taken {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_STATE>());
        }

        let default_nvs = Self::init(reinit)?;

        *taken = true;
        Ok(default_nvs)
    }

    fn init(reinit: bool) -> Result<Self, EspError> {
        if let Some(err) = EspError::from(unsafe { nvs_flash_init() }) {
            match err.code() {
                ESP_ERR_NVS_NO_FREE_PAGES | ESP_ERR_NVS_NEW_VERSION_FOUND if reinit => {
                    if err.code() == ESP_ERR_NVS_NEW_VERSION_FOUND {
                        warn!("NVS partition has a new version, erasing and re-initializing the partition");
                    } else {
                        warn!("NVS partition has no free pages, erasing and re-initializing the partition");
                    }

                    esp!(unsafe { nvs_flash_erase() })?;
                    esp!(unsafe { nvs_flash_init() })?;
                }
                _ => Err(err)?,
            }
        }

        Ok(Self(()))
    }
}

impl Drop for NvsDefault {
    fn drop(&mut self) {
        //esp!(nvs_flash_deinit()).unwrap(); TODO: To be checked why it fails
        *DEFAULT_TAKEN.lock() = false;

        info!("NvsDefault dropped");
    }
}

impl NvsPartitionId for NvsDefault {
    #[allow(clippy::manual_c_str_literals)]
    fn name(&self) -> &CStr {
        CStr::from_bytes_with_nul(b"\0").unwrap()
    }
}

pub struct NvsCustom(CString);

impl NvsCustom {
    fn new(partition: &str) -> Result<Self, EspError> {
        let mut registrations = NONDEFAULT_LOCKED.lock();

        Self::init(partition, &mut registrations)
    }

    fn init(
        partition: &str,
        registrations: &mut alloc::collections::BTreeSet<CString>,
    ) -> Result<Self, EspError> {
        let c_partition = to_cstring_arg(partition)?;

        if registrations.contains(c_partition.as_ref()) {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_STATE>());
        }

        unsafe {
            if let Some(err) = EspError::from(nvs_flash_init_partition(c_partition.as_ptr())) {
                match err.code() {
                    ESP_ERR_NVS_NO_FREE_PAGES | ESP_ERR_NVS_NEW_VERSION_FOUND => {
                        esp!(nvs_flash_erase_partition(c_partition.as_ptr()))?;
                        esp!(nvs_flash_init_partition(c_partition.as_ptr()))?;
                    }
                    _ => Err(err)?,
                }
            }
        }

        registrations.insert(c_partition.clone());

        Ok(Self(c_partition))
    }
}

impl Drop for NvsCustom {
    fn drop(&mut self) {
        {
            let mut registrations = NONDEFAULT_LOCKED.lock();

            esp!(unsafe { nvs_flash_deinit_partition(self.0.as_ptr()) }).unwrap();
            registrations.remove(self.0.as_ref());
        }

        info!("NvsCustom dropped");
    }
}

impl NvsPartitionId for NvsCustom {
    fn name(&self) -> &CStr {
        self.0.as_c_str()
    }
}
pub struct NvsEncrypted(CString);

impl NvsEncrypted {
    fn new(partition: &str, key_partition: Option<&str>) -> Result<Self, EspError> {
        let mut registrations = NONDEFAULT_LOCKED.lock();

        Self::init(partition, key_partition, &mut registrations)
    }

    fn init(
        partition: &str,
        key_partition: Option<&str>,
        registrations: &mut alloc::collections::BTreeSet<CString>,
    ) -> Result<Self, EspError> {
        let c_partition = to_cstring_arg(partition)?;

        if registrations.contains(c_partition.as_ref()) {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_STATE>());
        }

        let c_key_partition = if let Some(key_partition) = key_partition {
            Some(to_cstring_arg(key_partition)?)
        } else {
            None
        };

        let keys_partition_ptr = unsafe {
            esp_partition_find_first(
                esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_DATA_NVS_KEYS,
                match c_key_partition {
                    Some(ref v) => v.as_ptr(),
                    None => core::ptr::null(),
                },
            )
        };

        if keys_partition_ptr.is_null() {
            warn!("No NVS keys partition found");
            return Err(EspError::from_infallible::<ESP_FAIL>());
        }

        let mut config = nvs_sec_cfg_t::default();
        match unsafe { nvs_flash_read_security_cfg(keys_partition_ptr, &mut config as *mut _) } {
            ESP_ERR_NVS_KEYS_NOT_INITIALIZED | ESP_ERR_NVS_CORRUPT_KEY_PART => {
                info!("Partition not initialized, generating keys");
                esp!(unsafe {
                    nvs_flash_generate_keys(keys_partition_ptr, &mut config as *mut _)
                })?;
            }
            other => esp!(other)?,
        }

        esp!(unsafe {
            nvs_flash_secure_init_partition(c_partition.as_ptr(), &mut config as *mut _)
        })?;

        registrations.insert(c_partition.clone());

        Ok(Self(c_partition))
    }
}

// These functions are copied from NvsCustom, maybe there's a way to write this in a shorter way?
impl Drop for NvsEncrypted {
    fn drop(&mut self) {
        {
            let mut registrations = NONDEFAULT_LOCKED.lock();

            esp!(unsafe { nvs_flash_deinit_partition(self.0.as_ptr()) }).unwrap();
            registrations.remove(self.0.as_ref());
        }

        info!("NvsEncrypted dropped");
    }
}

impl NvsPartitionId for NvsEncrypted {
    fn name(&self) -> &CStr {
        self.0.as_c_str()
    }
}

#[derive(Debug)]
pub struct EspNvsPartition<T: NvsPartitionId>(Arc<T>);

impl EspNvsPartition<NvsDefault> {
    /// Take the default NVS partition, initializing it if full or if a new version is detected
    pub fn take() -> Result<Self, EspError> {
        Self::take_with(true)
    }

    /// Take the default NVS partition
    ///
    /// # Arguments
    /// - `reinit`: Whether to reinitialize the partition if full or if a new version is detected
    pub fn take_with(reinit: bool) -> Result<Self, EspError> {
        Ok(Self(Arc::new(NvsDefault::new(reinit)?)))
    }
}

impl EspNvsPartition<NvsCustom> {
    pub fn take(partition: &str) -> Result<Self, EspError> {
        Ok(Self(Arc::new(NvsCustom::new(partition)?)))
    }
}

impl EspNvsPartition<NvsEncrypted> {
    pub fn take(partition: &str, keys_partition: Option<&str>) -> Result<Self, EspError> {
        Ok(Self(Arc::new(NvsEncrypted::new(
            partition,
            keys_partition,
        )?)))
    }
}

impl<T> Clone for EspNvsPartition<T>
where
    T: NvsPartitionId,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl RawHandle for EspNvsPartition<NvsCustom> {
    type Handle = *const u8;

    fn handle(&self) -> Self::Handle {
        self.0.name().as_ptr() as *const _
    }
}

impl RawHandle for EspNvsPartition<NvsEncrypted> {
    type Handle = *const u8;

    fn handle(&self) -> Self::Handle {
        self.0.name().as_ptr() as *const _
    }
}

pub type EspDefaultNvs = EspNvs<NvsDefault>;
pub type EspCustomNvs = EspNvs<NvsCustom>;
pub type EspEncryptedNvs = EspNvs<NvsEncrypted>;

#[allow(dead_code)]
pub struct EspNvs<T: NvsPartitionId>(EspNvsPartition<T>, nvs_handle_t);

impl<T: NvsPartitionId> EspNvs<T> {
    pub fn new(
        partition: EspNvsPartition<T>,
        namespace: &str,
        read_write: bool,
    ) -> Result<Self, EspError> {
        let c_namespace = to_cstring_arg(namespace)?;

        let mut handle: nvs_handle_t = 0;

        if partition.0.is_default() {
            esp!(unsafe {
                nvs_open(
                    c_namespace.as_ptr(),
                    if read_write {
                        nvs_open_mode_t_NVS_READWRITE
                    } else {
                        nvs_open_mode_t_NVS_READONLY
                    },
                    &mut handle as *mut _,
                )
            })?;
        } else {
            esp!(unsafe {
                nvs_open_from_partition(
                    partition.0.name().as_ptr(),
                    c_namespace.as_ptr(),
                    if read_write {
                        nvs_open_mode_t_NVS_READWRITE
                    } else {
                        nvs_open_mode_t_NVS_READONLY
                    },
                    &mut handle as *mut _,
                )
            })?;
        }

        Ok(Self(partition, handle))
    }

    #[cfg(all(
        not(esp_idf_version_major = "4"),
        not(all(esp_idf_version_major = "5", esp_idf_version_minor = "1"))
    ))]
    pub fn find_key(&self, name: &str) -> Result<Option<NvsDataType>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut entry_type: nvs_type_t = nvs_type_t_NVS_TYPE_ANY;

        let result = unsafe { nvs_find_key(self.1, c_key.as_ptr(), &mut entry_type as *mut _) };

        match result {
            ESP_OK => Ok(Some(NvsDataType::from(entry_type))),
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                esp!(err)?;
                Ok(None)
            }
        }
    }

    pub fn remove(&self, name: &str) -> Result<bool, EspError> {
        let c_key = to_cstring_arg(name)?;

        // nvs_erase_key is not scoped by datatype
        let result = unsafe { nvs_erase_key(self.1, c_key.as_ptr()) };

        if result == ESP_ERR_NVS_NOT_FOUND {
            Ok(false)
        } else {
            esp!(result)?;
            esp!(unsafe { nvs_commit(self.1) })?;

            Ok(true)
        }
    }

    pub fn blob_len(&self, name: &str) -> Result<Option<usize>, EspError> {
        let c_key = to_cstring_arg(name)?;

        #[allow(unused_assignments)]
        let mut len = 0;

        match unsafe { nvs_get_blob(self.1, c_key.as_ptr(), ptr::null_mut(), &mut len as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(len))
            }
        }
    }

    pub fn get_blob<'a>(
        &self,
        name: &str,
        buf: &'a mut [u8],
    ) -> Result<Option<&'a [u8]>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut len = buf.len();

        match unsafe {
            nvs_get_blob(
                self.1,
                c_key.as_ptr(),
                buf.as_mut_ptr() as *mut _,
                &mut len as *mut _,
            )
        } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(&buf[..len]))
            }
        }
    }

    pub fn set_blob(&self, name: &str, buf: &[u8]) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        // start by just clearing this key
        unsafe { nvs_erase_key(self.1, c_key.as_ptr()) };

        esp!(unsafe { nvs_set_blob(self.1, c_key.as_ptr(), buf.as_ptr().cast(), buf.len()) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn str_len(&self, name: &str) -> Result<Option<usize>, EspError> {
        let c_key = to_cstring_arg(name)?;

        #[allow(unused_assignments)]
        let mut len = 0;

        match unsafe { nvs_get_str(self.1, c_key.as_ptr(), ptr::null_mut(), &mut len as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(len))
            }
        }
    }

    pub fn get_str<'a>(&self, name: &str, buf: &'a mut [u8]) -> Result<Option<&'a str>, EspError> {
        let c_key = to_cstring_arg(name)?;

        let mut len = buf.len();
        match unsafe {
            nvs_get_str(
                self.1,
                c_key.as_ptr(),
                buf.as_mut_ptr() as *mut _,
                &mut len as *mut _,
            )
        } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(unsafe {
                    core::str::from_utf8_unchecked(&(buf[..len - 1]))
                }))
            }
        }
    }

    pub fn set_str(&self, name: &str, val: &str) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;
        let c_val = to_cstring_arg(val)?;

        // start by just clearing this key
        unsafe { nvs_erase_key(self.1, c_key.as_ptr()) };

        esp!(unsafe { nvs_set_str(self.1, c_key.as_ptr(), c_val.as_ptr(),) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_u8(&self, name: &str) -> Result<Option<u8>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [u8; 1] = [0; 1];

        match unsafe { nvs_get_u8(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_u8(&self, name: &str, val: u8) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_u8(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_i8(&self, name: &str) -> Result<Option<i8>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [i8; 1] = [0; 1];

        match unsafe { nvs_get_i8(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_i8(&self, name: &str, val: i8) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_i8(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_u16(&self, name: &str) -> Result<Option<u16>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [u16; 1] = [0; 1];

        match unsafe { nvs_get_u16(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_u16(&self, name: &str, val: u16) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_u16(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_i16(&self, name: &str) -> Result<Option<i16>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [i16; 1] = [0; 1];

        match unsafe { nvs_get_i16(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_i16(&self, name: &str, val: i16) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_i16(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_u32(&self, name: &str) -> Result<Option<u32>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [u32; 1] = [0; 1];

        match unsafe { nvs_get_u32(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_u32(&self, name: &str, val: u32) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_u32(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_i32(&self, name: &str) -> Result<Option<i32>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [i32; 1] = [0; 1];

        match unsafe { nvs_get_i32(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_i32(&self, name: &str, val: i32) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_i32(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_u64(&self, name: &str) -> Result<Option<u64>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [u64; 1] = [0; 1];

        match unsafe { nvs_get_u64(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_u64(&self, name: &str, val: u64) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_u64(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }

    pub fn get_i64(&self, name: &str) -> Result<Option<i64>, EspError> {
        let c_key = to_cstring_arg(name)?;
        let mut result: [i64; 1] = [0; 1];

        match unsafe { nvs_get_i64(self.1, c_key.as_ptr(), &mut result[0] as *mut _) } {
            ESP_ERR_NVS_NOT_FOUND => Ok(None),
            err => {
                // bail on error
                esp!(err)?;

                Ok(Some(result[0]))
            }
        }
    }

    pub fn set_i64(&self, name: &str, val: i64) -> Result<(), EspError> {
        let c_key = to_cstring_arg(name)?;

        esp!(unsafe { nvs_set_i64(self.1, c_key.as_ptr(), val) })?;

        esp!(unsafe { nvs_commit(self.1) })?;

        Ok(())
    }
}

impl<T: NvsPartitionId> Drop for EspNvs<T> {
    fn drop(&mut self) {
        unsafe {
            nvs_close(self.1);
        }

        info!("EspNvs dropped");
    }
}

unsafe impl<T: NvsPartitionId> Send for EspNvs<T> {}

impl RawHandle for EspNvs<NvsCustom> {
    type Handle = nvs_handle_t;

    fn handle(&self) -> Self::Handle {
        self.1
    }
}
impl RawHandle for EspNvs<NvsEncrypted> {
    type Handle = nvs_handle_t;

    fn handle(&self) -> Self::Handle {
        self.1
    }
}

/// A specialized key-value storage wrapper around `EspNvs` that provides a simplified interface
/// for storing and retrieving arbitrary data as byte arrays.
///
/// This struct was introduced to solve issue [#585](https://github.com/esp-rs/esp-idf-svc/issues/585)
/// where the `contains()` method in `EspNvs` incorrectly returned `false` for string values
/// that actually existed in NVS partitions. The root cause was that `EspNvs` implements
/// two different storage strategies:
///
/// 1. **Native ESP-IDF NVS API**: Direct access to ESP-IDF's native types (u8, u16, u32, u64, i8, i16, i32, i64, str, blob)
/// 2. **Serialized storage**: Everything stored as either u64 (≤7 bytes) or blob (>7 bytes) for compatibility with serde
///
/// `EspKeyValueStorage` focuses on the second approach, providing a clean interface for:
/// - Storing any data that can be represented as `&[u8]`
/// - Automatic optimization: values ≤7 bytes stored as u64, larger values as blobs
/// - Consistent `contains()` method that works correctly with this storage strategy
/// - Full compatibility with Rust serde implementations (postcard, json, etc.)
///
/// ## Usage
///
/// ```rust,no_run
/// use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspKeyValueStorage};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let partition = EspDefaultNvsPartition::take()?;
/// let storage = EspKeyValueStorage::new(partition, "my_namespace", true)?;
///
/// // Store data as bytes
/// let data = b"hello world";
/// storage.set_raw("my_key", data)?;
///
/// // Check if key exists (this works correctly, unlike the original EspNvs bug)
/// assert!(storage.contains("my_key")?);
///
/// // Retrieve data
/// let mut buffer = [0u8; 64];
/// if let Some(retrieved) = storage.get_raw("my_key", &mut buffer)? {
///     assert_eq!(retrieved, data);
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## Performance Characteristics
///
/// - **Small values (≤7 bytes)**: Stored as u64 for efficiency
/// - **Large values (>7 bytes)**: Stored as ESP-IDF blobs
/// - **Memory efficient**: No unnecessary allocations for small values
/// - **Flash efficient**: Optimized storage format reduces wear on flash memory
pub struct EspKeyValueStorage<T: NvsPartitionId>(EspNvs<T>);

impl<T: NvsPartitionId> EspKeyValueStorage<T> {
    pub const fn new(nvs: EspNvs<T>) -> Self {
        Self(nvs)
    }

    pub const fn esp_nvs(&self) -> &EspNvs<T> {
        &self.0
    }

    pub fn partition(&self) -> &EspNvsPartition<T> {
        &self.0 .0
    }

    pub fn contains(&self, name: &str) -> Result<bool, EspError> {
        self.0.find_key(name)?.map_or(Ok(false), |data_type| {
            Ok(matches!(data_type, NvsDataType::Blob | NvsDataType::U64))
        })
    }

    pub fn remove(&mut self, name: &str) -> Result<bool, EspError> {
        self.0.remove(name)
    }

    fn len(&self, name: &str) -> Result<Option<usize>, EspError> {
        if let Some(data_type) = self.0.find_key(name)? {
            return match data_type {
                NvsDataType::Blob => self.0.blob_len(name),
                NvsDataType::U64 => {
                    let value = self.0.get_u64(name)?;
                    if value.is_none() {
                        return Ok(None);
                    }
                    let value = value.unwrap();
                    // u64 value was found, decode it by getting the first byte as length
                    let len: u8 = (value & 0xff) as u8;

                    Ok(Some(len as _))
                }
                _ => Ok(None), // Other types are not supported in this storage
            };
        }
        Ok(None)
    }

    pub fn get_raw<'a>(&self, name: &str, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, EspError> {
        if let Some(data_type) = self.0.find_key(name)? {
            return match data_type {
                NvsDataType::Blob => self.0.get_blob(name, buf),
                NvsDataType::U64 => {
                    if let Some(mut u64value) = self.0.get_u64(name)? {
                        // The first byte is the length
                        let len = (u64value & 0xff) as u8;

                        if buf.len() < len as _ {
                            return Err(EspError::from_infallible::<ESP_ERR_NVS_INVALID_LENGTH>());
                        }

                        u64value >>= 8;
                        let array: [u8; 7] = [
                            (u64value & 0xff) as u8,
                            ((u64value >> 8) & 0xff) as u8,
                            ((u64value >> 16) & 0xff) as u8,
                            ((u64value >> 24) & 0xff) as u8,
                            ((u64value >> 32) & 0xff) as u8,
                            ((u64value >> 40) & 0xff) as u8,
                            ((u64value >> 48) & 0xff) as u8,
                        ];

                        buf[..len as usize].copy_from_slice(&array[..len as usize]);

                        Ok(Some(&buf[..len as usize]))
                    } else {
                        Ok(None)
                    }
                }
                _ => Ok(None), // Other types are not supported in this storage
            };
        }
        Ok(None)
    }

    pub fn set_raw(&self, name: &str, buf: &[u8]) -> Result<bool, EspError> {
        // start by just clearing this key, ignoring the result since it may not exist
        _ = self.0.remove(name);

        if buf.len() < 8 {
            // If the buffer is less than 8 bytes, store it as a u64
            let mut u64value: u_int64_t = 0;

            // Set the first byte as the length
            u64value |= buf.len() as u_int64_t;

            // Fill the rest of the u64 with the buffer values
            for v in buf.iter().rev() {
                u64value <<= 8;
                u64value |= *v as u_int64_t;
            }

            u64value <<= 8;
            u64value |= buf.len() as u_int64_t;
            self.0.set_u64(name, u64value)?;
        } else {
            self.0.set_blob(name, buf)?;
        }
        Ok(true)
    }
}

impl<T: NvsPartitionId> StorageBase for EspKeyValueStorage<T> {
    type Error = EspError;

    fn contains(&self, name: &str) -> Result<bool, Self::Error> {
        EspKeyValueStorage::contains(self, name)
    }

    fn remove(&mut self, name: &str) -> Result<bool, Self::Error> {
        EspKeyValueStorage::remove(self, name)
    }
}

impl<T: NvsPartitionId> RawStorage for EspKeyValueStorage<T> {
    fn len(&self, name: &str) -> Result<Option<usize>, Self::Error> {
        EspKeyValueStorage::len(self, name)
    }

    fn get_raw<'a>(&self, name: &str, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Self::Error> {
        EspKeyValueStorage::get_raw(self, name, buf)
    }

    fn set_raw(&mut self, name: &str, buf: &[u8]) -> Result<bool, Self::Error> {
        EspKeyValueStorage::set_raw(self, name, buf)
    }
}
