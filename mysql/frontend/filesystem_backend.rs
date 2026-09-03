//! Unix capability backend for [`super::RegistryRoot`].
//!
//! The constructor is the only operation that accepts a path. It immediately
//! opens that path as a directory and all later operations use names relative
//! to the retained descriptor. Database files contain a small registry
//! envelope until the format-v2 page marker and core attachment boundary is
//! implemented; this envelope is not presented as a SQLite database.

use super::*;
use serde::de::DeserializeOwned;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

#[path = "filesystem_backend/database_metadata.rs"]
mod database_metadata;

const MANIFEST_FILE: &str = ".turso-mysql-root.json";
const REGISTRY_FILE: &str = ".turso-mysql-registry.json";
const LOCK_FILE: &str = ".turso-mysql-root.lock";
const MAX_MANIFEST_BYTES: usize = 4096;
const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const DATABASE_ENVELOPE_MAGIC: &[u8] = b"TURSO_MYSQL_REGISTRY_ENVELOPE_V2\0";
const DATABASE_ENVELOPE_BYTES: usize = DATABASE_ENVELOPE_MAGIC.len() + 5 + 35;
const COMPANION_SUFFIX: &str = ".turso-mysql-registry-companion";
const PRIVATE_TEMP_MAX_AGE: Duration = Duration::from_secs(60 * 60);

const PRIVATE_TEMP_PREFIXES: [(&[u8], PrivateTemporaryKind); 3] = [
    (
        b".turso-mysql-registry.tmp.",
        PrivateTemporaryKind::Registry,
    ),
    (
        b".turso-mysql-database-main.tmp.",
        PrivateTemporaryKind::DatabaseMain,
    ),
    (
        b".turso-mysql-database-companion.tmp.",
        PrivateTemporaryKind::DatabaseCompanion,
    ),
];

static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

/// A retained directory capability. No child operation reinterprets a path.
pub(crate) struct OsDataRoot {
    directory: File,
    #[cfg(test)]
    private_temporary_cleanup_test_hook: Option<PrivateTemporaryCleanupTestHook>,
}

/// A cloneable reference to the lock file. Clones retain the same open handle.
pub(crate) struct OsRegistryLock(Arc<File>);

/// The inspected writable main/companion descriptor pair for one opaque database key.
///
/// Both files retain the handles that were checked through the root descriptor;
/// callers never reopen them by a logical database name. The companion is not
/// named as a SQLite WAL until this frontend writes a valid SQLite v2 main file.
pub(crate) struct OsDatabaseHandle {
    main_file: File,
    companion_file: File,
    identity: OpaqueFileKey,
}

/// A private writable main/companion pair retained through initialization and
/// publication. The open descriptors identify the same inodes later linked
/// under their final names.
pub(crate) struct OsDatabaseStage {
    main_file: File,
    companion_file: File,
    main_temporary: String,
    companion_temporary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseArtifactRole {
    Main,
    Companion,
}

#[derive(Debug, Clone, Copy)]
enum PrivateTemporaryKind {
    Registry,
    DatabaseMain,
    DatabaseCompanion,
}

#[cfg(test)]
struct PrivateTemporaryCleanupTestHook {
    fail_unlink_at_attempt: usize,
    unlink_attempts: usize,
    fsync_attempts: usize,
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: the stream owns the duplicated descriptor returned by
        // `fdopendir`, and is closed exactly once here.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

impl DatabaseArtifactRole {
    const fn as_byte(self) -> u8 {
        match self {
            Self::Main => 1,
            Self::Companion => 2,
        }
    }
}

impl Clone for OsRegistryLock {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl OsDatabaseHandle {
    pub(crate) fn main_file(&self) -> &File {
        &self.main_file
    }

    pub(crate) fn companion_file(&self) -> &File {
        &self.companion_file
    }

    pub(crate) fn identity(&self) -> &OpaqueFileKey {
        &self.identity
    }
}

impl OsDatabaseStage {
    pub(crate) fn main_file(&self) -> &File {
        &self.main_file
    }

    pub(crate) fn companion_file(&self) -> &File {
        &self.companion_file
    }
}

impl OsDataRoot {
    pub(crate) fn open(path: &Path) -> Result<Self, RegistryError> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| RegistryError::Backend)?;
        // This is the one path-based operation. The resulting descriptor is
        // retained and is the capability used by every method below.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        let directory = unsafe { File::from_raw_fd(fd) };
        let metadata = directory.metadata().map_err(|_| RegistryError::Backend)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(RegistryError::Backend);
        }
        Ok(Self {
            directory,
            #[cfg(test)]
            private_temporary_cleanup_test_hook: None,
        })
    }

    fn open_child(
        &self,
        name: &str,
        flags: i32,
        mode: libc::mode_t,
    ) -> Result<File, RegistryError> {
        let name = CString::new(name.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn open_child_optional(&self, name: &str, flags: i32) -> Result<Option<File>, RegistryError> {
        let name = CString::new(name.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0,
            )
        };
        if fd < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(RegistryError::Backend);
        }
        // SAFETY: `fd` is a fresh descriptor owned by this value.
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    }

    fn open_private_temporary(&self, name: &[u8]) -> Result<Option<File>, RegistryError> {
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        loop {
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                    0,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                return Ok(Some(unsafe { File::from_raw_fd(fd) }));
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // A candidate that disappeared, became a symlink, or is otherwise
            // not openable is left untouched. Cleanup must fail closed for
            // entries that cannot be inspected through this capability.
            return Ok(None);
        }
    }

    fn directory_private_temporary_names(&self) -> Result<Vec<Vec<u8>>, RegistryError> {
        let duplicated =
            unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            return Err(RegistryError::Backend);
        }
        let stream = unsafe { libc::fdopendir(duplicated) };
        if stream.is_null() {
            // SAFETY: `duplicated` was not consumed when `fdopendir` failed.
            unsafe {
                libc::close(duplicated);
            }
            return Err(RegistryError::Backend);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            errno::set_errno(errno::Errno(0));
            // SAFETY: `stream.0` is a valid directory stream until its Drop.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if errno::errno().0 != 0 {
                    return Err(RegistryError::Backend);
                }
                break;
            }
            // SAFETY: `d_name` is a NUL-terminated name supplied by libc.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if Self::private_temporary_kind(name).is_some() {
                names.push(name.to_vec());
            }
        }
        Ok(names)
    }

    fn private_temporary_kind(name: &[u8]) -> Option<PrivateTemporaryKind> {
        for (prefix, kind) in PRIVATE_TEMP_PREFIXES {
            let Some(rest) = name.strip_prefix(prefix) else {
                continue;
            };
            let mut parts = rest.split(|byte| *byte == b'.');
            let Some(pid) = parts.next() else {
                continue;
            };
            let Some(counter) = parts.next() else {
                continue;
            };
            let Some(random) = parts.next() else {
                continue;
            };
            if parts.next().is_none()
                && Self::positive_decimal(pid)
                && Self::positive_decimal(counter)
                && random.len() == 32
                && random
                    .iter()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Some(kind);
            }
        }
        None
    }

    fn positive_decimal(value: &[u8]) -> bool {
        if value.is_empty() || (value.len() > 1 && value[0] == b'0') {
            return false;
        }
        let mut number = 0u64;
        for byte in value {
            if !byte.is_ascii_digit() {
                return false;
            }
            let Some(next) = number
                .checked_mul(10)
                .and_then(|number| number.checked_add(u64::from(byte - b'0')))
            else {
                return false;
            };
            number = next;
        }
        number != 0
    }

    fn collect_private_temporary_files(&mut self) -> Result<(), RegistryError> {
        let now = SystemTime::now();
        let names = self.directory_private_temporary_names()?;
        let mut removed = false;
        for name in names {
            let Some(file) = self.open_private_temporary(&name)? else {
                continue;
            };
            let metadata = file.metadata().map_err(|_| RegistryError::Backend)?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || !Self::is_stale(
                    metadata.modified().map_err(|_| RegistryError::Backend)?,
                    now,
                )
            {
                continue;
            }
            let identity = Self::file_identity(&file)?;

            // Re-open after the age and ownership check. Cooperative registry
            // writers hold the same exclusive root lock, so the pathname
            // cannot be replaced by a live creator before unlink.
            let Some(current) = self.open_private_temporary(&name)? else {
                continue;
            };
            let current_metadata = current.metadata().map_err(|_| RegistryError::Backend)?;
            if !current_metadata.is_file()
                || current_metadata.uid() != unsafe { libc::geteuid() }
                || Self::file_identity(&current)? != identity
                || !Self::is_stale(
                    current_metadata
                        .modified()
                        .map_err(|_| RegistryError::Backend)?,
                    now,
                )
            {
                continue;
            }
            match self.unlink_private_temporary_if_present(&name) {
                Ok(did_remove) => removed |= did_remove,
                Err(error) => {
                    if removed {
                        self.fsync_dir()?;
                    }
                    return Err(error);
                }
            }
        }
        if removed {
            self.fsync_dir()?;
        }
        Ok(())
    }

    fn is_stale(modified: SystemTime, now: SystemTime) -> bool {
        now.duration_since(modified)
            .map(|age| age >= PRIVATE_TEMP_MAX_AGE)
            .unwrap_or(false)
    }

    fn read_bounded(file: File, limit: usize) -> Result<Vec<u8>, RegistryError> {
        let length = file.metadata().map_err(|_| RegistryError::Backend)?.len();
        if length > limit as u64 {
            return Err(RegistryError::Backend);
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| RegistryError::Backend)?;
        if bytes.len() > limit {
            return Err(RegistryError::Backend);
        }
        Ok(bytes)
    }

    fn read_json<T: DeserializeOwned>(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Option<T>, RegistryError> {
        let Some(file) = self.open_child_optional(name, libc::O_RDONLY)? else {
            return Ok(None);
        };
        let bytes = Self::read_bounded(file, limit)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| RegistryError::Backend)
    }

    fn file_identity(file: &File) -> Result<FileIdentity, RegistryError> {
        let metadata = file.metadata().map_err(|_| RegistryError::Backend)?;
        Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn inspect_open_database(
        file: &File,
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        if !file
            .metadata()
            .map_err(|_| RegistryError::Backend)?
            .is_file()
        {
            return Ok(DatabaseFileInspection::Mismatch);
        }
        let bytes = Self::read_bounded_at_start(file, DATABASE_ENVELOPE_BYTES)?;
        Ok(if Self::envelope_matches(&bytes, expected, role) {
            DatabaseFileInspection::Matching
        } else {
            DatabaseFileInspection::Mismatch
        })
    }

    fn read_bounded_at_start(file: &File, limit: usize) -> Result<Vec<u8>, RegistryError> {
        if file.metadata().map_err(|_| RegistryError::Backend)?.len() > limit as u64 {
            return Err(RegistryError::Backend);
        }
        let mut bytes = vec![0; limit + 1];
        let mut read = 0;
        while read < bytes.len() {
            let count = file
                .read_at(&mut bytes[read..], read as u64)
                .map_err(|_| RegistryError::Backend)?;
            if count == 0 {
                break;
            }
            read += count;
        }
        bytes.truncate(read);
        if bytes.len() > limit {
            return Err(RegistryError::Backend);
        }
        Ok(bytes)
    }

    fn tombstone_name(expected: &DatabaseFileExpectation) -> String {
        format!(
            ".turso-mysql-database-tombstone-{}",
            expected.file_key().as_str()
        )
    }

    fn companion_name(expected: &DatabaseFileExpectation) -> String {
        format!("{}{}", expected.file_key().as_str(), COMPANION_SUFFIX)
    }

    fn companion_tombstone_name(expected: &DatabaseFileExpectation) -> String {
        format!(
            ".turso-mysql-companion-tombstone-{}",
            expected.file_key().as_str()
        )
    }

    fn inspect_named_database(
        &self,
        name: &str,
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        let Some(file) = self.open_child_optional(name, libc::O_RDONLY)? else {
            return Ok(DatabaseFileInspection::Missing);
        };
        Self::inspect_open_database(&file, expected, role)
    }

    fn unlink_tombstone_if_matching(
        &mut self,
        tombstone: &str,
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Result<(), RegistryError> {
        let Some(file) = self.open_child_optional(tombstone, libc::O_RDONLY)? else {
            return Ok(());
        };
        if Self::inspect_open_database(&file, expected, role)? != DatabaseFileInspection::Matching {
            return Err(RegistryError::Backend);
        }
        self.unlink_if_present(tombstone)?;
        self.fsync_dir()
    }

    fn unlink_database_artifact(
        &mut self,
        name: &str,
        tombstone: &str,
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Result<(), RegistryError> {
        let Some(file) = self.open_child_optional(name, libc::O_RDONLY)? else {
            return self.unlink_tombstone_if_matching(tombstone, expected, role);
        };
        if Self::inspect_open_database(&file, expected, role)? != DatabaseFileInspection::Matching {
            return Err(RegistryError::Backend);
        }
        let identity = Self::file_identity(&file)?;
        match self.inspect_named_database(tombstone, expected, role)? {
            DatabaseFileInspection::Missing => {}
            DatabaseFileInspection::Partial
            | DatabaseFileInspection::Matching
            | DatabaseFileInspection::Mismatch => return Err(RegistryError::Backend),
        }
        self.rename_child(name, tombstone)?;
        self.fsync_dir()?;

        let Some(tombstone_file) = self.open_child_optional(tombstone, libc::O_RDONLY)? else {
            return Err(RegistryError::Backend);
        };
        if Self::inspect_open_database(&tombstone_file, expected, role)?
            != DatabaseFileInspection::Matching
            || Self::file_identity(&tombstone_file)? != identity
        {
            return Err(RegistryError::Backend);
        }
        self.unlink_if_present(tombstone)?;
        self.fsync_dir()
    }

    fn preflight_database_artifact(
        &self,
        name: &str,
        tombstone: &str,
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Result<(), RegistryError> {
        let named = self.inspect_named_database(name, expected, role)?;
        let tombstoned = self.inspect_named_database(tombstone, expected, role)?;
        match (named, tombstoned) {
            (DatabaseFileInspection::Missing, DatabaseFileInspection::Missing)
            | (DatabaseFileInspection::Matching, DatabaseFileInspection::Missing)
            | (DatabaseFileInspection::Missing, DatabaseFileInspection::Matching) => Ok(()),
            _ => Err(RegistryError::Backend),
        }
    }

    fn write_new_synced(&self, name: &str, bytes: &[u8]) -> Result<(), RegistryError> {
        let mut file =
            self.open_child(name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
        file.write_all(bytes).map_err(|_| RegistryError::Backend)?;
        file.sync_all().map_err(|_| RegistryError::Backend)
    }

    fn unlink_if_present(&self, name: &str) -> Result<(), RegistryError> {
        self.unlink_if_present_bytes(name.as_bytes())
    }

    fn unlink_if_present_bytes(&self, name: &[u8]) -> Result<(), RegistryError> {
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn unlink_private_temporary_if_present(&mut self, name: &[u8]) -> Result<bool, RegistryError> {
        #[cfg(test)]
        if let Some(hook) = &mut self.private_temporary_cleanup_test_hook {
            hook.unlink_attempts += 1;
            if hook.unlink_attempts == hook.fail_unlink_at_attempt {
                return Err(RegistryError::Backend);
            }
        }
        let name = CString::new(name).map_err(|_| RegistryError::Backend)?;
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(true)
        } else if matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT) | Some(libc::EISDIR) | Some(libc::ELOOP) | Some(libc::EPERM)
        ) {
            Ok(false)
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn rename_child(&self, from: &str, to: &str) -> Result<(), RegistryError> {
        let from = CString::new(from.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let to = CString::new(to.as_bytes()).map_err(|_| RegistryError::Backend)?;
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
            Err(RegistryError::Backend)
        }
    }

    fn publish_child_new(&self, from: &str, to: &str) -> Result<(), RegistryError> {
        let from = CString::new(from.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let to = CString::new(to.as_bytes()).map_err(|_| RegistryError::Backend)?;
        let result = unsafe {
            libc::linkat(
                self.directory.as_raw_fd(),
                from.as_ptr(),
                self.directory.as_raw_fd(),
                to.as_ptr(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }

    fn publish_staged_child_new(
        &self,
        temporary: &str,
        stage_file: &File,
        final_name: &str,
    ) -> Result<(), RegistryError> {
        // This detects a replaced private name before it is made visible. The
        // private root and advisory registry lock still define the same-UID
        // cooperative-writer trust boundary; this is not a complete defense
        // against a non-cooperating writer racing every system call.
        let stage_identity = Self::file_identity(stage_file)?;
        let temporary_file = self.open_child(temporary, libc::O_RDONLY, 0)?;
        if Self::file_identity(&temporary_file)? != stage_identity {
            return Err(RegistryError::Backend);
        }

        self.publish_child_new(temporary, final_name)?;

        let final_file = self.open_child(final_name, libc::O_RDONLY, 0)?;
        if Self::file_identity(&final_file)? != stage_identity {
            // Do not guess whether this name is ours: recovery will only remove
            // files whose envelope and identity match the Creating record.
            return Err(RegistryError::Backend);
        }

        self.unlink_if_present(temporary)
    }

    fn database_envelope(
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> Vec<u8> {
        let marker = expected.marker();
        let mut bytes = Vec::with_capacity(DATABASE_ENVELOPE_BYTES);
        bytes.extend_from_slice(DATABASE_ENVELOPE_MAGIC);
        bytes.push(marker.version);
        bytes.push(match marker.owner {
            FrontendOwner::MySql => 1,
        });
        bytes.push(marker.lower_case_table_names);
        bytes.push(marker.reserved_bits);
        bytes.push(role.as_byte());
        bytes.extend_from_slice(expected.file_key().as_str().as_bytes());
        bytes
    }

    fn envelope_matches(
        bytes: &[u8],
        expected: &DatabaseFileExpectation,
        role: DatabaseArtifactRole,
    ) -> bool {
        if bytes.len() != DATABASE_ENVELOPE_BYTES || !bytes.starts_with(DATABASE_ENVELOPE_MAGIC) {
            return false;
        }
        let mut offset = DATABASE_ENVELOPE_MAGIC.len();
        let marker = MySqlOwnerMarkerV2 {
            version: bytes[offset],
            owner: match bytes[offset + 1] {
                1 => FrontendOwner::MySql,
                _ => return false,
            },
            lower_case_table_names: bytes[offset + 2],
            reserved_bits: bytes[offset + 3],
        };
        if bytes[offset + 4] != role.as_byte() {
            return false;
        }
        offset += 5;
        let Ok(key) = std::str::from_utf8(&bytes[offset..]) else {
            return false;
        };
        marker == expected.marker()
            && marker.validate_for_policy(NamePolicy::LowerCaseTableNames1)
            && key == expected.file_key().as_str()
    }

    fn next_private_name(prefix: &str) -> Result<String, RegistryError> {
        let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|_| RegistryError::Backend)?;
        let mut suffix = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut suffix, "{byte:02x}").map_err(|_| RegistryError::Backend)?;
        }
        Ok(format!(
            ".turso-mysql-{}.{}.{}.{}",
            prefix,
            std::process::id(),
            id,
            suffix
        ))
    }
}

impl RegistryRoot for OsDataRoot {
    type RegistryLock = OsRegistryLock;
    type DatabaseHandle = OsDatabaseHandle;
    type DatabaseStage = OsDatabaseStage;

    fn acquire_exclusive_registry_lock(&mut self) -> Result<Self::RegistryLock, RegistryError> {
        let lock = self.open_child(LOCK_FILE, libc::O_RDWR | libc::O_CREAT, 0o600)?;
        loop {
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if matches!(
                error.raw_os_error(),
                Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
            ) {
                return Err(RegistryError::RegistryAlreadyOpen);
            }
            return Err(RegistryError::Backend);
        }
        self.collect_private_temporary_files()?;
        Ok(OsRegistryLock(Arc::new(lock)))
    }

    fn read_manifest(&mut self) -> Result<Option<RootManifest>, RegistryError> {
        self.read_json(MANIFEST_FILE, MAX_MANIFEST_BYTES)
    }

    fn create_manifest_new(&mut self, manifest: &RootManifest) -> Result<(), RegistryError> {
        manifest.validate()?;
        let bytes = serde_json::to_vec(manifest).map_err(|_| RegistryError::Backend)?;
        self.write_new_synced(MANIFEST_FILE, &bytes)
    }

    fn read_registry(&mut self) -> Result<Option<RegistrySnapshot>, RegistryError> {
        let Some(registry) = self.read_json(REGISTRY_FILE, MAX_REGISTRY_BYTES)? else {
            return Ok(None);
        };
        Ok(Some(registry))
    }

    fn replace_registry(&mut self, registry: &RegistrySnapshot) -> Result<(), RegistryError> {
        let bytes = serde_json::to_vec(registry).map_err(|_| RegistryError::Backend)?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(RegistryError::Backend);
        }
        let temporary = Self::next_private_name("registry.tmp")?;
        if let Err(error) = self.write_new_synced(&temporary, &bytes) {
            let _ = self.unlink_if_present(&temporary);
            return Err(error);
        }
        if let Err(error) = self.rename_child(&temporary, REGISTRY_FILE) {
            // The temporary is ours, and the destination was not touched by a
            // failed rename. Cleanup failure must not hide the write failure.
            let _ = self.unlink_if_present(&temporary);
            return Err(error);
        }
        self.fsync_dir()
    }

    fn allocate_file_key(&mut self) -> Result<OpaqueFileKey, RegistryError> {
        loop {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).map_err(|_| RegistryError::Backend)?;
            if random.iter().all(|byte| *byte == 0) {
                continue;
            }
            let mut key = String::with_capacity(35);
            key.push_str("db_");
            for byte in random {
                use std::fmt::Write as _;
                write!(&mut key, "{byte:02x}").map_err(|_| RegistryError::Backend)?;
            }
            return OpaqueFileKey::new(key);
        }
    }

    fn inspect_database_creation(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        let names = [
            (
                expected.file_key().as_str().to_string(),
                DatabaseArtifactRole::Main,
            ),
            (
                Self::companion_name(expected),
                DatabaseArtifactRole::Companion,
            ),
            (Self::tombstone_name(expected), DatabaseArtifactRole::Main),
            (
                Self::companion_tombstone_name(expected),
                DatabaseArtifactRole::Companion,
            ),
        ];
        for (name, role) in names {
            if self.inspect_named_database(&name, expected, role)?
                != DatabaseFileInspection::Missing
            {
                return Ok(DatabaseFileInspection::Mismatch);
            }
        }
        Ok(DatabaseFileInspection::Missing)
    }

    fn stage_database_new(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<Self::DatabaseStage, RegistryError> {
        let main = Self::database_envelope(expected, DatabaseArtifactRole::Main);
        let companion = Self::database_envelope(expected, DatabaseArtifactRole::Companion);
        let main_temporary = Self::next_private_name("database-main.tmp")?;
        let companion_temporary = Self::next_private_name("database-companion.tmp")?;
        let mut main_file = self.open_child(
            &main_temporary,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        if main_file.write_all(&main).is_err() {
            return match self.unlink_if_present(&main_temporary) {
                Ok(()) => Err(RegistryError::Backend),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        let mut companion_file = match self.open_child(
            &companion_temporary,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) => {
                let cleanup = self.unlink_if_present(&main_temporary);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(cleanup_error),
                };
            }
        };
        if companion_file.write_all(&companion).is_err() {
            let cleanup_main = self.unlink_if_present(&main_temporary);
            let cleanup_companion = self.unlink_if_present(&companion_temporary);
            return match (cleanup_main, cleanup_companion) {
                (Ok(()), Ok(())) => Err(RegistryError::Backend),
                (Err(cleanup_error), _) | (_, Err(cleanup_error)) => Err(cleanup_error),
            };
        }
        Ok(OsDatabaseStage {
            main_file,
            companion_file,
            main_temporary,
            companion_temporary,
        })
    }

    fn sync_database_stage(&mut self, stage: &Self::DatabaseStage) -> Result<(), RegistryError> {
        stage
            .main_file
            .sync_all()
            .map_err(|_| RegistryError::Backend)?;
        stage
            .companion_file
            .sync_all()
            .map_err(|_| RegistryError::Backend)
    }

    fn publish_database_stage_new(
        &mut self,
        expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError> {
        self.publish_staged_child_new(
            &stage.main_temporary,
            &stage.main_file,
            expected.file_key().as_str(),
        )?;
        self.publish_staged_child_new(
            &stage.companion_temporary,
            &stage.companion_file,
            &Self::companion_name(expected),
        )
    }

    fn abort_database_stage(
        &mut self,
        _expected: &DatabaseFileExpectation,
        stage: Self::DatabaseStage,
    ) -> Result<(), RegistryError> {
        self.unlink_if_present(&stage.main_temporary)?;
        self.unlink_if_present(&stage.companion_temporary)
    }

    fn inspect_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<DatabaseFileInspection, RegistryError> {
        let main = self
            .open_child_optional(expected.file_key().as_str(), libc::O_RDONLY)?
            .map(|file| Self::inspect_open_database(&file, expected, DatabaseArtifactRole::Main))
            .transpose()?;
        let companion_name = Self::companion_name(expected);
        let companion = self
            .open_child_optional(&companion_name, libc::O_RDONLY)?
            .map(|file| {
                Self::inspect_open_database(&file, expected, DatabaseArtifactRole::Companion)
            })
            .transpose()?;
        Ok(match (main, companion) {
            (None, None) => DatabaseFileInspection::Missing,
            (Some(DatabaseFileInspection::Matching), Some(DatabaseFileInspection::Matching)) => {
                DatabaseFileInspection::Matching
            }
            (Some(DatabaseFileInspection::Matching), None)
            | (None, Some(DatabaseFileInspection::Matching)) => DatabaseFileInspection::Partial,
            _ => DatabaseFileInspection::Mismatch,
        })
    }

    fn open_database(
        &mut self,
        expected: &DatabaseFileExpectation,
    ) -> Result<OpenDatabaseInspection<Self::DatabaseHandle>, RegistryError> {
        let main = self.open_child_optional(expected.file_key().as_str(), libc::O_RDWR)?;
        let companion_name = Self::companion_name(expected);
        let companion = self.open_child_optional(&companion_name, libc::O_RDWR)?;
        match (main, companion) {
            (None, None) => Ok(OpenDatabaseInspection::Missing),
            (Some(main_file), Some(companion_file))
                if Self::inspect_open_database(
                    &main_file,
                    expected,
                    DatabaseArtifactRole::Main,
                )? == DatabaseFileInspection::Matching
                    && Self::inspect_open_database(
                        &companion_file,
                        expected,
                        DatabaseArtifactRole::Companion,
                    )? == DatabaseFileInspection::Matching =>
            {
                Ok(OpenDatabaseInspection::Matching(OsDatabaseHandle {
                    main_file,
                    companion_file,
                    identity: expected.file_key().clone(),
                }))
            }
            _ => Ok(OpenDatabaseInspection::Mismatch),
        }
    }

    fn unlink_database(&mut self, expected: &DatabaseFileExpectation) -> Result<(), RegistryError> {
        let main_tombstone = Self::tombstone_name(expected);
        let companion_name = Self::companion_name(expected);
        let companion_tombstone = Self::companion_tombstone_name(expected);
        // Validate the complete bundle before removing either artifact. A
        // corrupt companion tombstone must not turn an otherwise recoverable
        // drop into a half-deleted database.
        self.preflight_database_artifact(
            expected.file_key().as_str(),
            &main_tombstone,
            expected,
            DatabaseArtifactRole::Main,
        )?;
        self.preflight_database_artifact(
            &companion_name,
            &companion_tombstone,
            expected,
            DatabaseArtifactRole::Companion,
        )?;
        self.unlink_database_artifact(
            expected.file_key().as_str(),
            &main_tombstone,
            expected,
            DatabaseArtifactRole::Main,
        )?;
        self.unlink_database_artifact(
            &companion_name,
            &companion_tombstone,
            expected,
            DatabaseArtifactRole::Companion,
        )
    }

    fn fsync_dir(&mut self) -> Result<(), RegistryError> {
        #[cfg(test)]
        if let Some(hook) = &mut self.private_temporary_cleanup_test_hook {
            hook.fsync_attempts += 1;
        }
        let result = unsafe { libc::fsync(self.directory.as_raw_fd()) };
        if result == 0 {
            Ok(())
        } else {
            Err(RegistryError::Backend)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn expected(key: &str) -> DatabaseFileExpectation {
        DatabaseFileExpectation::new(
            OpaqueFileKey::new(key.to_owned()).unwrap(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        )
    }

    fn create_database_new(
        root: &mut OsDataRoot,
        expected: &DatabaseFileExpectation,
    ) -> Result<(), RegistryError> {
        let stage = root.stage_database_new(expected)?;
        root.sync_database_stage(&stage)?;
        root.publish_database_stage_new(expected, stage)?;
        root.fsync_dir()
    }

    #[test]
    fn uses_relative_files_and_round_trips_durable_state() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let manifest = RootManifest::lower_case_table_names_1();
        root.create_manifest_new(&manifest).unwrap();
        root.fsync_dir().unwrap();
        assert_eq!(root.read_manifest().unwrap(), Some(manifest));

        let entry = expected("db_00000000000000000000000000000001");
        create_database_new(&mut root, &entry).unwrap();
        root.fsync_dir().unwrap();
        assert!(directory.path().join(entry.file_key().as_str()).is_file());
        assert!(directory
            .path()
            .join(OsDataRoot::companion_name(&entry))
            .is_file());
        assert_eq!(
            root.inspect_database(&entry).unwrap(),
            DatabaseFileInspection::Matching
        );
        let registry = RegistrySnapshot {
            entries: [(
                DatabaseName::parse("app").unwrap(),
                RegistryEntry {
                    file_key: entry.file_key().clone(),
                    state: DatabaseState::Ready,
                },
            )]
            .into_iter()
            .collect(),
        };
        root.replace_registry(&registry).unwrap();
        assert_eq!(root.read_registry().unwrap(), Some(registry));
    }

    #[test]
    fn staged_descriptors_are_the_inodes_published_under_final_names() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000017");
        let stage = root.stage_database_new(&entry).unwrap();
        let main_identity = OsDataRoot::file_identity(stage.main_file()).unwrap();
        let companion_identity = OsDataRoot::file_identity(stage.companion_file()).unwrap();
        assert!(!directory.path().join(entry.file_key().as_str()).exists());
        assert!(!directory
            .path()
            .join(OsDataRoot::companion_name(&entry))
            .exists());

        root.sync_database_stage(&stage).unwrap();
        root.publish_database_stage_new(&entry, stage).unwrap();
        root.fsync_dir().unwrap();

        let OpenDatabaseInspection::Matching(handle) = root.open_database(&entry).unwrap() else {
            panic!("published stage must open as a matching bundle");
        };
        assert_eq!(
            OsDataRoot::file_identity(handle.main_file()).unwrap(),
            main_identity
        );
        assert_eq!(
            OsDataRoot::file_identity(handle.companion_file()).unwrap(),
            companion_identity
        );
    }

    #[test]
    fn opened_bundle_retains_writable_main_and_companion_descriptors() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000010");
        create_database_new(&mut root, &entry).unwrap();

        let OpenDatabaseInspection::Matching(handle) = root.open_database(&entry).unwrap() else {
            panic!("created database must open as a matching bundle");
        };
        assert_eq!(handle.identity(), entry.file_key());
        for file in [handle.main_file(), handle.companion_file()] {
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            assert_eq!(flags & libc::O_ACCMODE, libc::O_RDWR);
        }
        assert_ne!(
            OsDataRoot::file_identity(handle.main_file()).unwrap(),
            OsDataRoot::file_identity(handle.companion_file()).unwrap()
        );
    }

    #[test]
    fn swapped_main_and_companion_roles_are_rejected() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000013");
        create_database_new(&mut root, &entry).unwrap();

        let main = directory.path().join(entry.file_key().as_str());
        let companion = directory.path().join(OsDataRoot::companion_name(&entry));
        let temporary = directory.path().join("swap-temporary");
        fs::rename(&main, &temporary).unwrap();
        fs::rename(&companion, &main).unwrap();
        fs::rename(&temporary, &companion).unwrap();

        assert_eq!(
            root.inspect_database(&entry).unwrap(),
            DatabaseFileInspection::Mismatch
        );
        assert!(matches!(
            root.open_database(&entry),
            Ok(OpenDatabaseInspection::Mismatch)
        ));
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        assert!(main.exists());
        assert!(companion.exists());
    }

    #[test]
    fn missing_companion_is_partial_and_is_removed_by_lifecycle_cleanup() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000011");
        create_database_new(&mut root, &entry).unwrap();
        let companion_name = OsDataRoot::companion_name(&entry);
        fs::remove_file(directory.path().join(&companion_name)).unwrap();

        assert_eq!(
            root.inspect_database(&entry).unwrap(),
            DatabaseFileInspection::Partial
        );
        assert!(matches!(
            root.open_database(&entry),
            Ok(OpenDatabaseInspection::Mismatch)
        ));
        root.unlink_database(&entry).unwrap();
        assert!(!directory.path().join(entry.file_key().as_str()).exists());
        assert!(!directory.path().join(companion_name).exists());
    }

    #[test]
    fn publish_rejects_a_regular_file_replacing_the_main_stage_path() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        let retained_stage_file = directory.path().join("retained-main-stage");
        let mut replacement = None;

        assert_eq!(
            registry.create_with_initializer(
                "replaced_main",
                |stage, _, lifetime| -> Result<(), RegistryError> {
                    drop(lifetime);
                    let temporary = directory.path().join(&stage.main_temporary);
                    fs::rename(&temporary, &retained_stage_file).unwrap();
                    fs::write(&temporary, b"unrelated replacement").unwrap();
                    replacement = Some(temporary);
                    Ok(())
                }
            ),
            Err(RegistryError::Backend)
        );
        let name = DatabaseName::parse("replaced_main").unwrap();
        let entry = &registry.snapshot.entries[&name];
        assert_eq!(entry.state, DatabaseState::Creating);
        assert_eq!(
            fs::read(replacement.expect("initializer must record replacement")).unwrap(),
            b"unrelated replacement"
        );
        assert!(retained_stage_file.exists());
        assert!(!directory.path().join(entry.file_key.as_str()).exists());
    }

    #[test]
    fn publish_rejects_a_symlink_replacing_the_companion_stage_path() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let target = outside.path().join("unrelated-target");
        fs::write(&target, b"keep").unwrap();

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000019");
        let stage = root.stage_database_new(&entry).unwrap();
        let temporary = directory.path().join(&stage.companion_temporary);
        let retained_stage_file = directory.path().join("retained-companion-stage");
        fs::rename(&temporary, &retained_stage_file).unwrap();
        symlink(&target, &temporary).unwrap();

        assert_eq!(
            root.publish_database_stage_new(&entry, stage),
            Err(RegistryError::Backend)
        );
        assert!(directory.path().join(entry.file_key().as_str()).exists());
        assert!(!directory
            .path()
            .join(OsDataRoot::companion_name(&entry))
            .exists());
        assert!(temporary.is_symlink());
        assert!(retained_stage_file.exists());
        assert_eq!(fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn reopening_recovers_an_interrupted_create_with_a_missing_companion() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        let name = registry.create("partial_create").unwrap();
        let expected = DatabaseFileExpectation::new(
            registry.snapshot.entries[&name].file_key.clone(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        );
        let companion = OsDataRoot::companion_name(&expected);
        fs::remove_file(directory.path().join(&companion)).unwrap();
        registry.snapshot.entries.get_mut(&name).unwrap().state = DatabaseState::Creating;
        registry.persist_snapshot().unwrap();
        drop(registry);

        let root = OsDataRoot::open(directory.path()).unwrap();
        let registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(!registry.contains(name.as_str()).unwrap());
        assert!(!directory.path().join(expected.file_key().as_str()).exists());
        assert!(!directory.path().join(companion).exists());
    }

    #[test]
    fn identity_mismatch_is_never_unlinked() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let actual = expected("db_00000000000000000000000000000002");
        let wrong = expected("db_00000000000000000000000000000003");
        create_database_new(&mut root, &actual).unwrap();
        assert_eq!(
            root.inspect_database(&wrong).unwrap(),
            DatabaseFileInspection::Missing
        );
        let swapped = expected("db_00000000000000000000000000000002");
        let bytes = OsDataRoot::database_envelope(&wrong, DatabaseArtifactRole::Main);
        let path = directory.path().join(swapped.file_key().as_str());
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            root.inspect_database(&actual).unwrap(),
            DatabaseFileInspection::Mismatch
        );
        assert_eq!(root.unlink_database(&actual), Err(RegistryError::Backend));
        assert!(path.exists());
    }

    #[test]
    fn root_lock_is_exclusive_across_directory_capabilities() {
        let directory = private_tempdir();
        let mut first = OsDataRoot::open(directory.path()).unwrap();
        let mut second = OsDataRoot::open(directory.path()).unwrap();
        let lock = first.acquire_exclusive_registry_lock().unwrap();
        assert!(matches!(
            second.acquire_exclusive_registry_lock(),
            Err(RegistryError::RegistryAlreadyOpen)
        ));
        drop(lock);
        assert!(second.acquire_exclusive_registry_lock().is_ok());
    }

    #[test]
    fn stale_private_temporary_files_are_collected_when_the_root_lock_is_acquired() {
        let directory = private_tempdir();
        let names = [
            ".turso-mysql-registry.tmp.123.1.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-main.tmp.123.2.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-companion.tmp.123.3.0123456789abcdef0123456789abcdef",
        ];
        for name in names {
            let file = File::create(directory.path().join(name)).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
                .unwrap();
        }

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let _lock = root.acquire_exclusive_registry_lock().unwrap();
        for name in names {
            assert!(!directory.path().join(name).exists(), "{name}");
        }
    }

    #[test]
    fn partial_private_temporary_cleanup_syncs_before_returning_an_unlink_error() {
        let directory = private_tempdir();
        let names = [
            ".turso-mysql-registry.tmp.123.11.0123456789abcdef0123456789abcdef",
            ".turso-mysql-database-main.tmp.123.12.0123456789abcdef0123456789abcdef",
        ];
        for name in names {
            let file = File::create(directory.path().join(name)).unwrap();
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
                .unwrap();
        }

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.private_temporary_cleanup_test_hook = Some(PrivateTemporaryCleanupTestHook {
            fail_unlink_at_attempt: 2,
            unlink_attempts: 0,
            fsync_attempts: 0,
        });

        assert!(matches!(
            root.acquire_exclusive_registry_lock(),
            Err(RegistryError::Backend)
        ));
        let hook = root.private_temporary_cleanup_test_hook.as_ref().unwrap();
        assert_eq!(hook.unlink_attempts, 2);
        assert_eq!(hook.fsync_attempts, 1);
        assert_eq!(
            names
                .iter()
                .filter(|name| directory.path().join(name).exists())
                .count(),
            1
        );
    }

    #[test]
    fn private_temporary_cleanup_preserves_foreign_malformed_and_symlink_entries() {
        let directory = private_tempdir();
        let outside = private_tempdir();
        let target = outside.path().join("keep");
        fs::write(&target, b"keep").unwrap();

        let fresh = ".turso-mysql-registry.tmp.123.4.0123456789abcdef0123456789abcdef";
        File::create(directory.path().join(fresh)).unwrap();
        let foreign = ".turso-mysql-foreign.tmp.123.5.0123456789abcdef0123456789abcdef";
        let foreign_path = directory.path().join(foreign);
        let foreign_file = File::create(&foreign_path).unwrap();
        foreign_file
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let malformed = ".turso-mysql-registry.tmp.0123.6.0123456789abcdef0123456789abcdef";
        let malformed_path = directory.path().join(malformed);
        let malformed_file = File::create(&malformed_path).unwrap();
        malformed_file
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();

        let symlink_name = ".turso-mysql-database-main.tmp.123.7.0123456789abcdef0123456789abcdef";
        let symlink_path = directory.path().join(symlink_name);
        symlink(&target, &symlink_path).unwrap();
        let directory_name =
            ".turso-mysql-database-companion.tmp.123.8.0123456789abcdef0123456789abcdef";
        fs::create_dir(directory.path().join(directory_name)).unwrap();

        let mut root = OsDataRoot::open(directory.path()).unwrap();
        let _lock = root.acquire_exclusive_registry_lock().unwrap();
        assert!(directory.path().join(fresh).exists());
        assert!(foreign_path.is_file());
        assert!(malformed_path.is_file());
        assert!(symlink_path.is_symlink());
        assert!(directory.path().join(directory_name).is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn root_symlink_is_not_accepted_as_a_capability() {
        let parent = private_tempdir();
        let target = tempfile::tempdir().unwrap();
        let link = parent.path().join("root-link");
        symlink(target.path(), &link).unwrap();
        assert!(matches!(
            OsDataRoot::open(&link),
            Err(RegistryError::Backend)
        ));
    }

    #[test]
    fn root_must_be_private_to_the_server_owner() {
        let directory = private_tempdir();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            OsDataRoot::open(directory.path()),
            Err(RegistryError::Backend)
        ));
    }

    #[test]
    fn existing_manifest_without_registry_fails_closed() {
        let directory = private_tempdir();
        {
            let mut root = OsDataRoot::open(directory.path()).unwrap();
            root.acquire_exclusive_registry_lock().unwrap();
            root.create_manifest_new(&RootManifest::lower_case_table_names_1())
                .unwrap();
            root.fsync_dir().unwrap();
        }
        let root = OsDataRoot::open(directory.path()).unwrap();
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::Backend)
        ));
        assert!(!directory.path().join(REGISTRY_FILE).exists());
    }

    #[test]
    fn existing_registry_without_manifest_fails_closed() {
        let directory = private_tempdir();
        {
            let mut root = OsDataRoot::open(directory.path()).unwrap();
            root.acquire_exclusive_registry_lock().unwrap();
            root.replace_registry(&RegistrySnapshot::default()).unwrap();
        }
        let root = OsDataRoot::open(directory.path()).unwrap();
        assert!(matches!(
            DatabaseRegistry::open_or_create(root),
            Err(RegistryError::Backend)
        ));
    }

    #[test]
    fn second_open_reads_the_durable_registry() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut first = DatabaseRegistry::open_or_create(root).unwrap();
        first.create("App").unwrap();
        drop(first);

        let root = OsDataRoot::open(directory.path()).unwrap();
        let second = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(second.contains("app").unwrap());
    }

    #[test]
    fn dropping_tombstone_is_recovered_after_a_crash_before_unlink() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        let name = registry.create("crash_safe").unwrap();
        let file_key = registry.snapshot.entries[&name].file_key.clone();
        registry.snapshot.entries.get_mut(&name).unwrap().state = DatabaseState::Dropping;
        registry.persist_snapshot().unwrap();
        let expected = DatabaseFileExpectation::new(
            file_key,
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        );
        let tombstone = OsDataRoot::tombstone_name(&expected);
        registry
            .root
            .rename_child(expected.file_key().as_str(), &tombstone)
            .unwrap();
        registry.root.fsync_dir().unwrap();
        drop(registry);

        let root = OsDataRoot::open(directory.path()).unwrap();
        let registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(!registry.contains(name.as_str()).unwrap());
        assert!(!directory.path().join(&tombstone).exists());
        assert!(!directory.path().join(expected.file_key().as_str()).exists());
    }

    #[test]
    fn foreign_dropping_tombstone_is_never_unlinked() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let owned = expected("db_00000000000000000000000000000007");
        let foreign = expected("db_00000000000000000000000000000008");
        create_database_new(&mut root, &owned).unwrap();
        let tombstone = OsDataRoot::tombstone_name(&owned);
        root.rename_child(owned.file_key().as_str(), &tombstone)
            .unwrap();
        fs::write(
            directory.path().join(&tombstone),
            OsDataRoot::database_envelope(&foreign, DatabaseArtifactRole::Main),
        )
        .unwrap();
        assert_eq!(root.unlink_database(&owned), Err(RegistryError::Backend));
        assert!(directory.path().join(&tombstone).exists());
    }

    #[test]
    fn foreign_companion_tombstone_is_rejected_before_main_is_unlinked() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let owned = expected("db_00000000000000000000000000000014");
        let foreign = expected("db_00000000000000000000000000000015");
        create_database_new(&mut root, &owned).unwrap();
        let companion_tombstone = OsDataRoot::companion_tombstone_name(&owned);
        fs::write(
            directory.path().join(&companion_tombstone),
            OsDataRoot::database_envelope(&foreign, DatabaseArtifactRole::Companion),
        )
        .unwrap();

        assert_eq!(root.unlink_database(&owned), Err(RegistryError::Backend));
        assert!(directory.path().join(owned.file_key().as_str()).exists());
        assert!(directory
            .path()
            .join(OsDataRoot::companion_name(&owned))
            .exists());
        assert!(directory.path().join(companion_tombstone).exists());
    }

    #[test]
    fn create_preflight_rejects_stale_lifecycle_tombstones() {
        let directory = private_tempdir();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        let entry = expected("db_00000000000000000000000000000016");
        let tombstone = OsDataRoot::companion_tombstone_name(&entry);
        fs::write(
            directory.path().join(&tombstone),
            OsDataRoot::database_envelope(&entry, DatabaseArtifactRole::Companion),
        )
        .unwrap();

        assert_eq!(
            root.inspect_database_creation(&entry).unwrap(),
            DatabaseFileInspection::Mismatch
        );
        assert!(!directory.path().join(entry.file_key().as_str()).exists());
        assert!(!directory
            .path()
            .join(OsDataRoot::companion_name(&entry))
            .exists());
        assert!(directory.path().join(tombstone).exists());
    }

    #[test]
    fn symlink_database_is_rejected_without_touching_target() {
        let directory = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("not-owned");
        fs::write(&target, b"keep").unwrap();
        let entry = expected("db_00000000000000000000000000000004");
        symlink(&target, directory.path().join(entry.file_key().as_str())).unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        assert_eq!(root.inspect_database(&entry), Err(RegistryError::Backend));
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn symlink_companion_is_rejected_without_touching_target() {
        let directory = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("not-owned-companion");
        fs::write(&target, b"keep").unwrap();
        let entry = expected("db_00000000000000000000000000000012");
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        create_database_new(&mut root, &entry).unwrap();
        let companion_name = OsDataRoot::companion_name(&entry);
        fs::remove_file(directory.path().join(&companion_name)).unwrap();
        symlink(&target, directory.path().join(&companion_name)).unwrap();

        assert_eq!(root.inspect_database(&entry), Err(RegistryError::Backend));
        assert!(matches!(
            root.open_database(&entry),
            Err(RegistryError::Backend)
        ));
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        assert_eq!(fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn registry_uses_atomic_replacement_not_destination_contents() {
        let directory = private_tempdir();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside-registry");
        fs::write(&target, b"outside").unwrap();
        symlink(&target, directory.path().join(REGISTRY_FILE)).unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.replace_registry(&RegistrySnapshot::default()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        assert!(directory.path().join(REGISTRY_FILE).is_file());
    }

    #[test]
    fn database_registry_completes_a_create_acquire_drop_cycle() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        assert!(directory.path().join(REGISTRY_FILE).is_file());
        let name = registry.create("App").unwrap();
        let expected = DatabaseFileExpectation::new(
            registry.snapshot.entries[&name].file_key.clone(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        );
        let lease = registry.acquire(name.as_str()).unwrap();
        registry.release(lease).unwrap();
        registry.drop_database(name.as_str()).unwrap();
        assert!(!registry.contains(name.as_str()).unwrap());
        assert!(!directory.path().join(expected.file_key().as_str()).exists());
        assert!(!directory
            .path()
            .join(OsDataRoot::companion_name(&expected))
            .exists());
    }

    #[test]
    fn reopening_registry_acquires_a_fresh_complete_bundle() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut first = DatabaseRegistry::open_or_create(root).unwrap();
        let name = first.create("reopen").unwrap();
        let key = first.snapshot.entries[&name].file_key.clone();
        drop(first);

        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut second = DatabaseRegistry::open_or_create(root).unwrap();
        let lease = second.acquire(name.as_str()).unwrap();
        let expected = DatabaseFileExpectation::new(
            key.clone(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        );
        assert_eq!(lease.database_handle().identity(), &key);
        assert_eq!(
            OsDataRoot::inspect_open_database(
                lease.database_handle().main_file(),
                &expected,
                DatabaseArtifactRole::Main,
            )
            .unwrap(),
            DatabaseFileInspection::Matching
        );
        assert_eq!(
            OsDataRoot::inspect_open_database(
                lease.database_handle().companion_file(),
                &expected,
                DatabaseArtifactRole::Companion,
            )
            .unwrap(),
            DatabaseFileInspection::Matching
        );
        second.release(lease).unwrap();
    }

    #[test]
    fn acquired_lease_keeps_the_inspected_file_after_name_replacement() {
        let directory = private_tempdir();
        let root = OsDataRoot::open(directory.path()).unwrap();
        let mut registry = DatabaseRegistry::open_or_create(root).unwrap();
        let name = registry.create("orders").unwrap();
        let owned = DatabaseFileExpectation::new(
            registry.snapshot.entries[&name].file_key.clone(),
            MySqlOwnerMarkerV2::for_policy(NamePolicy::LowerCaseTableNames1),
        );
        let lease = registry.acquire(name.as_str()).unwrap();
        let original_identity =
            OsDataRoot::file_identity(lease.database_handle().main_file()).unwrap();
        let original_companion_identity =
            OsDataRoot::file_identity(lease.database_handle().companion_file()).unwrap();

        let replacement = directory.path().join("replacement");
        let foreign = expected("db_00000000000000000000000000000009");
        fs::write(
            &replacement,
            OsDataRoot::database_envelope(&foreign, DatabaseArtifactRole::Main),
        )
        .unwrap();
        let named_file = directory.path().join(owned.file_key().as_str());
        fs::rename(&replacement, &named_file).unwrap();

        let companion_replacement = directory.path().join("companion-replacement");
        fs::write(
            &companion_replacement,
            OsDataRoot::database_envelope(&foreign, DatabaseArtifactRole::Companion),
        )
        .unwrap();
        let named_companion = directory.path().join(OsDataRoot::companion_name(&owned));
        fs::rename(&companion_replacement, &named_companion).unwrap();

        let replacement_metadata = fs::metadata(&named_file).unwrap();
        assert_ne!(
            original_identity,
            FileIdentity {
                device: replacement_metadata.dev(),
                inode: replacement_metadata.ino(),
            }
        );
        assert_eq!(
            OsDataRoot::inspect_open_database(
                lease.database_handle().main_file(),
                &owned,
                DatabaseArtifactRole::Main,
            )
            .unwrap(),
            DatabaseFileInspection::Matching
        );
        assert_eq!(
            OsDataRoot::inspect_open_database(
                lease.database_handle().companion_file(),
                &owned,
                DatabaseArtifactRole::Companion,
            )
            .unwrap(),
            DatabaseFileInspection::Matching
        );
        assert_eq!(
            OsDataRoot::file_identity(lease.database_handle().companion_file()).unwrap(),
            original_companion_identity
        );
        assert_eq!(
            registry.root.inspect_database(&owned).unwrap(),
            DatabaseFileInspection::Mismatch
        );
        assert!(matches!(
            registry.acquire(name.as_str()),
            Err(RegistryError::DatabaseMarkerMismatch(actual)) if actual == name
        ));
        registry.release(lease).unwrap();
    }

    #[test]
    fn oversized_database_envelope_is_rejected_before_read_allocation() {
        let directory = private_tempdir();
        let entry = expected("db_00000000000000000000000000000005");
        fs::write(
            directory.path().join(entry.file_key().as_str()),
            vec![0u8; DATABASE_ENVELOPE_BYTES + 1],
        )
        .unwrap();
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        assert_eq!(root.inspect_database(&entry), Err(RegistryError::Backend));
        assert_eq!(root.unlink_database(&entry), Err(RegistryError::Backend));
        assert!(directory.path().join(entry.file_key().as_str()).is_file());
    }

    #[test]
    fn special_database_file_does_not_block_inspection() {
        let directory = private_tempdir();
        let entry = expected("db_00000000000000000000000000000006");
        let path = CString::new(
            directory
                .path()
                .join(entry.file_key().as_str())
                .as_os_str()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let mut root = OsDataRoot::open(directory.path()).unwrap();
        root.acquire_exclusive_registry_lock().unwrap();
        assert_eq!(
            root.inspect_database(&entry),
            Ok(DatabaseFileInspection::Mismatch)
        );
    }
}
