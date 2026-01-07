//! Safe wrapper around mdbx-rs FFI layer.
//!
//! This module provides a safe Rust API for MDBX database operations,
//! wrapping the unsafe FFI calls from mdbx-rs. All unsafe code is contained
//! within this module, exposing only safe interfaces to the rest of the crate.
//!
//! # Safety
//!
//! The wrapper ensures:
//! - RAII patterns for automatic resource cleanup (Drop implementations)
//! - Proper lifetime management (transactions borrow environment)
//! - Data is copied out of MDBX before transaction ends (no dangling pointers)
//! - Error codes are properly mapped to Result types

#![allow(dead_code)]

use std::ffi::{c_void, CStr, CString};
use std::marker::PhantomData;
use std::path::Path;
use std::ptr;
use std::rc::Rc;

use mdbx_rs::{
    mdbx_cursor_close, mdbx_cursor_get, mdbx_cursor_open, mdbx_dbi_open, mdbx_del, mdbx_env_close,
    mdbx_env_create, mdbx_env_open, mdbx_env_set_geometry, mdbx_env_set_maxdbs, mdbx_env_sync_ex,
    mdbx_get, mdbx_put, mdbx_strerror, mdbx_txn_abort, mdbx_txn_begin, mdbx_txn_commit,
    MDBX_cursor, MDBX_cursor_op, MDBX_dbi, MDBX_env, MDBX_txn, MDBX_val, MDBX_CREATE,
    MDBX_NOTFOUND, MDBX_RDONLY, MDBX_SUCCESS,
};

/// Error type for MDBX operations.
#[derive(Debug, Clone)]
pub struct Error {
    code: i32,
    message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MDBX error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

impl Error {
    fn from_code(code: i32) -> Self {
        // Use mdbx_strerror to get the error message from MDBX.
        // SAFETY: mdbx_strerror returns a pointer to a static string that is always valid.
        let message = unsafe {
            let msg_ptr = mdbx_strerror(code);
            if msg_ptr.is_null() {
                format!("Unknown error (code {})", code)
            } else {
                CStr::from_ptr(msg_ptr).to_string_lossy().into_owned()
            }
        };
        Self { code, message }
    }

    /// Returns true if this error represents "not found".
    pub fn is_not_found(&self) -> bool {
        self.code == MDBX_NOTFOUND
    }
}

/// Result type for MDBX operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Check MDBX return code and convert to Result.
fn check_rc(rc: i32) -> Result<()> {
    if rc == MDBX_SUCCESS {
        Ok(())
    } else {
        Err(Error::from_code(rc))
    }
}

/// Geometry configuration for the database.
#[derive(Debug, Clone)]
pub struct Geometry {
    /// Lower bound of database size in bytes.
    pub size_lower: i64,
    /// Current database size in bytes.
    pub size_now: i64,
    /// Upper bound of database size in bytes.
    pub size_upper: i64,
    /// Growth step in bytes (-1 for auto).
    pub growth_step: i64,
    /// Shrink threshold in bytes (-1 for auto).
    pub shrink_threshold: i64,
    /// Page size in bytes.
    pub page_size: i64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            size_lower: -1,
            size_now: 4096 * 256,
            size_upper: 1024 * 1024 * 1024 * 1024, // 1TB
            growth_step: -1,
            shrink_threshold: -1,
            page_size: 4096,
        }
    }
}

/// Database flags for opening/creating databases.
#[derive(Debug, Clone, Copy, Default)]
pub struct DatabaseFlags(u32);

impl DatabaseFlags {
    /// Create the database if it doesn't exist.
    pub const CREATE: Self = Self(MDBX_CREATE as u32);
    /// No flags.
    pub const DEFAULT: Self = Self(0);

    /// Get the raw bits value.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Write flags for put operations.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteFlags(u32);

impl WriteFlags {
    /// Default write flags.
    pub const DEFAULT: Self = Self(0);

    /// Get the raw bits value.
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// MDBX Environment wrapper.
///
/// Manages the database environment lifecycle. When dropped, the environment
/// is properly closed.
pub struct Environment {
    env: *mut MDBX_env,
}

// SAFETY: The MDBX environment handle is thread-safe for concurrent access.
//
// From libmdbx documentation (https://libmdbx.dqdkfa.ru/intro.html):
// "The library is fully thread-aware and supports concurrent read/write access
// from multiple processes and threads."
//
// Concurrent usage must be through separate transactions per thread.
// Transactions and cursors are NOT thread-safe and must not be moved between
// threads (they are `!Send` and `!Sync`).
//
// Dropping/closing the environment requires no live transactions, which Rust's
// borrow checker enforces through the lifetime parameter on transactions.
unsafe impl Send for Environment {}
unsafe impl Sync for Environment {}

impl Environment {
    /// Create a new environment builder.
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::new()
    }

    /// Sync the environment to disk.
    ///
    /// # Arguments
    /// * `force` - If true, force a synchronous flush.
    pub fn sync(&self, force: bool) -> Result<()> {
        // SAFETY: env pointer is valid for the lifetime of Environment.
        // mdbx_env_sync_ex is safe to call with valid env pointer.
        let rc = unsafe { mdbx_env_sync_ex(self.env, force, false) };
        check_rc(rc)
    }

    /// Begin a read-only transaction.
    pub fn begin_ro_txn(&self) -> Result<RoTransaction<'_>> {
        let mut txn: *mut MDBX_txn = ptr::null_mut();
        // SAFETY: env pointer is valid, txn is a valid output pointer.
        // MDBX_RDONLY flag creates a read-only transaction.
        let rc = unsafe { mdbx_txn_begin(self.env, ptr::null_mut(), MDBX_RDONLY as u32, &mut txn) };
        check_rc(rc)?;
        Ok(RoTransaction {
            txn,
            _marker: PhantomData,
            _not_send: PhantomData,
        })
    }

    /// Begin a read-write transaction.
    pub fn begin_rw_txn(&self) -> Result<RwTransaction<'_>> {
        let mut txn: *mut MDBX_txn = ptr::null_mut();
        // SAFETY: env pointer is valid, txn is a valid output pointer.
        // flags=0 creates a read-write transaction.
        let rc = unsafe { mdbx_txn_begin(self.env, ptr::null_mut(), 0, &mut txn) };
        check_rc(rc)?;
        Ok(RwTransaction {
            txn,
            committed: false,
            _marker: PhantomData,
            _not_send: PhantomData,
        })
    }
}

impl Drop for Environment {
    fn drop(&mut self) {
        if !self.env.is_null() {
            // SAFETY: env pointer is valid and we own it.
            // After this call, the pointer is invalid.
            unsafe {
                mdbx_env_close(self.env);
            }
        }
    }
}

/// Builder for creating an Environment.
pub struct EnvironmentBuilder {
    max_dbs: u32,
    geometry: Geometry,
}

impl EnvironmentBuilder {
    fn new() -> Self {
        Self {
            max_dbs: 10,
            geometry: Geometry::default(),
        }
    }

    /// Set the maximum number of named databases.
    pub fn set_max_dbs(mut self, max_dbs: u32) -> Self {
        self.max_dbs = max_dbs;
        self
    }

    /// Set the database geometry.
    pub fn set_geometry(mut self, geometry: Geometry) -> Self {
        self.geometry = geometry;
        self
    }

    /// Open the environment at the given path.
    pub fn open(self, path: &Path) -> Result<Environment> {
        let mut env: *mut MDBX_env = ptr::null_mut();

        // SAFETY: env is a valid output pointer.
        let rc = unsafe { mdbx_env_create(&mut env) };
        check_rc(rc)?;

        // SAFETY: env pointer is now valid from mdbx_env_create.
        let rc = unsafe { mdbx_env_set_maxdbs(env, self.max_dbs) };
        if rc != MDBX_SUCCESS {
            unsafe { mdbx_env_close(env) };
            return Err(Error::from_code(rc));
        }

        // SAFETY: env pointer is valid, geometry values are within valid ranges.
        let rc = unsafe {
            mdbx_env_set_geometry(
                env,
                self.geometry.size_lower as isize,
                self.geometry.size_now as isize,
                self.geometry.size_upper as isize,
                self.geometry.growth_step as isize,
                self.geometry.shrink_threshold as isize,
                self.geometry.page_size as isize,
            )
        };
        if rc != MDBX_SUCCESS {
            unsafe { mdbx_env_close(env) };
            return Err(Error::from_code(rc));
        }

        let path_str = path.to_str().ok_or_else(|| Error {
            code: -22,
            message: "Invalid path: not valid UTF-8".to_string(),
        })?;
        let path_cstr = CString::new(path_str).map_err(|_| Error {
            code: -22,
            message: "Invalid path: contains null byte".to_string(),
        })?;

        // SAFETY: env pointer is valid, path_cstr is a valid C string.
        // Mode 0o644 is standard file permissions.
        let rc = unsafe { mdbx_env_open(env, path_cstr.as_ptr(), 0, 0o644) };
        if rc != MDBX_SUCCESS {
            unsafe { mdbx_env_close(env) };
            return Err(Error::from_code(rc));
        }

        Ok(Environment { env })
    }
}

impl Default for EnvironmentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Database handle.
#[derive(Debug, Clone, Copy)]
pub struct Database {
    dbi: MDBX_dbi,
}

impl Database {
    /// Get the raw DBI handle.
    pub fn dbi(&self) -> MDBX_dbi {
        self.dbi
    }
}

/// Read-only transaction.
///
/// Provides read access to the database. Automatically aborted on drop.
///
/// # Thread Safety
///
/// Transactions are NOT thread-safe and must not be moved between threads.
/// This is enforced by the `_not_send` marker which makes this type `!Send` and `!Sync`.
pub struct RoTransaction<'env> {
    txn: *mut MDBX_txn,
    _marker: PhantomData<&'env Environment>,
    /// Marker to make this type `!Send` and `!Sync`.
    /// MDBX transactions are thread-local and must not be moved between threads.
    _not_send: PhantomData<Rc<()>>,
}

impl<'env> RoTransaction<'env> {
    /// Open a database by name.
    ///
    /// # Arguments
    /// * `name` - Database name, or None for the default database.
    pub fn open_db(&self, name: Option<&str>) -> Result<Database> {
        let mut dbi: MDBX_dbi = 0;
        let name_cstr = name.map(CString::new).transpose().map_err(|_| Error {
            code: -22,
            message: "Invalid database name: contains null byte".to_string(),
        })?;
        let name_ptr = name_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());

        // SAFETY: txn pointer is valid, name_ptr is either null or valid C string.
        let rc = unsafe { mdbx_dbi_open(self.txn, name_ptr, 0, &mut dbi) };
        check_rc(rc)?;
        Ok(Database { dbi })
    }

    /// Get a value by key.
    ///
    /// Returns None if the key is not found.
    pub fn get<V: FromMdbxValue>(&self, db: Database, key: &[u8]) -> Result<Option<V>> {
        let key_val = MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val = MDBX_val::default();

        // SAFETY: txn and dbi are valid, key_val points to valid data.
        let rc = unsafe { mdbx_get(self.txn, db.dbi, &key_val, &mut data_val) };

        if rc == MDBX_SUCCESS {
            // SAFETY: data_val points to valid data within the transaction.
            // We copy it immediately before the transaction ends.
            let value = unsafe { V::from_mdbx_val(&data_val) };
            Ok(Some(value))
        } else if rc == MDBX_NOTFOUND {
            Ok(None)
        } else {
            Err(Error::from_code(rc))
        }
    }

    /// Create a cursor for iterating over the database.
    pub fn cursor(&self, db: &Database) -> Result<Cursor<'_>> {
        let mut cursor: *mut MDBX_cursor = ptr::null_mut();
        // SAFETY: txn and dbi are valid, cursor is a valid output pointer.
        let rc = unsafe { mdbx_cursor_open(self.txn, db.dbi, &mut cursor) };
        check_rc(rc)?;
        Ok(Cursor {
            cursor,
            _marker: PhantomData,
            _not_send: PhantomData,
        })
    }
}

impl Drop for RoTransaction<'_> {
    fn drop(&mut self) {
        if !self.txn.is_null() {
            // SAFETY: txn pointer is valid and we own it.
            unsafe {
                mdbx_txn_abort(self.txn);
            }
        }
    }
}

/// Read-write transaction.
///
/// Provides read and write access to the database. Must be explicitly committed,
/// otherwise it is aborted on drop.
///
/// # Thread Safety
///
/// Transactions are NOT thread-safe and must not be moved between threads.
/// This is enforced by the `_not_send` marker which makes this type `!Send` and `!Sync`.
pub struct RwTransaction<'env> {
    txn: *mut MDBX_txn,
    committed: bool,
    _marker: PhantomData<&'env Environment>,
    /// Marker to make this type `!Send` and `!Sync`.
    /// MDBX transactions are thread-local and must not be moved between threads.
    _not_send: PhantomData<Rc<()>>,
}

impl<'env> RwTransaction<'env> {
    /// Open a database by name.
    pub fn open_db(&self, name: Option<&str>) -> Result<Database> {
        let mut dbi: MDBX_dbi = 0;
        let name_cstr = name.map(CString::new).transpose().map_err(|_| Error {
            code: -22,
            message: "Invalid database name: contains null byte".to_string(),
        })?;
        let name_ptr = name_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());

        // SAFETY: txn pointer is valid, name_ptr is either null or valid C string.
        let rc = unsafe { mdbx_dbi_open(self.txn, name_ptr, 0, &mut dbi) };
        check_rc(rc)?;
        Ok(Database { dbi })
    }

    /// Create a database if it doesn't exist.
    ///
    /// The passed flags are combined with MDBX_CREATE to ensure the database
    /// is created if it doesn't exist.
    pub fn create_db(&self, name: Option<&str>, flags: DatabaseFlags) -> Result<Database> {
        let mut dbi: MDBX_dbi = 0;
        let name_cstr = name.map(CString::new).transpose().map_err(|_| Error {
            code: -22,
            message: "Invalid database name: contains null byte".to_string(),
        })?;
        let name_ptr = name_cstr
            .as_ref()
            .map(|c| c.as_ptr())
            .unwrap_or(ptr::null());

        // Combine user-provided flags with MDBX_CREATE to ensure database creation.
        let combined_flags = MDBX_CREATE as u32 | flags.bits();

        // SAFETY: txn pointer is valid, name_ptr is either null or valid C string.
        // MDBX_CREATE flag ensures the database is created if it doesn't exist.
        let rc = unsafe { mdbx_dbi_open(self.txn, name_ptr, combined_flags, &mut dbi) };
        check_rc(rc)?;
        Ok(Database { dbi })
    }

    /// Get a value by key.
    pub fn get<V: FromMdbxValue>(&self, db: Database, key: &[u8]) -> Result<Option<V>> {
        let key_val = MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val = MDBX_val::default();

        // SAFETY: txn and dbi are valid, key_val points to valid data.
        let rc = unsafe { mdbx_get(self.txn, db.dbi, &key_val, &mut data_val) };

        if rc == MDBX_SUCCESS {
            // SAFETY: data_val points to valid data within the transaction.
            let value = unsafe { V::from_mdbx_val(&data_val) };
            Ok(Some(value))
        } else if rc == MDBX_NOTFOUND {
            Ok(None)
        } else {
            Err(Error::from_code(rc))
        }
    }

    /// Put a key-value pair.
    pub fn put(&self, db: Database, key: &[u8], value: &[u8], flags: WriteFlags) -> Result<()> {
        let key_val = MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val = MDBX_val {
            iov_len: value.len(),
            iov_base: value.as_ptr() as *mut c_void,
        };

        // SAFETY: txn and dbi are valid, key_val and data_val point to valid data.
        let rc = unsafe { mdbx_put(self.txn, db.dbi, &key_val, &mut data_val, flags.bits()) };
        check_rc(rc)
    }

    /// Delete a key (and optionally a specific value).
    ///
    /// Returns Ok(()) even if the key was not found.
    pub fn del(&self, db: Database, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let key_val = MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let data_ptr = value.map(|v| MDBX_val {
            iov_len: v.len(),
            iov_base: v.as_ptr() as *mut c_void,
        });

        // SAFETY: txn and dbi are valid, key_val points to valid data.
        let rc = unsafe {
            mdbx_del(
                self.txn,
                db.dbi,
                &key_val,
                data_ptr
                    .as_ref()
                    .map(|p| p as *const _)
                    .unwrap_or(ptr::null()),
            )
        };

        if rc == MDBX_SUCCESS || rc == MDBX_NOTFOUND {
            Ok(())
        } else {
            Err(Error::from_code(rc))
        }
    }

    /// Create a cursor for iterating over the database.
    pub fn cursor(&self, db: &Database) -> Result<Cursor<'_>> {
        let mut cursor: *mut MDBX_cursor = ptr::null_mut();
        // SAFETY: txn and dbi are valid, cursor is a valid output pointer.
        let rc = unsafe { mdbx_cursor_open(self.txn, db.dbi, &mut cursor) };
        check_rc(rc)?;
        Ok(Cursor {
            cursor,
            _marker: PhantomData,
            _not_send: PhantomData,
        })
    }

    /// Commit the transaction.
    pub fn commit(mut self) -> Result<()> {
        // SAFETY: txn pointer is valid and we own it.
        let rc = unsafe { mdbx_txn_commit(self.txn) };
        self.committed = true;
        check_rc(rc)
    }
}

impl Drop for RwTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed && !self.txn.is_null() {
            // SAFETY: txn pointer is valid and we own it.
            unsafe {
                mdbx_txn_abort(self.txn);
            }
        }
    }
}

/// Cursor for iterating over database entries.
///
/// # Thread Safety
///
/// Cursors are NOT thread-safe and must not be moved between threads.
/// This is enforced by the `_not_send` marker which makes this type `!Send` and `!Sync`.
pub struct Cursor<'txn> {
    cursor: *mut MDBX_cursor,
    _marker: PhantomData<&'txn ()>,
    /// Marker to make this type `!Send` and `!Sync`.
    /// MDBX cursors are thread-local and must not be moved between threads.
    _not_send: PhantomData<Rc<()>>,
}

impl<'txn> Cursor<'txn> {
    /// Move to the next entry and return the key-value pair.
    ///
    /// Returns None when there are no more entries.
    pub fn next<K: FromMdbxValue, V: FromMdbxValue>(&mut self) -> Result<Option<(K, V)>> {
        let mut key_val = MDBX_val::default();
        let mut data_val = MDBX_val::default();

        // SAFETY: cursor is valid, key_val and data_val are valid output pointers.
        let rc = unsafe {
            mdbx_cursor_get(
                self.cursor,
                &mut key_val,
                &mut data_val,
                MDBX_cursor_op::MDBX_NEXT,
            )
        };

        if rc == MDBX_SUCCESS {
            // SAFETY: key_val and data_val point to valid data within the cursor's transaction.
            let key = unsafe { K::from_mdbx_val(&key_val) };
            let value = unsafe { V::from_mdbx_val(&data_val) };
            Ok(Some((key, value)))
        } else if rc == MDBX_NOTFOUND {
            Ok(None)
        } else {
            Err(Error::from_code(rc))
        }
    }
}

impl Drop for Cursor<'_> {
    fn drop(&mut self) {
        if !self.cursor.is_null() {
            // SAFETY: cursor pointer is valid and we own it.
            unsafe {
                mdbx_cursor_close(self.cursor);
            }
        }
    }
}

/// Trait for types that can be constructed from MDBX_val.
///
/// # Safety
///
/// Implementations must copy data from the MDBX_val immediately,
/// as the pointer becomes invalid after the transaction ends.
pub trait FromMdbxValue {
    /// Create a value from an MDBX_val.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `val` points to valid memory.
    unsafe fn from_mdbx_val(val: &MDBX_val) -> Self;
}

impl FromMdbxValue for Vec<u8> {
    unsafe fn from_mdbx_val(val: &MDBX_val) -> Self {
        if val.iov_base.is_null() || val.iov_len == 0 {
            Vec::new()
        } else {
            // SAFETY: Caller guarantees val points to valid memory.
            // We copy the data immediately.
            std::slice::from_raw_parts(val.iov_base as *const u8, val.iov_len).to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_env() -> (TempDir, Environment) {
        let dir = TempDir::new().expect("Failed to create temp dir");
        let env = Environment::builder()
            .set_max_dbs(10)
            .open(dir.path())
            .expect("Failed to open environment");
        (dir, env)
    }

    #[test]
    fn test_open_environment() {
        let (_dir, _env) = create_test_env();
    }

    #[test]
    fn test_create_database() {
        let (_dir, env) = create_test_env();
        let txn = env.begin_rw_txn().expect("Failed to begin transaction");
        let _db = txn
            .create_db(Some("test"), DatabaseFlags::CREATE)
            .expect("Failed to create database");
        txn.commit().expect("Failed to commit");
    }

    #[test]
    fn test_put_get() {
        let (_dir, env) = create_test_env();

        {
            let txn = env.begin_rw_txn().expect("Failed to begin transaction");
            let db = txn
                .create_db(Some("test"), DatabaseFlags::CREATE)
                .expect("Failed to create database");
            txn.put(db, b"key1", b"value1", WriteFlags::DEFAULT)
                .expect("Failed to put");
            txn.commit().expect("Failed to commit");
        }

        {
            let txn = env.begin_ro_txn().expect("Failed to begin transaction");
            let db = txn.open_db(Some("test")).expect("Failed to open database");
            let value: Option<Vec<u8>> = txn.get(db, b"key1").expect("Failed to get");
            assert_eq!(value, Some(b"value1".to_vec()));
        }
    }

    #[test]
    fn test_delete() {
        let (_dir, env) = create_test_env();

        {
            let txn = env.begin_rw_txn().expect("Failed to begin transaction");
            let db = txn
                .create_db(Some("test"), DatabaseFlags::CREATE)
                .expect("Failed to create database");
            txn.put(db, b"key1", b"value1", WriteFlags::DEFAULT)
                .expect("Failed to put");
            txn.commit().expect("Failed to commit");
        }

        {
            let txn = env.begin_rw_txn().expect("Failed to begin transaction");
            let db = txn.open_db(Some("test")).expect("Failed to open database");
            txn.del(db, b"key1", None).expect("Failed to delete");
            txn.commit().expect("Failed to commit");
        }

        {
            let txn = env.begin_ro_txn().expect("Failed to begin transaction");
            let db = txn.open_db(Some("test")).expect("Failed to open database");
            let value: Option<Vec<u8>> = txn.get(db, b"key1").expect("Failed to get");
            assert_eq!(value, None);
        }
    }

    #[test]
    fn test_cursor_iteration() {
        let (_dir, env) = create_test_env();

        {
            let txn = env.begin_rw_txn().expect("Failed to begin transaction");
            let db = txn
                .create_db(Some("test"), DatabaseFlags::CREATE)
                .expect("Failed to create database");
            txn.put(db, b"aaa", b"111", WriteFlags::DEFAULT)
                .expect("Failed to put");
            txn.put(db, b"bbb", b"222", WriteFlags::DEFAULT)
                .expect("Failed to put");
            txn.put(db, b"ccc", b"333", WriteFlags::DEFAULT)
                .expect("Failed to put");
            txn.commit().expect("Failed to commit");
        }

        {
            let txn = env.begin_ro_txn().expect("Failed to begin transaction");
            let db = txn.open_db(Some("test")).expect("Failed to open database");
            let mut cursor = txn.cursor(&db).expect("Failed to create cursor");

            let mut entries = Vec::new();
            while let Some((key, value)) = cursor
                .next::<Vec<u8>, Vec<u8>>()
                .expect("Cursor iteration failed")
            {
                entries.push((key, value));
            }

            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].0, b"aaa".to_vec());
            assert_eq!(entries[1].0, b"bbb".to_vec());
            assert_eq!(entries[2].0, b"ccc".to_vec());
        }
    }
}
