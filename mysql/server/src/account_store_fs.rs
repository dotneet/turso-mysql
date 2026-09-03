//! Unix persistence for the authenticated account snapshot.
//!
//! This module deliberately stores opaque bytes.  The account and privilege
//! codec owns the meaning of those bytes; this layer only gives it a private,
//! crash-safe file replacement primitive.  The constructor is the only
//! operation that accepts a path.  Once constructed, every child operation
//! uses the retained directory descriptor.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    thread,
    time::{Duration, Instant},
};

use errno::{errno, set_errno, Errno};
use zeroize::Zeroizing;

const FINAL_FILE_NAME: &[u8] = b".turso-mysql-authz-v1";
const LOCK_FILE_NAME: &[u8] = b".turso-mysql-authz.lock";
const PROVISIONING_LOCK_FILE_NAME: &[u8] = b".turso-mysql-provision.lock";
const TEMP_FILE_PREFIX: &[u8] = b".turso-mysql-authz-v1.tmp.";
const PENDING_FILE_NAME: &[u8] = b".turso-mysql-provision-pending-v1";
const PENDING_TEMP_PREFIX: &[u8] = b".turso-mysql-provision-pending-v1.tmp.";
const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PENDING_BYTES: usize = 512;
const PRIVATE_MODE: u32 = 0o600;
const PRIVATE_ROOT_MODE: u32 = 0o700;
const PROVISIONING_LOCK_RETRY: Duration = Duration::from_millis(10);

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Filesystem failures are intentionally coarse so neither paths nor backend
/// messages can leak into logs or protocol errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountStoreFsError {
    /// The configured directory is not a private directory owned by this uid.
    InvalidRoot,
    /// A snapshot, lock, or temporary entry failed its private-file checks.
    InvalidEntry,
    /// The stored snapshot is larger than the bounded read/write limit.
    SnapshotTooLarge,
    /// An operating-system operation failed without a more specific category.
    Backend,
    /// Another provisioning transaction retained the bounded provisioning lock.
    ProvisioningLockTimedOut,
}

impl std::fmt::Display for AccountStoreFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRoot => f.write_str("account store root is invalid"),
            Self::InvalidEntry => f.write_str("account store entry is invalid"),
            Self::SnapshotTooLarge => f.write_str("account store snapshot is too large"),
            Self::Backend => f.write_str("account store filesystem operation failed"),
            Self::ProvisioningLockTimedOut => f.write_str("account store provisioning is busy"),
        }
    }
}

impl std::error::Error for AccountStoreFsError {}

/// The result of comparing the on-disk snapshot while holding the writer lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionalPublish {
    /// The new bytes were durably published.
    Published {
        /// The inode that was published under the final snapshot name.
        identity: AccountStoreSnapshotIdentity,
    },
    /// The expected final-file state did not match the on-disk state.
    Conflict,
}

/// Stable identity for one snapshot inode below a retained account root.
///
/// The identity is captured while publishing and must be supplied to any
/// cleanup that may unlink the final name. Bytes alone are not enough because
/// another writer may have replaced the pathname between a failed CAS and
/// cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountStoreSnapshotIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConditionalRemove {
    Removed,
    AlreadyAbsent,
    Conflict,
}

/// A retained capability for one private account-store directory.
pub(crate) struct AccountStoreRoot {
    directory: File,
}

impl std::fmt::Debug for AccountStoreRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountStoreRoot")
            .field("directory", &"<retained>")
            .finish()
    }
}

impl AccountStoreRoot {
    /// Opens an explicitly configured private directory and retains its fd.
    pub(crate) fn open(path: &Path) -> Result<Self, AccountStoreFsError> {
        let components = checked_root_components(path)?;
        let owner_uid = effective_uid();
        let mut directory = open_root_directory()?;
        validate_trusted_ancestor(&directory, owner_uid)?;
        for component in components {
            directory = open_directory_child(&directory, &component)?;
            validate_trusted_ancestor(&directory, owner_uid)?;
        }
        if !is_private_directory(&directory) {
            return Err(AccountStoreFsError::InvalidRoot);
        }
        Ok(Self { directory })
    }

    /// Reads the current snapshot, or `None` when it has not been published.
    pub(crate) fn read_snapshot(&self) -> Result<Option<Zeroizing<Vec<u8>>>, AccountStoreFsError> {
        self.read_snapshot_unlocked()
    }

    /// Publishes only when no final snapshot exists while holding the writer lock.
    pub(crate) fn publish_if_absent(
        &self,
        bytes: &[u8],
    ) -> Result<ConditionalPublish, AccountStoreFsError> {
        self.with_writer_lock(|| {
            if self.read_snapshot_unlocked()?.is_some() {
                Ok(ConditionalPublish::Conflict)
            } else {
                let identity = self.publish_snapshot_unlocked(bytes)?;
                Ok(ConditionalPublish::Published { identity })
            }
        })
    }

    pub(crate) fn publish_if_absent_until(
        &self,
        bytes: &[u8],
        deadline: Instant,
    ) -> Result<ConditionalPublish, AccountStoreFsError> {
        self.with_writer_lock_until(deadline, || {
            if self.read_snapshot_unlocked()?.is_some() {
                Ok(ConditionalPublish::Conflict)
            } else {
                let identity = self.publish_snapshot_unlocked(bytes)?;
                Ok(ConditionalPublish::Published { identity })
            }
        })
    }

    /// Publishes only when the final snapshot still exactly matches `expected`.
    pub(crate) fn publish_if_unchanged(
        &self,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<ConditionalPublish, AccountStoreFsError> {
        self.with_writer_lock(|| {
            let current = self.read_snapshot_unlocked()?;
            if current.is_none_or(|bytes| bytes.as_slice() != expected) {
                return Ok(ConditionalPublish::Conflict);
            }
            let identity = self.publish_snapshot_unlocked(replacement)?;
            Ok(ConditionalPublish::Published { identity })
        })
    }

    /// Removes the final snapshot only when its retained inode and bytes are
    /// still the ones published by the caller. The writer lock serializes this
    /// with all account-store writers that use this capability.
    pub(crate) fn remove_snapshot_if_matches(
        &self,
        expected_identity: AccountStoreSnapshotIdentity,
        expected_bytes: &[u8],
    ) -> Result<ConditionalRemove, AccountStoreFsError> {
        self.with_writer_lock(|| {
            self.remove_snapshot_if_matches_unlocked(expected_identity, expected_bytes)
        })
    }

    pub(crate) fn remove_snapshot_if_matches_until(
        &self,
        expected_identity: AccountStoreSnapshotIdentity,
        expected_bytes: &[u8],
        deadline: Instant,
    ) -> Result<ConditionalRemove, AccountStoreFsError> {
        self.with_writer_lock_until(deadline, || {
            self.remove_snapshot_if_matches_unlocked(expected_identity, expected_bytes)
        })
    }

    fn read_snapshot_unlocked(&self) -> Result<Option<Zeroizing<Vec<u8>>>, AccountStoreFsError> {
        let Some(mut file) = self.open_optional_child(FINAL_FILE_NAME, libc::O_RDONLY)? else {
            return Ok(None);
        };
        Self::read_snapshot_file(&mut file).map(Some)
    }

    fn read_snapshot_file(file: &mut File) -> Result<Zeroizing<Vec<u8>>, AccountStoreFsError> {
        let metadata = private_regular_metadata(file)?;
        let length =
            usize::try_from(metadata.len()).map_err(|_| AccountStoreFsError::SnapshotTooLarge)?;
        if length > MAX_SNAPSHOT_BYTES {
            return Err(AccountStoreFsError::SnapshotTooLarge);
        }

        // The size comes from fstat on the same descriptor that is read. The
        // extra byte check catches a file that grew after fstat without making
        // the secret buffer grow through a Vec reallocation.
        let mut bytes = Zeroizing::new(vec![0u8; length]);
        file.read_exact(&mut bytes)
            .map_err(|_| AccountStoreFsError::Backend)?;
        let mut extra = Zeroizing::new([0u8; 1]);
        match file.read(&mut extra[..]) {
            Ok(0) => {}
            Ok(_) => return Err(AccountStoreFsError::SnapshotTooLarge),
            Err(_) => return Err(AccountStoreFsError::Backend),
        }
        Ok(bytes)
    }

    fn read_private_file(
        &self,
        name: &[u8],
        maximum: usize,
    ) -> Result<Option<Vec<u8>>, AccountStoreFsError> {
        let Some(mut file) = self.open_optional_child(name, libc::O_RDONLY)? else {
            return Ok(None);
        };
        self.read_file_exact(&mut file, maximum).map(Some)
    }

    fn read_file_exact(
        &self,
        file: &mut File,
        maximum: usize,
    ) -> Result<Vec<u8>, AccountStoreFsError> {
        let metadata = private_regular_metadata(file)?;
        let length =
            usize::try_from(metadata.len()).map_err(|_| AccountStoreFsError::SnapshotTooLarge)?;
        if length > maximum {
            return Err(AccountStoreFsError::SnapshotTooLarge);
        }
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)
            .map_err(|_| AccountStoreFsError::Backend)?;
        let mut extra = [0; 1];
        match file.read(&mut extra) {
            Ok(0) => Ok(bytes),
            Ok(_) => Err(AccountStoreFsError::SnapshotTooLarge),
            Err(_) => Err(AccountStoreFsError::Backend),
        }
    }

    /// Publishes one complete snapshot through a private writer lock and an
    /// fsynced temp-file rename. The old final file is never read or followed.
    #[cfg(test)]
    pub(crate) fn publish_snapshot(&self, bytes: &[u8]) -> Result<(), AccountStoreFsError> {
        self.with_writer_lock(|| self.publish_snapshot_unlocked(bytes).map(|_| ()))
    }

    fn publish_snapshot_unlocked(
        &self,
        bytes: &[u8],
    ) -> Result<AccountStoreSnapshotIdentity, AccountStoreFsError> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(AccountStoreFsError::SnapshotTooLarge);
        }
        let temporary = self.create_private_temporary(TEMP_FILE_PREFIX)?;
        let temporary_name = temporary.0;
        let mut file = temporary.1;

        let result = (|| {
            file.write_all(bytes)
                .map_err(|_| AccountStoreFsError::Backend)?;
            file.sync_all().map_err(|_| AccountStoreFsError::Backend)?;
            let identity = snapshot_identity(&private_regular_metadata(&file)?);
            self.rename_child(&temporary_name, FINAL_FILE_NAME)?;
            self.sync_directory()?;
            Ok(identity)
        })();

        if result.is_err() {
            // Cleanup is deliberately best effort. The primary operation's
            // classification is more useful than a second unlink failure.
            self.unlink_child_if_present(&temporary_name);
        }
        result
    }

    fn publish_private_file(
        &self,
        final_name: &[u8],
        temporary_prefix: &[u8],
        bytes: &[u8],
    ) -> Result<(), AccountStoreFsError> {
        let (temporary_name, mut file) = self.create_private_temporary(temporary_prefix)?;
        let result = (|| {
            file.write_all(bytes)
                .map_err(|_| AccountStoreFsError::Backend)?;
            file.sync_all().map_err(|_| AccountStoreFsError::Backend)?;
            validate_private_regular_file(&file)?;
            self.rename_child(&temporary_name, final_name)?;
            self.sync_directory()
        })();
        if result.is_err() {
            self.unlink_child_if_present(&temporary_name);
        }
        result
    }

    /// Removes private temporary files left by an interrupted writer.
    ///
    /// The method acquires the same writer lock as publishing, so an
    /// independent cleanup caller remains safe.
    pub(crate) fn cleanup_temporary_files(&self) -> Result<(), AccountStoreFsError> {
        self.with_writer_lock(|| self.cleanup_temporary_files_unlocked())
    }

    pub(crate) fn cleanup_temporary_files_until(
        &self,
        deadline: Instant,
    ) -> Result<(), AccountStoreFsError> {
        self.with_writer_lock_until(deadline, || self.cleanup_temporary_files_unlocked())
    }

    fn cleanup_temporary_files_unlocked(&self) -> Result<(), AccountStoreFsError> {
        let names = self.private_temporary_names()?;
        let mut removed = false;
        for name in names {
            let Ok(Some(file)) = self.open_optional_child(&name, libc::O_RDONLY) else {
                continue;
            };
            let Ok(metadata) = private_regular_metadata(&file) else {
                continue;
            };
            let identity = (metadata.dev(), metadata.ino());
            let Ok(Some(current)) = self.open_optional_child(&name, libc::O_RDONLY) else {
                continue;
            };
            let Ok(current_metadata) = private_regular_metadata(&current) else {
                continue;
            };
            if identity != (current_metadata.dev(), current_metadata.ino()) {
                continue;
            }
            if self.unlink_child_if_present(&name) {
                removed = true;
            }
        }
        if removed {
            self.sync_directory()?;
        }
        Ok(())
    }

    pub(crate) fn acquire_provisioning_lock(&self) -> Result<File, AccountStoreFsError> {
        self.acquire_named_lock(PROVISIONING_LOCK_FILE_NAME, None)
    }

    pub(crate) fn acquire_provisioning_lock_until(
        &self,
        deadline: Instant,
    ) -> Result<File, AccountStoreFsError> {
        self.acquire_named_lock(PROVISIONING_LOCK_FILE_NAME, Some(deadline))
    }

    pub(crate) fn read_provisioning_journal(&self) -> Result<Option<Vec<u8>>, AccountStoreFsError> {
        self.read_private_file(PENDING_FILE_NAME, MAX_PENDING_BYTES)
    }

    pub(crate) fn publish_provisioning_journal(
        &self,
        bytes: &[u8],
    ) -> Result<(), AccountStoreFsError> {
        if bytes.len() > MAX_PENDING_BYTES {
            return Err(AccountStoreFsError::SnapshotTooLarge);
        }
        self.publish_private_file(PENDING_FILE_NAME, PENDING_TEMP_PREFIX, bytes)
    }

    pub(crate) fn clear_provisioning_journal_if_matches(
        &self,
        expected: &[u8],
    ) -> Result<(), AccountStoreFsError> {
        let Some(mut first) = self.open_optional_child(PENDING_FILE_NAME, libc::O_RDONLY)? else {
            return Ok(());
        };
        let identity = snapshot_identity(&private_regular_metadata(&first)?);
        if self.read_file_exact(&mut first, MAX_PENDING_BYTES)? != expected {
            return Err(AccountStoreFsError::InvalidEntry);
        }
        let Some(mut current) = self.open_optional_child(PENDING_FILE_NAME, libc::O_RDONLY)? else {
            return Ok(());
        };
        if snapshot_identity(&private_regular_metadata(&current)?) != identity
            || self.read_file_exact(&mut current, MAX_PENDING_BYTES)? != expected
        {
            return Err(AccountStoreFsError::InvalidEntry);
        }
        if !self.unlink_child(PENDING_FILE_NAME)? {
            return Ok(());
        }
        self.sync_directory()
    }

    fn acquire_writer_lock(&self) -> Result<File, AccountStoreFsError> {
        self.acquire_named_lock(LOCK_FILE_NAME, None)
    }

    fn acquire_writer_lock_until(&self, deadline: Instant) -> Result<File, AccountStoreFsError> {
        self.acquire_named_lock(LOCK_FILE_NAME, Some(deadline))
    }

    fn acquire_named_lock(
        &self,
        name: &[u8],
        deadline: Option<Instant>,
    ) -> Result<File, AccountStoreFsError> {
        let lock = self.open_named_lock_file(name)?;
        loop {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(AccountStoreFsError::ProvisioningLockTimedOut);
            }
            // SAFETY: `lock` is an open descriptor for the lock file and the
            // operation does not dereference any Rust-managed pointer.
            let operation = libc::LOCK_EX | if deadline.is_some() { libc::LOCK_NB } else { 0 };
            let result = unsafe { libc::flock(lock.as_raw_fd(), operation) };
            if result == 0 {
                validate_private_regular_file(&lock)?;
                return Ok(lock);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code)
                    if deadline.is_some()
                        && (code == libc::EAGAIN || code == libc::EWOULDBLOCK) =>
                {
                    let deadline = deadline.expect("bounded lock has a deadline");
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(AccountStoreFsError::ProvisioningLockTimedOut);
                    }
                    thread::sleep(remaining.min(PROVISIONING_LOCK_RETRY));
                }
                _ => return Err(AccountStoreFsError::Backend),
            }
        }
    }

    fn open_named_lock_file(&self, lock_name: &[u8]) -> Result<File, AccountStoreFsError> {
        let name = CString::new(lock_name).map_err(|_| AccountStoreFsError::InvalidEntry)?;
        loop {
            // Creating with O_EXCL lets us distinguish a new lock (which may
            // need fchmod after a restrictive umask) from an existing lock
            // whose mode must be rejected rather than silently repaired.
            // SAFETY: `name` is NUL-terminated and the directory fd is
            // retained by `self`; the returned fd is owned below.
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
                    PRIVATE_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                let lock = unsafe { File::from_raw_fd(fd) };
                let result =
                    set_private_mode(&lock).and_then(|()| validate_private_regular_file(&lock));
                if result.is_err() {
                    self.unlink_child_if_present(lock_name);
                }
                result?;
                return Ok(lock);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(AccountStoreFsError::Backend);
            }
            let lock = self.open_child(lock_name, libc::O_RDWR | libc::O_NONBLOCK, 0)?;
            validate_private_regular_file(&lock)?;
            return Ok(lock);
        }
    }

    fn with_writer_lock<T>(
        &self,
        action: impl FnOnce() -> Result<T, AccountStoreFsError>,
    ) -> Result<T, AccountStoreFsError> {
        let _lock = self.acquire_writer_lock()?;
        action()
    }

    fn with_writer_lock_until<T>(
        &self,
        deadline: Instant,
        action: impl FnOnce() -> Result<T, AccountStoreFsError>,
    ) -> Result<T, AccountStoreFsError> {
        let _lock = self.acquire_writer_lock_until(deadline)?;
        action()
    }

    fn remove_snapshot_if_matches_unlocked(
        &self,
        expected_identity: AccountStoreSnapshotIdentity,
        expected_bytes: &[u8],
    ) -> Result<ConditionalRemove, AccountStoreFsError> {
        let Some(mut file) = self.open_optional_child(FINAL_FILE_NAME, libc::O_RDONLY)? else {
            return Ok(ConditionalRemove::AlreadyAbsent);
        };
        let metadata = private_regular_metadata(&file)?;
        if snapshot_identity(&metadata) != expected_identity {
            return Ok(ConditionalRemove::Conflict);
        }
        if Self::read_snapshot_file(&mut file)?.as_slice() != expected_bytes {
            return Ok(ConditionalRemove::Conflict);
        }

        // Reopen immediately before unlinking so a pathname replacement
        // cannot turn this abort into deletion of another snapshot.
        let Some(mut current) = self.open_optional_child(FINAL_FILE_NAME, libc::O_RDONLY)? else {
            return Ok(ConditionalRemove::AlreadyAbsent);
        };
        let current_metadata = private_regular_metadata(&current)?;
        if snapshot_identity(&current_metadata) != expected_identity {
            return Ok(ConditionalRemove::Conflict);
        }
        if Self::read_snapshot_file(&mut current)?.as_slice() != expected_bytes {
            return Ok(ConditionalRemove::Conflict);
        }
        if !self.unlink_child(FINAL_FILE_NAME)? {
            return Ok(ConditionalRemove::AlreadyAbsent);
        }
        self.sync_directory()?;
        Ok(ConditionalRemove::Removed)
    }

    fn create_private_temporary(
        &self,
        prefix: &[u8],
    ) -> Result<(Vec<u8>, File), AccountStoreFsError> {
        for _ in 0..16 {
            let name = next_temporary_name(prefix)?;
            let c_name = CString::new(name.as_slice()).map_err(|_| AccountStoreFsError::Backend)?;
            // SAFETY: `c_name` is NUL-terminated and the directory fd is held
            // by `self`; O_EXCL makes a guessed temporary name harmless.
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    c_name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    PRIVATE_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                let file = unsafe { File::from_raw_fd(fd) };
                let result =
                    set_private_mode(&file).and_then(|()| validate_private_regular_file(&file));
                if result.is_err() {
                    self.unlink_child_if_present(&name);
                }
                result?;
                return Ok((name, file));
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(AccountStoreFsError::Backend);
        }
        Err(AccountStoreFsError::Backend)
    }

    fn open_child(
        &self,
        name: &[u8],
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Result<File, AccountStoreFsError> {
        let name = CString::new(name).map_err(|_| AccountStoreFsError::InvalidEntry)?;
        // SAFETY: `name` is NUL-terminated and the directory fd is retained by
        // `self`. No path supplied after construction reaches this syscall.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(AccountStoreFsError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_optional_child(
        &self,
        name: &[u8],
        flags: libc::c_int,
    ) -> Result<Option<File>, AccountStoreFsError> {
        let name = CString::new(name).map_err(|_| AccountStoreFsError::InvalidEntry)?;
        // SAFETY: `name` is NUL-terminated and the directory fd is retained by
        // `self`. O_NONBLOCK ensures a malicious FIFO cannot block this read.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is a fresh descriptor owned by this value.
            return Ok(Some(unsafe { File::from_raw_fd(fd) }));
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        Err(AccountStoreFsError::InvalidEntry)
    }

    fn rename_child(&self, from: &[u8], to: &[u8]) -> Result<(), AccountStoreFsError> {
        let from = CString::new(from).map_err(|_| AccountStoreFsError::Backend)?;
        let to = CString::new(to).map_err(|_| AccountStoreFsError::Backend)?;
        // SAFETY: both names are NUL-terminated and resolved relative to the
        // retained directory descriptor. `renameat` never follows `to`.
        let result = unsafe {
            libc::renameat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(AccountStoreFsError::Backend)
        }
    }

    fn unlink_child_if_present(&self, name: &[u8]) -> bool {
        self.unlink_child(name).unwrap_or(false)
    }

    fn unlink_child(&self, name: &[u8]) -> Result<bool, AccountStoreFsError> {
        let Ok(name) = CString::new(name) else {
            return Err(AccountStoreFsError::Backend);
        };
        loop {
            // SAFETY: `name` is NUL-terminated and resolved below the retained
            // directory descriptor. unlinkat does not follow a symlink.
            let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
            if result == 0 {
                return Ok(true);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::ENOENT) => return Ok(false),
                _ => return Err(AccountStoreFsError::Backend),
            }
        }
    }

    fn sync_directory(&self) -> Result<(), AccountStoreFsError> {
        self.directory
            .sync_all()
            .map_err(|_| AccountStoreFsError::Backend)
    }

    fn private_temporary_names(&self) -> Result<Vec<Vec<u8>>, AccountStoreFsError> {
        let duplicated =
            // SAFETY: duplicating a live directory descriptor does not borrow
            // through the returned integer; ownership transfers to fdopendir.
            unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            return Err(AccountStoreFsError::Backend);
        }
        // SAFETY: `duplicated` is a directory fd and is transferred to the
        // returned DIR stream when fdopendir succeeds.
        let stream = unsafe { libc::fdopendir(duplicated) };
        if stream.is_null() {
            // SAFETY: fdopendir did not consume the descriptor on failure.
            unsafe { libc::close(duplicated) };
            return Err(AccountStoreFsError::Backend);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            set_errno(Errno(0));
            // SAFETY: `stream.0` remains valid until `DirectoryStream` drops.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if errno().0 != 0 {
                    return Err(AccountStoreFsError::Backend);
                }
                break;
            }
            // SAFETY: d_name is the NUL-terminated name owned by this dirent.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name.starts_with(TEMP_FILE_PREFIX) || name.starts_with(PENDING_TEMP_PREFIX) {
                names.push(name.to_vec());
            }
        }
        Ok(names)
    }
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this stream owns the descriptor passed to fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

fn checked_root_components(path: &Path) -> Result<Vec<Vec<u8>>, AccountStoreFsError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(AccountStoreFsError::InvalidRoot);
    }
    let mut components = Vec::new();
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b"." || component == b".." {
            return Err(AccountStoreFsError::InvalidRoot);
        }
        components.push(component.to_vec());
    }
    Ok(components)
}

fn open_root_directory() -> Result<File, AccountStoreFsError> {
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
        return Err(AccountStoreFsError::InvalidRoot);
    }
    // SAFETY: fd is fresh and becomes owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_child(parent: &File, component: &[u8]) -> Result<File, AccountStoreFsError> {
    let component = CString::new(component).map_err(|_| AccountStoreFsError::InvalidRoot)?;
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
        return Err(AccountStoreFsError::InvalidRoot);
    }
    // SAFETY: fd is fresh and becomes owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_trusted_ancestor(directory: &File, owner_uid: u32) -> Result<(), AccountStoreFsError> {
    let metadata = directory
        .metadata()
        .map_err(|_| AccountStoreFsError::InvalidRoot)?;
    if metadata.is_dir()
        && (metadata.uid() == 0 || metadata.uid() == owner_uid)
        && metadata.mode() & 0o022 == 0
    {
        Ok(())
    } else {
        Err(AccountStoreFsError::InvalidRoot)
    }
}

fn is_private_directory(directory: &File) -> bool {
    let Ok(metadata) = directory.metadata() else {
        return false;
    };
    metadata.is_dir()
        && metadata.uid() == effective_uid()
        && metadata.mode() & 0o7777 == PRIVATE_ROOT_MODE
}

fn validate_private_regular_file(file: &File) -> Result<(), AccountStoreFsError> {
    private_regular_metadata(file).map(|_| ())
}

fn private_regular_metadata(file: &File) -> Result<std::fs::Metadata, AccountStoreFsError> {
    let metadata = file
        .metadata()
        .map_err(|_| AccountStoreFsError::InvalidEntry)?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != PRIVATE_MODE
    {
        return Err(AccountStoreFsError::InvalidEntry);
    }
    Ok(metadata)
}

fn snapshot_identity(metadata: &std::fs::Metadata) -> AccountStoreSnapshotIdentity {
    AccountStoreSnapshotIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn set_private_mode(file: &File) -> Result<(), AccountStoreFsError> {
    // SAFETY: the descriptor is owned by `file` for the duration of this call;
    // fchmod does not dereference a Rust-managed pointer.
    let result = unsafe { libc::fchmod(file.as_raw_fd(), PRIVATE_MODE as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(AccountStoreFsError::Backend)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid reads the effective uid of the current process and does
    // not dereference a pointer or access Rust-managed memory.
    unsafe { libc::geteuid() }
}

fn next_temporary_name(prefix: &[u8]) -> Result<Vec<u8>, AccountStoreFsError> {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| AccountStoreFsError::Backend)?;
    let mut name = prefix.to_vec();
    name.extend_from_slice(std::process::id().to_string().as_bytes());
    name.push(b'.');
    name.extend_from_slice(id.to_string().as_bytes());
    name.push(b'.');
    for byte in random {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        name.push(HEX[(byte >> 4) as usize]);
        name.push(HEX[(byte & 0xf) as usize]);
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn write_private_file(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn make_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated test path and no descriptor
        // or Rust reference is aliased by mkfifo.
        assert_eq!(
            unsafe { libc::mkfifo(path.as_ptr(), PRIVATE_MODE as libc::mode_t) },
            0
        );
    }

    #[test]
    fn root_requires_exact_private_mode_and_rejects_symlinks() {
        let root = private_root();
        assert!(AccountStoreRoot::open(root.path()).is_ok());
        assert!(matches!(
            AccountStoreRoot::open(Path::new(".")),
            Err(AccountStoreFsError::InvalidRoot)
        ));
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            AccountStoreRoot::open(root.path()),
            Err(AccountStoreFsError::InvalidRoot)
        ));

        let parent = private_root();
        let link = parent.path().join("root-link");
        symlink(root.path(), &link).unwrap();
        assert!(matches!(
            AccountStoreRoot::open(&link),
            Err(AccountStoreFsError::InvalidRoot)
        ));

        let writable_ancestor = private_root();
        let child = writable_ancestor.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(writable_ancestor.path(), fs::Permissions::from_mode(0o730)).unwrap();
        assert!(matches!(
            AccountStoreRoot::open(&child),
            Err(AccountStoreFsError::InvalidRoot)
        ));
    }

    #[test]
    fn round_trip_and_replacement_are_atomic_at_the_final_name() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        assert_eq!(store.read_snapshot().unwrap(), None);
        store.publish_snapshot(b"first snapshot").unwrap();
        assert_eq!(
            store.read_snapshot().unwrap().map(|bytes| bytes.to_vec()),
            Some(b"first snapshot".to_vec())
        );
        store.publish_snapshot(b"replacement").unwrap();
        assert_eq!(
            store.read_snapshot().unwrap().map(|bytes| bytes.to_vec()),
            Some(b"replacement".to_vec())
        );
        assert_eq!(
            fs::metadata(
                root.path()
                    .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME))
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o7777,
            PRIVATE_MODE
        );
    }

    #[test]
    fn symlink_and_fifo_targets_are_rejected_without_blocking() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        let outside = root.path().join(".account-store-outside-target");
        write_private_file(&outside, b"secret");
        symlink(
            &outside,
            root.path()
                .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME)),
        )
        .unwrap();
        assert_eq!(
            store.read_snapshot(),
            Err(AccountStoreFsError::InvalidEntry)
        );
        fs::remove_file(
            root.path()
                .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME)),
        )
        .unwrap();
        make_fifo(
            &root
                .path()
                .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME)),
        );
        assert_eq!(
            store.read_snapshot(),
            Err(AccountStoreFsError::InvalidEntry)
        );
        fs::remove_file(outside).unwrap();
    }

    #[test]
    fn oversized_snapshot_is_rejected_on_read_and_write() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        let oversized = vec![0x5a; MAX_SNAPSHOT_BYTES + 1];
        assert_eq!(
            store.publish_snapshot(&oversized),
            Err(AccountStoreFsError::SnapshotTooLarge)
        );
        write_private_file(
            &root
                .path()
                .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME)),
            &oversized,
        );
        assert_eq!(
            store.read_snapshot(),
            Err(AccountStoreFsError::SnapshotTooLarge)
        );
    }

    #[test]
    fn lock_and_snapshot_require_exact_private_file_mode() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        store.publish_snapshot(b"initial").unwrap();

        fs::set_permissions(
            root.path()
                .join(std::ffi::OsStr::from_bytes(LOCK_FILE_NAME)),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert_eq!(
            store.publish_snapshot(b"blocked"),
            Err(AccountStoreFsError::InvalidEntry)
        );
        fs::set_permissions(
            root.path()
                .join(std::ffi::OsStr::from_bytes(LOCK_FILE_NAME)),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        fs::set_permissions(
            root.path()
                .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME)),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert_eq!(
            store.read_snapshot(),
            Err(AccountStoreFsError::InvalidEntry)
        );
    }

    #[test]
    fn lock_fifo_is_rejected_without_blocking() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        make_fifo(
            &root
                .path()
                .join(std::ffi::OsStr::from_bytes(LOCK_FILE_NAME)),
        );
        assert_eq!(
            store.publish_snapshot(b"blocked"),
            Err(AccountStoreFsError::InvalidEntry)
        );
    }

    #[test]
    fn cleanup_removes_only_private_temp_residue() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        let stale = root
            .path()
            .join(String::from_utf8_lossy(TEMP_FILE_PREFIX).to_string() + "stale");
        write_private_file(&stale, b"unfinished");
        let unrelated = root.path().join("unrelated");
        write_private_file(&unrelated, b"keep");
        let wrong_mode = root
            .path()
            .join(String::from_utf8_lossy(TEMP_FILE_PREFIX).to_string() + "wrong-mode");
        write_private_file(&wrong_mode, b"keep");
        fs::set_permissions(&wrong_mode, fs::Permissions::from_mode(0o640)).unwrap();
        let symlink_target = root.path().join("symlink-target");
        write_private_file(&symlink_target, b"keep");
        let symlink_entry = root
            .path()
            .join(String::from_utf8_lossy(TEMP_FILE_PREFIX).to_string() + "symlink");
        symlink(&symlink_target, &symlink_entry).unwrap();
        let fifo = root
            .path()
            .join(String::from_utf8_lossy(TEMP_FILE_PREFIX).to_string() + "fifo");
        make_fifo(&fifo);
        store.cleanup_temporary_files().unwrap();
        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert!(wrong_mode.exists());
        assert!(fs::symlink_metadata(symlink_entry).is_ok());
        assert!(fs::metadata(fifo).is_ok());
    }

    #[test]
    fn lock_file_is_private_and_temp_files_do_not_remain_after_publish() {
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        store.publish_snapshot(b"payload").unwrap();
        let lock = fs::metadata(
            root.path()
                .join(std::ffi::OsStr::from_bytes(LOCK_FILE_NAME)),
        )
        .unwrap();
        assert_eq!(lock.permissions().mode() & 0o7777, PRIVATE_MODE);
        let entries = fs::read_dir(root.path()).unwrap();
        assert!(entries
            .filter_map(Result::ok)
            .all(|entry| { !entry.file_name().as_bytes().starts_with(TEMP_FILE_PREFIX) }));
    }

    #[test]
    fn restrictive_umask_still_produces_exact_private_modes() {
        const CHILD_ENV: &str = "TURSO_MYSQL_ACCOUNT_FS_UMASK_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(
                    "account_store_fs::tests::restrictive_umask_still_produces_exact_private_modes",
                )
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        // The child process isolates the process-global umask from sibling
        // unit tests that may create their own temporary directories.
        // SAFETY: umask only changes this child process's file-creation mask.
        unsafe { libc::umask(0o777) };
        let root = private_root();
        let store = AccountStoreRoot::open(root.path()).unwrap();
        store.publish_snapshot(b"payload").unwrap();
        let final_path = root
            .path()
            .join(std::ffi::OsStr::from_bytes(FINAL_FILE_NAME));
        let lock_path = root
            .path()
            .join(std::ffi::OsStr::from_bytes(LOCK_FILE_NAME));
        assert_eq!(
            fs::metadata(final_path).unwrap().permissions().mode() & 0o7777,
            PRIVATE_MODE
        );
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o7777,
            PRIVATE_MODE
        );
    }
}
