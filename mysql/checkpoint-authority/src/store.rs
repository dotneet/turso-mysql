// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Descriptor-backed durable state for one local checkpoint authority.

use std::{
    error::Error,
    ffi::{CStr, CString},
    fmt,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use errno::{errno, set_errno, Errno};
use sha2::{Digest, Sha256};
use turso_mysql_server::AccountStoreCheckpoint;

use crate::protocol::{AuthorityId, CHECKPOINT_BYTES};

const FINAL_NAME: &[u8] = b".turso-mysql-checkpoint-v1";
const BINDING_NAME: &[u8] = b".turso-mysql-checkpoint-authority-v1";
const LOCK_NAME: &[u8] = b".turso-mysql-checkpoint.lock";
const TEMP_PREFIX: &[u8] = b".turso-mysql-checkpoint-v1.tmp.";
const BINDING_TEMP_PREFIX: &[u8] = b".turso-mysql-checkpoint-authority-v1.tmp.";
const FILE_MODE: u32 = 0o600;
const ROOT_MODE: u32 = 0o700;
const MAX_RECORD_BYTES: usize = 512;
const MAGIC: &[u8; 4] = b"TMCS";
const BINDING_MAGIC: &[u8; 4] = b"TMCA";
const VERSION: u8 = 1;
const PREFIX_BYTES: usize = 8;
const CHECKSUM_BYTES: usize = 32;
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// One durable checkpoint state root bound to one authority ID.
pub struct CheckpointStore {
    directory: File,
    authority: AuthorityId,
}

impl CheckpointStore {
    /// Opens an exact private root and removes valid stale temporary files.
    pub fn open(
        root: impl AsRef<Path>,
        authority: AuthorityId,
    ) -> Result<Self, CheckpointStoreError> {
        let directory = open_private_root(root.as_ref())?;
        let store = Self {
            directory,
            authority,
        };
        store.initialize_authority_binding()?;
        Ok(store)
    }

    /// Reads the current exact checkpoint.
    pub fn read(&self) -> Result<Option<AccountStoreCheckpoint>, CheckpointStoreError> {
        self.validate_authority_binding_unlocked()?;
        Ok(self.read_record_unlocked()?.map(|record| record.checkpoint))
    }

    /// Persists a replacement only if expected matches. A repeat of a durable
    /// replacement succeeds even when expected no longer matches.
    pub fn compare_and_persist(
        &self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> Result<CheckpointStoreCas, CheckpointStoreError> {
        let replacement_record = self.encode_record(replacement)?;
        let lock = self.acquire_lock()?;
        let result = self.compare_and_persist_locked(expected, replacement, &replacement_record);
        drop(lock);
        result
    }

    /// Compares and persists without waiting past `deadline` for the writer lock.
    ///
    /// Once this method owns the lock, it completes the durable operation rather
    /// than reporting cancellation after publication could have started.
    pub(crate) fn compare_and_persist_until(
        &self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
        deadline: Instant,
        cancelled: impl Fn() -> bool,
    ) -> Result<CheckpointStoreCas, CheckpointStoreCasUntilError> {
        let replacement_record = self
            .encode_record(replacement)
            .map_err(CheckpointStoreCasUntilError::Store)?;
        let lock = self.acquire_lock_until(deadline, &cancelled)?;
        let result = (|| {
            check_lock_wait(deadline, &cancelled)?;
            self.compare_and_persist_locked(expected, replacement, &replacement_record)
                .map_err(CheckpointStoreCasUntilError::Store)
        })();
        drop(lock);
        result
    }

    /// Returns the authority ID bound to this state root.
    pub fn authority(&self) -> &AuthorityId {
        &self.authority
    }

    fn compare_and_persist_locked(
        &self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
        replacement_record: &[u8],
    ) -> Result<CheckpointStoreCas, CheckpointStoreError> {
        self.validate_authority_binding_unlocked()?;
        let current = self.read_record_unlocked()?;
        if current
            .as_ref()
            .is_some_and(|record| record.checkpoint == *replacement)
        {
            return Ok(CheckpointStoreCas::Durable);
        }
        let matches_expected = match (current.as_ref(), expected) {
            (None, None) => true,
            (Some(record), Some(expected)) => record.checkpoint == *expected,
            _ => false,
        };
        if !matches_expected {
            return Ok(CheckpointStoreCas::Conflict);
        }
        let valid_replacement = match current.as_ref() {
            None => replacement.revision() == 0,
            Some(record) => {
                record.checkpoint.belongs_to_same_store(*replacement)
                    && record.checkpoint.revision().checked_add(1) == Some(replacement.revision())
            }
        };
        if !valid_replacement {
            return Ok(CheckpointStoreCas::Conflict);
        }
        self.publish_unlocked(FINAL_NAME, TEMP_PREFIX, replacement_record)?;
        Ok(CheckpointStoreCas::Durable)
    }

    fn initialize_authority_binding(&self) -> Result<(), CheckpointStoreError> {
        let lock = self.acquire_lock()?;
        let result = match self.read_authority_binding_unlocked()? {
            Some(authority) if authority == self.authority => {
                self.cleanup_temporary_files_unlocked()
            }
            Some(_) => Err(CheckpointStoreError::AuthorityMismatch),
            None => {
                self.read_record_unlocked()?;
                self.cleanup_temporary_files_unlocked()?;
                self.publish_authority_binding_unlocked()
            }
        };
        drop(lock);
        result
    }

    fn validate_authority_binding_unlocked(&self) -> Result<(), CheckpointStoreError> {
        match self.read_authority_binding_unlocked()? {
            Some(authority) if authority == self.authority => Ok(()),
            Some(_) => Err(CheckpointStoreError::AuthorityMismatch),
            None => Err(CheckpointStoreError::InvalidState),
        }
    }

    fn read_authority_binding_unlocked(&self) -> Result<Option<AuthorityId>, CheckpointStoreError> {
        let Some(bytes) = self.read_private_file_unlocked(BINDING_NAME)? else {
            return Ok(None);
        };
        self.decode_authority_binding(&bytes).map(Some)
    }

    fn publish_authority_binding_unlocked(&self) -> Result<(), CheckpointStoreError> {
        let binding = self.encode_authority_binding()?;
        self.publish_unlocked(BINDING_NAME, BINDING_TEMP_PREFIX, &binding)
    }

    fn read_record_unlocked(&self) -> Result<Option<StoredRecord>, CheckpointStoreError> {
        let Some(bytes) = self.read_private_file_unlocked(FINAL_NAME)? else {
            return Ok(None);
        };
        self.decode_record(&bytes).map(Some)
    }

    fn read_private_file_unlocked(
        &self,
        name: &[u8],
    ) -> Result<Option<Vec<u8>>, CheckpointStoreError> {
        let Some(mut file) = self.open_optional(name, libc::O_RDONLY)? else {
            return Ok(None);
        };
        let metadata = private_regular_metadata(&file)?;
        let length =
            usize::try_from(metadata.len()).map_err(|_| CheckpointStoreError::InvalidState)?;
        if length > MAX_RECORD_BYTES {
            return Err(CheckpointStoreError::InvalidState);
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|_| CheckpointStoreError::Unavailable)?;
        let mut extra = [0; 1];
        match file.read(&mut extra) {
            Ok(0) => {}
            Ok(_) => return Err(CheckpointStoreError::InvalidState),
            Err(_) => return Err(CheckpointStoreError::Unavailable),
        }
        Ok(Some(bytes))
    }

    fn encode_record(
        &self,
        checkpoint: &AccountStoreCheckpoint,
    ) -> Result<Vec<u8>, CheckpointStoreError> {
        let authority = self.authority.as_str().as_bytes();
        let authority_len: u16 = authority
            .len()
            .try_into()
            .map_err(|_| CheckpointStoreError::InvalidState)?;
        let mut record =
            Vec::with_capacity(PREFIX_BYTES + authority.len() + CHECKPOINT_BYTES + CHECKSUM_BYTES);
        record.extend_from_slice(MAGIC);
        record.push(VERSION);
        record.push(0);
        record.extend_from_slice(&authority_len.to_be_bytes());
        record.extend_from_slice(authority);
        record.extend_from_slice(&checkpoint.to_bytes());
        record.extend_from_slice(&Sha256::digest(&record));
        if record.len() > MAX_RECORD_BYTES {
            return Err(CheckpointStoreError::InvalidState);
        }
        Ok(record)
    }

    fn encode_authority_binding(&self) -> Result<Vec<u8>, CheckpointStoreError> {
        let authority = self.authority.as_str().as_bytes();
        let authority_len: u16 = authority
            .len()
            .try_into()
            .map_err(|_| CheckpointStoreError::InvalidState)?;
        let mut record = Vec::with_capacity(PREFIX_BYTES + authority.len() + CHECKSUM_BYTES);
        record.extend_from_slice(BINDING_MAGIC);
        record.push(VERSION);
        record.push(0);
        record.extend_from_slice(&authority_len.to_be_bytes());
        record.extend_from_slice(authority);
        record.extend_from_slice(&Sha256::digest(&record));
        if record.len() > MAX_RECORD_BYTES {
            return Err(CheckpointStoreError::InvalidState);
        }
        Ok(record)
    }

    fn decode_record(&self, bytes: &[u8]) -> Result<StoredRecord, CheckpointStoreError> {
        if bytes.len() < PREFIX_BYTES + CHECKPOINT_BYTES + CHECKSUM_BYTES
            || bytes.len() > MAX_RECORD_BYTES
            || bytes[..4] != *MAGIC
            || bytes[4] != VERSION
            || bytes[5] != 0
        {
            return Err(CheckpointStoreError::InvalidState);
        }
        let authority_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
        let checkpoint_start = PREFIX_BYTES
            .checked_add(authority_len)
            .ok_or(CheckpointStoreError::InvalidState)?;
        let checkpoint_end = checkpoint_start
            .checked_add(CHECKPOINT_BYTES)
            .ok_or(CheckpointStoreError::InvalidState)?;
        let checksum_end = checkpoint_end
            .checked_add(CHECKSUM_BYTES)
            .ok_or(CheckpointStoreError::InvalidState)?;
        if checksum_end != bytes.len()
            || Sha256::digest(&bytes[..checkpoint_end]).as_slice() != &bytes[checkpoint_end..]
        {
            return Err(CheckpointStoreError::InvalidState);
        }
        let authority = std::str::from_utf8(&bytes[PREFIX_BYTES..checkpoint_start])
            .map_err(|_| CheckpointStoreError::InvalidState)
            .and_then(|value| {
                AuthorityId::new(value.to_owned()).map_err(|_| CheckpointStoreError::InvalidState)
            })?;
        if authority != self.authority {
            return Err(CheckpointStoreError::AuthorityMismatch);
        }
        let checkpoint =
            AccountStoreCheckpoint::from_bytes(&bytes[checkpoint_start..checkpoint_end])
                .map_err(|_| CheckpointStoreError::InvalidState)?;
        Ok(StoredRecord { checkpoint })
    }

    fn decode_authority_binding(&self, bytes: &[u8]) -> Result<AuthorityId, CheckpointStoreError> {
        if bytes.len() < PREFIX_BYTES + CHECKSUM_BYTES
            || bytes.len() > MAX_RECORD_BYTES
            || bytes[..4] != *BINDING_MAGIC
            || bytes[4] != VERSION
            || bytes[5] != 0
        {
            return Err(CheckpointStoreError::InvalidState);
        }
        let authority_len = usize::from(u16::from_be_bytes([bytes[6], bytes[7]]));
        let authority_end = PREFIX_BYTES
            .checked_add(authority_len)
            .ok_or(CheckpointStoreError::InvalidState)?;
        let checksum_end = authority_end
            .checked_add(CHECKSUM_BYTES)
            .ok_or(CheckpointStoreError::InvalidState)?;
        if checksum_end != bytes.len()
            || Sha256::digest(&bytes[..authority_end]).as_slice() != &bytes[authority_end..]
        {
            return Err(CheckpointStoreError::InvalidState);
        }
        std::str::from_utf8(&bytes[PREFIX_BYTES..authority_end])
            .map_err(|_| CheckpointStoreError::InvalidState)
            .and_then(|value| {
                AuthorityId::new(value.to_owned()).map_err(|_| CheckpointStoreError::InvalidState)
            })
    }

    fn publish_unlocked(
        &self,
        final_name: &[u8],
        temporary_prefix: &[u8],
        record: &[u8],
    ) -> Result<(), CheckpointStoreError> {
        let (name, mut file) = self.create_temporary(temporary_prefix)?;
        let result = (|| {
            file.write_all(record)
                .map_err(|_| CheckpointStoreError::Unavailable)?;
            file.sync_all()
                .map_err(|_| CheckpointStoreError::Unavailable)?;
            validate_private_regular_file(&file)?;
            self.rename(&name, final_name)?;
            self.directory
                .sync_all()
                .map_err(|_| CheckpointStoreError::Unavailable)
        })();
        if result.is_err() {
            let _ = self.unlink(&name);
        }
        result
    }

    fn acquire_lock(&self) -> Result<File, CheckpointStoreError> {
        let lock = self.open_lock()?;
        loop {
            // SAFETY: lock is an owned descriptor.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                validate_private_regular_file(&lock)?;
                return Ok(lock);
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(CheckpointStoreError::Unavailable);
        }
    }

    fn acquire_lock_until(
        &self,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> Result<File, CheckpointStoreCasUntilError> {
        check_lock_wait(deadline, cancelled)?;
        let lock = self
            .open_lock()
            .map_err(CheckpointStoreCasUntilError::Store)?;
        loop {
            check_lock_wait(deadline, cancelled)?;
            // SAFETY: lock is an owned descriptor.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                validate_private_regular_file(&lock)
                    .map_err(CheckpointStoreCasUntilError::Store)?;
                check_lock_wait(deadline, cancelled)?;
                return Ok(lock);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    wait_for_lock(deadline, cancelled)?;
                }
                _ => {
                    return Err(CheckpointStoreCasUntilError::Store(
                        CheckpointStoreError::Unavailable,
                    ));
                }
            }
        }
    }

    fn open_lock(&self) -> Result<File, CheckpointStoreError> {
        let name = CString::new(LOCK_NAME).expect("static lock name is valid");
        loop {
            // SAFETY: name is static and resolved below the retained directory.
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    FILE_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: fd is fresh and becomes owned by this File.
                let file = unsafe { File::from_raw_fd(fd) };
                set_private_mode(&file)?;
                validate_private_regular_file(&file)?;
                return Ok(file);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(CheckpointStoreError::Unavailable);
            }
            let file = self.open_child(LOCK_NAME, libc::O_RDWR | libc::O_NONBLOCK, 0)?;
            validate_private_regular_file(&file)?;
            return Ok(file);
        }
    }

    fn create_temporary(
        &self,
        temporary_prefix: &[u8],
    ) -> Result<(Vec<u8>, File), CheckpointStoreError> {
        for _ in 0..16 {
            let name = next_temporary_name(temporary_prefix)?;
            let c_name =
                CString::new(name.as_slice()).map_err(|_| CheckpointStoreError::Unavailable)?;
            // SAFETY: c_name is one component below the retained directory.
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    c_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    FILE_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: fd is fresh and becomes owned by this File.
                let file = unsafe { File::from_raw_fd(fd) };
                let result =
                    set_private_mode(&file).and_then(|()| validate_private_regular_file(&file));
                if result.is_err() {
                    let _ = self.unlink(&name);
                }
                result?;
                return Ok((name, file));
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(CheckpointStoreError::Unavailable);
        }
        Err(CheckpointStoreError::Unavailable)
    }

    fn cleanup_temporary_files_unlocked(&self) -> Result<(), CheckpointStoreError> {
        let names = self.temporary_names()?;
        let mut removed = false;
        for name in names {
            let Ok(Some(file)) = self.open_optional(&name, libc::O_RDONLY) else {
                continue;
            };
            let Ok(metadata) = private_regular_metadata(&file) else {
                continue;
            };
            let identity = (metadata.dev(), metadata.ino());
            let Ok(Some(current)) = self.open_optional(&name, libc::O_RDONLY) else {
                continue;
            };
            let Ok(current_metadata) = private_regular_metadata(&current) else {
                continue;
            };
            if identity == (current_metadata.dev(), current_metadata.ino()) && self.unlink(&name) {
                removed = true;
            }
        }
        if removed {
            self.directory
                .sync_all()
                .map_err(|_| CheckpointStoreError::Unavailable)?;
        }
        Ok(())
    }

    fn temporary_names(&self) -> Result<Vec<Vec<u8>>, CheckpointStoreError> {
        // SAFETY: dup creates a new descriptor consumed by fdopendir.
        let fd = unsafe { libc::dup(self.directory.as_raw_fd()) };
        if fd < 0 {
            return Err(CheckpointStoreError::Unavailable);
        }
        // SAFETY: fd is uniquely owned here.
        let directory = unsafe { libc::fdopendir(fd) };
        if directory.is_null() {
            // SAFETY: fdopendir did not take ownership on failure.
            unsafe { libc::close(fd) };
            return Err(CheckpointStoreError::Unavailable);
        }
        let directory = DirectoryStream(directory);
        let mut names = Vec::new();
        loop {
            set_errno(Errno(0));
            // SAFETY: DirectoryStream retains a valid DIR pointer.
            let entry = unsafe { libc::readdir(directory.0) };
            if entry.is_null() {
                if errno().0 != 0 {
                    return Err(CheckpointStoreError::Unavailable);
                }
                break;
            }
            // SAFETY: d_name belongs to the returned directory entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name.starts_with(TEMP_PREFIX) || name.starts_with(BINDING_TEMP_PREFIX) {
                names.push(name.to_vec());
            }
        }
        Ok(names)
    }

    fn open_child(
        &self,
        name: &[u8],
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<File, CheckpointStoreError> {
        let name = checked_name(name)?;
        // SAFETY: name is one component below the retained directory.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(CheckpointStoreError::Unavailable);
        }
        // SAFETY: fd is fresh and becomes owned by this File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_optional(
        &self,
        name: &[u8],
        flags: libc::c_int,
    ) -> Result<Option<File>, CheckpointStoreError> {
        let name = checked_name(name)?;
        // SAFETY: O_NONBLOCK prevents an unexpected FIFO from blocking.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )
        };
        if fd >= 0 {
            // SAFETY: fd is fresh and becomes owned by this File.
            return Ok(Some(unsafe { File::from_raw_fd(fd) }));
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(CheckpointStoreError::InvalidEntry)
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> Result<(), CheckpointStoreError> {
        let from = checked_name(from)?;
        let to = checked_name(to)?;
        // SAFETY: both names are one component below the retained directory.
        if unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
            )
        } == 0
        {
            Ok(())
        } else {
            Err(CheckpointStoreError::Unavailable)
        }
    }

    fn unlink(&self, name: &[u8]) -> bool {
        let Ok(name) = checked_name(name) else {
            return false;
        };
        // SAFETY: the checked name is below the retained directory.
        unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) == 0 }
    }
}

impl fmt::Debug for CheckpointStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckpointStore")
            .field("authority", &self.authority)
            .field("directory", &"<retained>")
            .finish()
    }
}

/// The successful outcome of a compare-and-persist operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointStoreCas {
    /// The replacement is durable, including an idempotent retry.
    Durable,
    /// The expected checkpoint was not current.
    Conflict,
}

/// The reason a deadline-aware CAS could not return a CAS outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointStoreCasUntilError {
    /// The caller's absolute deadline elapsed while waiting for the writer lock.
    TimedOut,
    /// The caller cancelled while waiting for the writer lock.
    Cancelled,
    /// The state root, binding, or filesystem operation failed.
    Store(CheckpointStoreError),
}

/// A redacted durable-state failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointStoreError {
    /// The configured root was not exact private state owned by this UID.
    InvalidRoot,
    /// An entry under the retained root had an invalid type, owner, or mode.
    InvalidEntry,
    /// The durable state was malformed or corrupt.
    InvalidState,
    /// The durable state belongs to another authority ID.
    AuthorityMismatch,
    /// A filesystem operation failed without exposing details.
    Unavailable,
}

impl fmt::Display for CheckpointStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot => f.write_str("checkpoint authority root is invalid"),
            Self::InvalidEntry => f.write_str("checkpoint authority entry is invalid"),
            Self::InvalidState => f.write_str("checkpoint authority state is invalid"),
            Self::AuthorityMismatch => f.write_str("checkpoint authority state belongs elsewhere"),
            Self::Unavailable => f.write_str("checkpoint authority state is unavailable"),
        }
    }
}

impl Error for CheckpointStoreError {}

struct StoredRecord {
    checkpoint: AccountStoreCheckpoint,
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this value owns the DIR pointer from fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

fn check_lock_wait(
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<(), CheckpointStoreCasUntilError> {
    if cancelled() {
        return Err(CheckpointStoreCasUntilError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(CheckpointStoreCasUntilError::TimedOut);
    }
    Ok(())
}

fn wait_for_lock(
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<(), CheckpointStoreCasUntilError> {
    check_lock_wait(deadline, cancelled)?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or(CheckpointStoreCasUntilError::TimedOut)?;
    thread::sleep(remaining.min(LOCK_RETRY_INTERVAL));
    check_lock_wait(deadline, cancelled)
}

fn open_private_root(root: &Path) -> Result<File, CheckpointStoreError> {
    let components = checked_root_components(root)?;
    let owner_uid = effective_uid();
    let mut directory = open_root_directory()?;
    validate_trusted_ancestor(&directory, owner_uid)?;
    for component in components {
        directory = open_directory_child(&directory, &component)?;
        validate_trusted_ancestor(&directory, owner_uid)?;
    }
    validate_private_root(&directory, owner_uid)?;
    Ok(directory)
}

fn checked_root_components(root: &Path) -> Result<Vec<Vec<u8>>, CheckpointStoreError> {
    let bytes = root.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(CheckpointStoreError::InvalidRoot);
    }
    let mut components = Vec::new();
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b"." || component == b".." {
            return Err(CheckpointStoreError::InvalidRoot);
        }
        components.push(component.to_vec());
    }
    Ok(components)
}

fn open_root_directory() -> Result<File, CheckpointStoreError> {
    let root = CString::new("/").expect("root path has no NUL");
    // SAFETY: root is NUL-terminated and the fresh descriptor is owned below.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(CheckpointStoreError::InvalidRoot);
    }
    // SAFETY: fd is fresh and becomes owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_child(parent: &File, component: &[u8]) -> Result<File, CheckpointStoreError> {
    let component = CString::new(component).map_err(|_| CheckpointStoreError::InvalidRoot)?;
    // SAFETY: component is one NUL-terminated name resolved below parent.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(CheckpointStoreError::InvalidRoot);
    }
    // SAFETY: fd is fresh and becomes owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_trusted_ancestor(directory: &File, owner_uid: u32) -> Result<(), CheckpointStoreError> {
    let metadata = directory
        .metadata()
        .map_err(|_| CheckpointStoreError::InvalidRoot)?;
    if metadata.is_dir()
        && (metadata.uid() == 0 || metadata.uid() == owner_uid)
        && metadata.mode() & 0o022 == 0
    {
        Ok(())
    } else {
        Err(CheckpointStoreError::InvalidRoot)
    }
}

fn validate_private_root(directory: &File, owner_uid: u32) -> Result<(), CheckpointStoreError> {
    let metadata = directory
        .metadata()
        .map_err(|_| CheckpointStoreError::InvalidRoot)?;
    if metadata.is_dir() && metadata.uid() == owner_uid && metadata.mode() & 0o7777 == ROOT_MODE {
        Ok(())
    } else {
        Err(CheckpointStoreError::InvalidRoot)
    }
}

fn private_regular_metadata(file: &File) -> Result<std::fs::Metadata, CheckpointStoreError> {
    let metadata = file
        .metadata()
        .map_err(|_| CheckpointStoreError::InvalidEntry)?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != FILE_MODE
    {
        return Err(CheckpointStoreError::InvalidEntry);
    }
    Ok(metadata)
}

fn validate_private_regular_file(file: &File) -> Result<(), CheckpointStoreError> {
    private_regular_metadata(file).map(|_| ())
}

fn set_private_mode(file: &File) -> Result<(), CheckpointStoreError> {
    // SAFETY: file owns this descriptor.
    if unsafe { libc::fchmod(file.as_raw_fd(), FILE_MODE as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(CheckpointStoreError::Unavailable)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments.
    unsafe { libc::geteuid() }
}

fn checked_name(name: &[u8]) -> Result<CString, CheckpointStoreError> {
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') || name == b"." || name == b".."
    {
        return Err(CheckpointStoreError::InvalidEntry);
    }
    CString::new(name).map_err(|_| CheckpointStoreError::InvalidEntry)
}

fn next_temporary_name(temporary_prefix: &[u8]) -> Result<Vec<u8>, CheckpointStoreError> {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| CheckpointStoreError::Unavailable)?;
    let mut name = temporary_prefix.to_vec();
    name.extend_from_slice(std::process::id().to_string().as_bytes());
    name.push(b'.');
    name.extend_from_slice(id.to_string().as_bytes());
    name.push(b'.');
    for byte in random {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        name.push(HEX[(byte >> 4) as usize]);
        name.push(HEX[(byte & 0x0f) as usize]);
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        os::fd::AsRawFd,
        os::unix::{
            ffi::OsStringExt,
            fs::{symlink, MetadataExt, PermissionsExt},
        },
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering as AtomicOrdering},
            Arc, Barrier,
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    fn authority() -> AuthorityId {
        AuthorityId::new("local-accounts-v1").unwrap()
    }

    fn checkpoint(revision: u64, digest: u8) -> AccountStoreCheckpoint {
        let mut bytes = [0; CHECKPOINT_BYTES];
        bytes[..32].fill(1);
        bytes[32..40].copy_from_slice(&revision.to_be_bytes());
        bytes[40..].fill(digest);
        AccountStoreCheckpoint::from_bytes(&bytes).unwrap()
    }

    fn checkpoint_for_other_store(revision: u64) -> AccountStoreCheckpoint {
        let mut bytes = checkpoint(revision, 1).to_bytes();
        bytes[..32].fill(2);
        AccountStoreCheckpoint::from_bytes(&bytes).unwrap()
    }

    fn private_root() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::Builder::new()
            .prefix("turso-checkpoint-store-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let path = root.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(ROOT_MODE)).unwrap();
        (root, path)
    }

    fn private_child(parent: &Path, name: &str) -> PathBuf {
        let child = parent.join(name);
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(ROOT_MODE)).unwrap();
        child
    }

    fn state_path(root: &Path) -> PathBuf {
        root.join(std::str::from_utf8(FINAL_NAME).unwrap())
    }

    fn binding_path(root: &Path) -> PathBuf {
        root.join(std::str::from_utf8(BINDING_NAME).unwrap())
    }

    fn hold_writer_lock(root: &Path) -> File {
        let lock = File::open(root.join(std::str::from_utf8(LOCK_NAME).unwrap())).unwrap();
        // SAFETY: this test owns the separate lock descriptor until it is dropped.
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        lock
    }

    #[test]
    fn persists_reopens_and_repeats_idempotently() {
        let (_temp, root) = private_root();
        let first = checkpoint(0, 1);
        let second = checkpoint(1, 2);
        let store = CheckpointStore::open(&root, authority()).unwrap();
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(
            store.compare_and_persist(None, &first).unwrap(),
            CheckpointStoreCas::Durable
        );
        assert_eq!(
            store.compare_and_persist(None, &first).unwrap(),
            CheckpointStoreCas::Durable
        );
        assert_eq!(
            store.compare_and_persist(Some(&first), &second).unwrap(),
            CheckpointStoreCas::Durable
        );
        drop(store);
        assert_eq!(
            CheckpointStore::open(&root, authority())
                .unwrap()
                .read()
                .unwrap(),
            Some(second)
        );
        let metadata = fs::metadata(state_path(&root)).unwrap();
        assert_eq!(metadata.mode() & 0o7777, FILE_MODE);
        assert_eq!(metadata.uid(), effective_uid());
    }

    #[test]
    fn rejects_conflicts_and_binds_one_authority() {
        let (_temp, root) = private_root();
        let first = checkpoint(0, 1);
        let store = CheckpointStore::open(&root, authority()).unwrap();
        store.compare_and_persist(None, &first).unwrap();
        assert_eq!(
            store
                .compare_and_persist(Some(&checkpoint(0, 3)), &checkpoint(1, 2))
                .unwrap(),
            CheckpointStoreCas::Conflict
        );
        drop(store);
        assert!(matches!(
            CheckpointStore::open(&root, AuthorityId::new("other").unwrap()),
            Err(CheckpointStoreError::AuthorityMismatch)
        ));
    }

    #[test]
    fn empty_root_is_durably_bound_before_the_first_checkpoint() {
        let (_temp, root) = private_root();
        let store = CheckpointStore::open(&root, authority()).unwrap();
        assert_eq!(store.read().unwrap(), None);
        drop(store);

        let metadata = fs::metadata(binding_path(&root)).unwrap();
        assert_eq!(metadata.mode() & 0o7777, FILE_MODE);
        assert_eq!(metadata.uid(), effective_uid());
        assert!(matches!(
            CheckpointStore::open(&root, AuthorityId::new("other").unwrap()),
            Err(CheckpointStoreError::AuthorityMismatch)
        ));
    }

    #[test]
    fn concurrent_first_opens_choose_one_authority_binding() {
        let (_temp, root) = private_root();
        let barrier = Arc::new(Barrier::new(3));
        let left_root = root.clone();
        let left_barrier = Arc::clone(&barrier);
        let left = thread::spawn(move || {
            left_barrier.wait();
            CheckpointStore::open(&left_root, authority()).map(|_| ())
        });
        let right_root = root;
        let right_barrier = Arc::clone(&barrier);
        let right = thread::spawn(move || {
            right_barrier.wait();
            CheckpointStore::open(&right_root, AuthorityId::new("other").unwrap()).map(|_| ())
        });
        barrier.wait();

        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(CheckpointStoreError::AuthorityMismatch)))
                .count(),
            1
        );
    }

    #[test]
    fn binding_rejects_corruption_symlinks_and_wrong_modes() {
        let (_temp, corrupt_root) = private_root();
        CheckpointStore::open(&corrupt_root, authority()).unwrap();
        let corrupt_path = binding_path(&corrupt_root);
        let mut bytes = fs::read(&corrupt_path).unwrap();
        bytes[0] ^= 1;
        fs::write(&corrupt_path, bytes).unwrap();
        fs::set_permissions(&corrupt_path, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        assert!(matches!(
            CheckpointStore::open(&corrupt_root, authority()),
            Err(CheckpointStoreError::InvalidState)
        ));

        let (_temp, symlink_root) = private_root();
        CheckpointStore::open(&symlink_root, authority()).unwrap();
        let symlink_path = binding_path(&symlink_root);
        fs::remove_file(&symlink_path).unwrap();
        symlink("target", &symlink_path).unwrap();
        assert!(matches!(
            CheckpointStore::open(&symlink_root, authority()),
            Err(CheckpointStoreError::InvalidEntry)
        ));

        let (_temp, mode_root) = private_root();
        CheckpointStore::open(&mode_root, authority()).unwrap();
        let mode_path = binding_path(&mode_root);
        fs::set_permissions(&mode_path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            CheckpointStore::open(&mode_root, authority()),
            Err(CheckpointStoreError::InvalidEntry)
        ));
    }

    #[test]
    fn rejects_initial_and_replacement_revision_discontinuities() {
        let (_temp, root) = private_root();
        let store = CheckpointStore::open(&root, authority()).unwrap();
        assert_eq!(
            store.compare_and_persist(None, &checkpoint(1, 1)).unwrap(),
            CheckpointStoreCas::Conflict
        );
        let first = checkpoint(0, 1);
        assert_eq!(
            store.compare_and_persist(None, &first).unwrap(),
            CheckpointStoreCas::Durable
        );
        for invalid in [
            checkpoint(0, 2),
            checkpoint(2, 2),
            checkpoint_for_other_store(1),
        ] {
            assert_eq!(
                store.compare_and_persist(Some(&first), &invalid).unwrap(),
                CheckpointStoreCas::Conflict
            );
            assert_eq!(store.read().unwrap(), Some(first));
        }
    }

    #[test]
    fn rejects_corrupt_state_symlinks_and_wrong_modes() {
        let (_temp, root) = private_root();
        let store = CheckpointStore::open(&root, authority()).unwrap();
        store.compare_and_persist(None, &checkpoint(0, 1)).unwrap();
        drop(store);
        let path = state_path(&root);
        let mut bytes = fs::read(&path).unwrap();
        bytes[10] ^= 1;
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        assert_eq!(
            CheckpointStore::open(&root, authority()).unwrap().read(),
            Err(CheckpointStoreError::InvalidState)
        );
        fs::remove_file(&path).unwrap();
        symlink("target", &path).unwrap();
        assert_eq!(
            CheckpointStore::open(&root, authority()).unwrap().read(),
            Err(CheckpointStoreError::InvalidEntry)
        );
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            CheckpointStore::open(&root, authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
    }

    #[test]
    fn open_cleans_only_valid_stale_temporaries() {
        let (_temp, root) = private_root();
        let stale = root.join(".turso-mysql-checkpoint-v1.tmp.stale");
        fs::write(&stale, b"stale").unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        let binding_stale =
            root.join(std::str::from_utf8(BINDING_TEMP_PREFIX).unwrap().to_owned() + "stale");
        fs::write(&binding_stale, b"stale").unwrap();
        fs::set_permissions(&binding_stale, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        let unrelated = root.join("unrelated");
        fs::write(&unrelated, b"keep").unwrap();
        fs::set_permissions(&unrelated, fs::Permissions::from_mode(FILE_MODE)).unwrap();
        CheckpointStore::open(&root, authority()).unwrap();
        assert!(!stale.exists());
        assert!(!binding_stale.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn concurrent_compare_and_persist_has_one_winner() {
        let (_temp, root) = private_root();
        let first = checkpoint(0, 1);
        let second = checkpoint(1, 2);
        let third = checkpoint(1, 3);
        let store = CheckpointStore::open(&root, authority()).unwrap();
        store.compare_and_persist(None, &first).unwrap();
        let root_a = root.clone();
        let left = thread::spawn(move || {
            CheckpointStore::open(&root_a, authority())
                .unwrap()
                .compare_and_persist(Some(&first), &second)
                .unwrap()
        });
        let root_b = root;
        let right = thread::spawn(move || {
            CheckpointStore::open(&root_b, authority())
                .unwrap()
                .compare_and_persist(Some(&first), &third)
                .unwrap()
        });
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == CheckpointStoreCas::Durable)
                .count(),
            1
        );
        assert!(matches!(store.read().unwrap(), Some(value) if value == second || value == third));
    }

    #[test]
    fn deadline_aware_cas_matches_the_public_cas() {
        let (_temp, root) = private_root();
        let store = CheckpointStore::open(&root, authority()).unwrap();
        let first = checkpoint(0, 1);
        assert_eq!(
            store.compare_and_persist_until(
                None,
                &first,
                Instant::now() + Duration::from_secs(1),
                || false,
            ),
            Ok(CheckpointStoreCas::Durable)
        );
        assert_eq!(
            store.compare_and_persist(None, &first).unwrap(),
            CheckpointStoreCas::Durable
        );
    }

    #[test]
    fn deadline_aware_cas_times_out_while_the_writer_lock_is_held() {
        let (_temp, root) = private_root();
        let store = Arc::new(CheckpointStore::open(&root, authority()).unwrap());
        let lock = hold_writer_lock(&root);
        let contender = Arc::clone(&store);
        let started = Instant::now();
        let result = thread::spawn(move || {
            contender.compare_and_persist_until(
                None,
                &checkpoint(0, 1),
                Instant::now() + Duration::from_millis(50),
                || false,
            )
        })
        .join()
        .unwrap();
        assert_eq!(result, Err(CheckpointStoreCasUntilError::TimedOut));
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(lock);
    }

    #[test]
    fn deadline_aware_cas_observes_cancellation_while_waiting_for_the_writer_lock() {
        let (_temp, root) = private_root();
        let store = CheckpointStore::open(&root, authority()).unwrap();
        let lock = hold_writer_lock(&root);
        let cancelled = Arc::new(AtomicBool::new(false));
        let setter = Arc::clone(&cancelled);
        let cancellation = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            setter.store(true, AtomicOrdering::Release);
        });
        let result = store.compare_and_persist_until(
            None,
            &checkpoint(0, 1),
            Instant::now() + Duration::from_secs(1),
            || cancelled.load(AtomicOrdering::Acquire),
        );
        cancellation.join().unwrap();
        assert_eq!(result, Err(CheckpointStoreCasUntilError::Cancelled));
        drop(lock);
    }

    #[test]
    fn deadline_aware_cas_persists_after_the_writer_lock_is_released() {
        let (_temp, root) = private_root();
        let store = Arc::new(CheckpointStore::open(&root, authority()).unwrap());
        let lock = hold_writer_lock(&root);
        let contender = Arc::clone(&store);
        let worker = thread::spawn(move || {
            contender.compare_and_persist_until(
                None,
                &checkpoint(0, 1),
                Instant::now() + Duration::from_secs(1),
                || false,
            )
        });
        thread::sleep(Duration::from_millis(20));
        drop(lock);
        assert_eq!(worker.join().unwrap(), Ok(CheckpointStoreCas::Durable));
        assert_eq!(store.read().unwrap(), Some(checkpoint(0, 1)));
    }

    #[test]
    fn existing_lock_must_be_private() {
        let (_temp, root) = private_root();
        File::create(root.join(std::str::from_utf8(LOCK_NAME).unwrap())).unwrap();
        assert!(matches!(
            CheckpointStore::open(&root, authority()),
            Err(CheckpointStoreError::InvalidEntry)
        ));
    }

    #[test]
    fn open_rejects_symlink_and_writable_ancestors() {
        let (_temp, root) = private_root();
        let target = private_child(&root, "target");
        private_child(&target, "child");
        symlink(&target, root.join("link")).unwrap();
        assert!(matches!(
            CheckpointStore::open(root.join("link").join("child"), authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));

        let ancestor = private_child(&root, "ancestor");
        let child = private_child(&ancestor, "child");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o730)).unwrap();
        assert!(matches!(
            CheckpointStore::open(&child, authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
    }

    #[test]
    fn open_rejects_relative_and_dot_root_components() {
        let (_temp, root) = private_root();
        assert!(matches!(
            CheckpointStore::open("relative", authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
        let nul_path = PathBuf::from(std::ffi::OsString::from_vec(b"/invalid\0root".to_vec()));
        assert!(matches!(
            CheckpointStore::open(nul_path, authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
        assert!(matches!(
            CheckpointStore::open(root.join("."), authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
        assert!(matches!(
            CheckpointStore::open(root.join(".."), authority()),
            Err(CheckpointStoreError::InvalidRoot)
        ));
    }
}
