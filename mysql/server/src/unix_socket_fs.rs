//! Unix filesystem capabilities used by the local MySQL listener.
//!
//! The listener directory is opened component by component and retained as a
//! descriptor. Every ancestor has a trusted owner and rejects group and other
//! write access, so a different effective UID cannot redirect the configured
//! pathname between the final validation and `bind`.

use std::ffi::CString;
use std::fs::File;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const SOCKET_LOCK_NAME: &[u8] = b".turso-mysql.socket.lock";
const PRIVATE_MODE: u32 = 0o600;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

/// Filesystem failures are intentionally coarse so paths, ids, and operating
/// system details do not escape into logs or protocol errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnixSocketFsError {
    /// The configured directory path is not absolute or contains a rejected component.
    InvalidPath,
    /// A directory component is missing, not a directory, or not private.
    InvalidDirectory,
    /// The endpoint or lock entry is not a permitted filesystem object.
    InvalidEntry,
    /// The endpoint already exists.
    EndpointExists,
    /// Another listener owns the socket lock.
    LockHeld,
    /// An operating-system operation failed without a more specific category.
    Backend,
}

impl std::fmt::Display for UnixSocketFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath => f.write_str("Unix socket path is invalid"),
            Self::InvalidDirectory => f.write_str("Unix socket directory is invalid"),
            Self::InvalidEntry => f.write_str("Unix socket filesystem entry is invalid"),
            Self::EndpointExists => f.write_str("Unix socket endpoint already exists"),
            Self::LockHeld => f.write_str("Unix socket is already owned"),
            Self::Backend => f.write_str("Unix socket filesystem operation failed"),
        }
    }
}

impl std::error::Error for UnixSocketFsError {}

/// Identity of one socket endpoint inode.
///
/// The fields stay private so callers can only use an identity for equality
/// checks; custom formatting avoids exposing device or inode numbers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketEndpointIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

impl std::fmt::Debug for SocketEndpointIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<socket endpoint identity>")
    }
}

/// A retained private directory below which socket names may be inspected.
pub(crate) struct UnixSocketDirectory {
    directory: File,
    owner_uid: u32,
}

impl std::fmt::Debug for UnixSocketDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixSocketDirectory")
            .field("directory", &"<retained>")
            .finish()
    }
}

impl UnixSocketDirectory {
    /// Opens every absolute path component with `O_NOFOLLOW` and retains the
    /// final descriptor after rejecting untrusted or writable ancestors and
    /// checking effective-uid ownership and mode 0700.
    pub(crate) fn open(path: &Path) -> Result<Self, UnixSocketFsError> {
        let components = checked_components(path)?;
        let owner_uid = effective_uid();
        let mut directory = open_root_directory()?;
        validate_trusted_ancestor(&directory, owner_uid)?;
        for component in components {
            directory = open_directory_child(&directory, &component)?;
            validate_trusted_ancestor(&directory, owner_uid)?;
        }
        validate_private_directory(&directory, owner_uid)?;
        Ok(Self {
            directory,
            owner_uid,
        })
    }

    /// Acquires the fixed private lock for the lifetime of the returned value.
    pub(crate) fn acquire_owner_lock(&self) -> Result<SocketOwnerLock, UnixSocketFsError> {
        let lock = self.open_lock_file()?;
        loop {
            // SAFETY: `lock` is an owned descriptor and flock does not access
            // Rust-managed memory through a pointer.
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                validate_private_file(&lock, self.owner_uid)?;
                return Ok(SocketOwnerLock { _lock: lock });
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    return Err(UnixSocketFsError::LockHeld);
                }
                _ => return Err(UnixSocketFsError::Backend),
            }
        }
    }

    /// Confirms that the child name is absent without following symlinks.
    pub(crate) fn ensure_endpoint_absent(&self, filename: &str) -> Result<(), UnixSocketFsError> {
        if self.stat_endpoint(filename)?.is_some() {
            Err(UnixSocketFsError::EndpointExists)
        } else {
            Ok(())
        }
    }

    /// Revalidates the retained directory's owner and exact private mode.
    pub(crate) fn revalidate(&self) -> Result<(), UnixSocketFsError> {
        validate_private_directory(&self.directory, self.owner_uid)
    }

    /// Checks that an absolute configured path still resolves to this directory.
    ///
    /// The caller must hold the owner lock while using this result because the
    /// final pathname check and a later bind cannot be one portable syscall.
    pub(crate) fn path_still_resolves_to_self(&self, path: &Path) -> Result<(), UnixSocketFsError> {
        let components = checked_components(path)?;
        let mut resolved = open_root_directory()?;
        validate_trusted_ancestor(&resolved, self.owner_uid)?;
        for component in components {
            resolved = open_directory_child(&resolved, &component)?;
            validate_trusted_ancestor(&resolved, self.owner_uid)?;
        }
        validate_private_directory(&resolved, self.owner_uid)?;
        if file_identity(&resolved)? == file_identity(&self.directory)? {
            Ok(())
        } else {
            Err(UnixSocketFsError::InvalidDirectory)
        }
    }

    /// Returns whether two retained capabilities refer to one directory.
    pub(crate) fn same_directory(
        &self,
        other: &UnixSocketDirectory,
    ) -> Result<bool, UnixSocketFsError> {
        Ok(file_identity(&self.directory)? == file_identity(&other.directory)?)
    }

    /// Sets an already-bound socket endpoint to mode 0600 and captures its
    /// pathname identity only if the object remains the same socket throughout.
    pub(crate) fn set_endpoint_private_mode_and_capture_identity(
        &self,
        filename: &str,
    ) -> Result<SocketEndpointIdentity, UnixSocketFsError> {
        let before = self
            .stat_endpoint(filename)?
            .ok_or(UnixSocketFsError::InvalidEntry)?;
        if before.file_type != libc::S_IFSOCK || before.owner_uid != self.owner_uid {
            return Err(UnixSocketFsError::InvalidEntry);
        }

        let name = checked_name(filename.as_bytes())?;
        // SAFETY: `name` is NUL-terminated and the directory descriptor is
        // retained by `self`.
        let result = unsafe {
            libc::fchmodat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                PRIVATE_MODE as libc::mode_t,
                0,
            )
        };
        if result < 0 {
            return Err(UnixSocketFsError::Backend);
        }

        let after = self
            .stat_endpoint(filename)?
            .ok_or(UnixSocketFsError::InvalidEntry)?;
        if after.file_type != libc::S_IFSOCK
            || after.owner_uid != self.owner_uid
            || after.mode != PRIVATE_MODE as libc::mode_t
            || after.identity != before.identity
        {
            return Err(UnixSocketFsError::InvalidEntry);
        }
        Ok(after.identity)
    }

    /// Returns the inode identity of an existing socket endpoint.
    pub(crate) fn endpoint_identity(
        &self,
        filename: &str,
    ) -> Result<Option<SocketEndpointIdentity>, UnixSocketFsError> {
        let Some(stat) = self.stat_endpoint(filename)? else {
            return Ok(None);
        };
        if stat.file_type != libc::S_IFSOCK {
            return Err(UnixSocketFsError::InvalidEntry);
        }
        Ok(Some(stat.identity))
    }

    /// Removes a socket created during bind before its identity was published.
    pub(crate) fn remove_unpublished_socket(
        &self,
        filename: &str,
    ) -> Result<bool, UnixSocketFsError> {
        let Some(stat) = self.stat_endpoint(filename)? else {
            return Ok(false);
        };
        if stat.file_type != libc::S_IFSOCK || stat.owner_uid != self.owner_uid {
            return Err(UnixSocketFsError::InvalidEntry);
        }
        self.unlink_endpoint_if_matches(filename, stat.identity)
    }

    /// Revalidates an endpoint identity immediately before a caller unlinks
    /// or publishes a pathname. A replacement of any kind returns `false`.
    pub(crate) fn endpoint_identity_matches(
        &self,
        filename: &str,
        expected: SocketEndpointIdentity,
    ) -> Result<bool, UnixSocketFsError> {
        let Some(stat) = self.stat_endpoint(filename)? else {
            return Ok(false);
        };
        Ok(stat.file_type == libc::S_IFSOCK
            && stat.owner_uid == self.owner_uid
            && stat.identity == expected)
    }

    /// Unlinks the endpoint only when its current pathname identity matches.
    /// The caller must hold the owner lock for the check-and-unlink sequence.
    pub(crate) fn unlink_endpoint_if_matches(
        &self,
        filename: &str,
        expected: SocketEndpointIdentity,
    ) -> Result<bool, UnixSocketFsError> {
        if !self.endpoint_identity_matches(filename, expected)? {
            return Ok(false);
        }
        let name = checked_name(filename.as_bytes())?;
        // SAFETY: `name` is one component and is resolved below the retained
        // directory descriptor. unlinkat does not follow a symlink.
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            return Ok(true);
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(UnixSocketFsError::Backend)
        }
    }

    fn open_lock_file(&self) -> Result<File, UnixSocketFsError> {
        let name = checked_name(SOCKET_LOCK_NAME)?;
        for _ in 0..16 {
            // Creating with O_EXCL prevents a guessed or replaced path from
            // being treated as this process's lock.
            // SAFETY: `name` is NUL-terminated and the directory descriptor is
            // retained by `self`; the returned descriptor is owned below.
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
                set_private_mode(&lock)?;
                validate_private_file(&lock, self.owner_uid)?;
                return Ok(lock);
            }

            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(UnixSocketFsError::Backend);
            }

            // O_NOFOLLOW makes an existing symlink fail closed. A short retry
            // handles a cooperative creator that exits between openat calls.
            // SAFETY: `name` and the directory descriptor remain valid; the
            // returned descriptor is owned below.
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                return Ok(unsafe { File::from_raw_fd(fd) });
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                continue;
            }
            return Err(UnixSocketFsError::InvalidEntry);
        }
        Err(UnixSocketFsError::Backend)
    }

    fn stat_endpoint(&self, filename: &str) -> Result<Option<EndpointStat>, UnixSocketFsError> {
        let name = checked_name(filename.as_bytes())?;
        let mut stat = MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `stat` points to writable storage and `name` is NUL-terminated.
        let result = unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(UnixSocketFsError::Backend);
        }
        // SAFETY: fstatat initialized `stat` on success.
        let stat = unsafe { stat.assume_init() };
        Ok(Some(EndpointStat {
            identity: SocketEndpointIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            },
            owner_uid: stat.st_uid,
            file_type: (stat.st_mode as libc::mode_t) & libc::S_IFMT,
            mode: (stat.st_mode as libc::mode_t) & 0o7777,
        }))
    }
}

/// A private lock held for the entire listener lifetime.
pub(crate) struct SocketOwnerLock {
    _lock: File,
}

impl std::fmt::Debug for SocketOwnerLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketOwnerLock")
            .field("lock", &"<held>")
            .finish()
    }
}

struct EndpointStat {
    identity: SocketEndpointIdentity,
    owner_uid: u32,
    file_type: libc::mode_t,
    mode: libc::mode_t,
}

fn checked_components(path: &Path) -> Result<Vec<Vec<u8>>, UnixSocketFsError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(UnixSocketFsError::InvalidPath);
    }
    let mut components = Vec::new();
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b"." || component == b".." {
            return Err(UnixSocketFsError::InvalidPath);
        }
        components.push(component.to_vec());
    }
    Ok(components)
}

fn checked_name(name: &[u8]) -> Result<CString, UnixSocketFsError> {
    if name.is_empty() || name.contains(&0) || name.contains(&b'/') || name == b"." || name == b".."
    {
        return Err(UnixSocketFsError::InvalidPath);
    }
    CString::new(name).map_err(|_| UnixSocketFsError::InvalidPath)
}

fn open_root_directory() -> Result<File, UnixSocketFsError> {
    let root = CString::new("/").expect("literal root path has no NUL");
    // SAFETY: `root` is NUL-terminated and the returned descriptor is owned below.
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(UnixSocketFsError::Backend);
    }
    // SAFETY: `fd` is a fresh descriptor owned by this value.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_child(parent: &File, component: &[u8]) -> Result<File, UnixSocketFsError> {
    let component = checked_name(component)?;
    // SAFETY: `component` is one NUL-terminated name and `parent` retains the
    // directory descriptor used for resolution.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(UnixSocketFsError::InvalidDirectory);
    }
    // SAFETY: `fd` is a fresh descriptor owned by this value.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_private_directory(directory: &File, owner_uid: u32) -> Result<(), UnixSocketFsError> {
    let metadata = directory
        .metadata()
        .map_err(|_| UnixSocketFsError::InvalidDirectory)?;
    if metadata.is_dir()
        && metadata.uid() == owner_uid
        && metadata.mode() & 0o7777 == PRIVATE_DIRECTORY_MODE
    {
        Ok(())
    } else {
        Err(UnixSocketFsError::InvalidDirectory)
    }
}

fn validate_trusted_ancestor(directory: &File, owner_uid: u32) -> Result<(), UnixSocketFsError> {
    let metadata = directory
        .metadata()
        .map_err(|_| UnixSocketFsError::InvalidDirectory)?;
    if metadata.is_dir()
        && (metadata.uid() == 0 || metadata.uid() == owner_uid)
        && metadata.mode() & 0o022 == 0
    {
        Ok(())
    } else {
        Err(UnixSocketFsError::InvalidDirectory)
    }
}

fn validate_private_file(file: &File, owner_uid: u32) -> Result<(), UnixSocketFsError> {
    let metadata = file
        .metadata()
        .map_err(|_| UnixSocketFsError::InvalidEntry)?;
    if metadata.is_file() && metadata.uid() == owner_uid && metadata.mode() & 0o7777 == PRIVATE_MODE
    {
        Ok(())
    } else {
        Err(UnixSocketFsError::InvalidEntry)
    }
}

fn file_identity(file: &File) -> Result<SocketEndpointIdentity, UnixSocketFsError> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `file` owns a valid descriptor and `stat` points to writable
    // storage for one platform `stat` value.
    let result = unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) };
    if result < 0 {
        return Err(UnixSocketFsError::InvalidDirectory);
    }
    // SAFETY: fstat initialized `stat` on success.
    let stat = unsafe { stat.assume_init() };
    Ok(SocketEndpointIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn set_private_mode(file: &File) -> Result<(), UnixSocketFsError> {
    // SAFETY: `file` owns this descriptor and fchmod does not dereference a
    // Rust-managed pointer.
    let result = unsafe { libc::fchmod(file.as_raw_fd(), PRIVATE_MODE as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(UnixSocketFsError::Backend)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid reads process state and does not dereference a pointer.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    fn private_child(parent: &Path, name: &str) -> PathBuf {
        let child = parent.join(name);
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE)).unwrap();
        child
    }

    fn private_root() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE)).unwrap();
        (root, path)
    }

    #[test]
    fn walks_private_directory_and_rejects_dot_components() {
        let (_root, root_path) = private_root();
        let child = private_child(&root_path, "child");
        let directory = UnixSocketDirectory::open(&child).unwrap();
        assert!(directory.revalidate().is_ok());
        assert!(directory.path_still_resolves_to_self(&child).is_ok());
        assert!(matches!(
            UnixSocketDirectory::open(&child.join(".")),
            Err(UnixSocketFsError::InvalidPath)
        ));
        assert!(matches!(
            UnixSocketDirectory::open(&child.join("..")),
            Err(UnixSocketFsError::InvalidPath)
        ));
    }

    #[test]
    fn rejects_symlinked_intermediate_and_final_directory() {
        let (_root, root_path) = private_root();
        let target = private_child(&root_path, "target");
        symlink(&target, root_path.join("intermediate")).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&root_path.join("intermediate")),
            Err(UnixSocketFsError::InvalidDirectory)
        ));

        symlink(&target, root_path.join("final")).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&root_path.join("final")),
            Err(UnixSocketFsError::InvalidDirectory)
        ));
    }

    #[test]
    fn rejects_wrong_mode_and_non_directory() {
        let (_root, root_path) = private_root();
        let wrong_mode = private_child(&root_path, "wrong-mode");
        fs::set_permissions(&wrong_mode, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&wrong_mode),
            Err(UnixSocketFsError::InvalidDirectory)
        ));

        let file = root_path.join("file");
        File::create(&file).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&file),
            Err(UnixSocketFsError::InvalidDirectory)
        ));
    }

    #[test]
    fn rejects_writable_ancestors_including_sticky_directories() {
        let (_root, root_path) = private_root();
        let writable = private_child(&root_path, "writable");
        let child = private_child(&writable, "child");

        fs::set_permissions(&writable, fs::Permissions::from_mode(0o770)).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&child),
            Err(UnixSocketFsError::InvalidDirectory)
        ));

        fs::set_permissions(&writable, fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&child),
            Err(UnixSocketFsError::InvalidDirectory)
        ));
    }

    #[test]
    fn revalidation_rejects_an_ancestor_that_becomes_writable() {
        let (_root, root_path) = private_root();
        let ancestor = private_child(&root_path, "ancestor");
        let child = private_child(&ancestor, "child");
        let directory = UnixSocketDirectory::open(&child).unwrap();

        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o770)).unwrap();

        assert!(matches!(
            directory.path_still_resolves_to_self(&child),
            Err(UnixSocketFsError::InvalidDirectory)
        ));
    }

    #[test]
    fn lock_is_private_and_non_reentrant() {
        let (_root, root_path) = private_root();
        let directory = UnixSocketDirectory::open(&root_path).unwrap();
        let owner = directory.acquire_owner_lock().unwrap();
        let second_directory = UnixSocketDirectory::open(&root_path).unwrap();
        assert!(matches!(
            second_directory.acquire_owner_lock(),
            Err(UnixSocketFsError::LockHeld)
        ));
        drop(owner);
        assert!(second_directory.acquire_owner_lock().is_ok());
    }

    #[test]
    fn existing_endpoint_kinds_are_rejected_without_following_symlinks() {
        let (_root, root_path) = private_root();
        let directory = UnixSocketDirectory::open(&root_path).unwrap();

        assert!(directory.ensure_endpoint_absent("missing").is_ok());

        let regular = root_path.join("regular");
        File::create(&regular).unwrap();
        assert!(matches!(
            directory.ensure_endpoint_absent("regular"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        fs::remove_file(&regular).unwrap();

        let symlink_target = root_path.join("symlink-target");
        File::create(&symlink_target).unwrap();
        symlink(&symlink_target, root_path.join("symlink")).unwrap();
        assert!(matches!(
            directory.ensure_endpoint_absent("symlink"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        fs::remove_file(root_path.join("symlink")).unwrap();
        fs::remove_file(&symlink_target).unwrap();

        let fifo = root_path.join("fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_name` is NUL-terminated and points to writable test
        // directory state only.
        assert_eq!(
            unsafe { libc::mkfifo(fifo_name.as_ptr(), PRIVATE_MODE as libc::mode_t) },
            0
        );
        assert!(matches!(
            directory.ensure_endpoint_absent("fifo"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        fs::remove_file(&fifo).unwrap();

        let socket = root_path.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(matches!(
            directory.ensure_endpoint_absent("socket"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        drop(listener);
    }

    #[test]
    fn endpoint_identity_revalidates_socket_inode() {
        let (_root, root_path) = private_root();
        let directory = UnixSocketDirectory::open(&root_path).unwrap();
        let socket = root_path.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        let identity = directory
            .set_endpoint_private_mode_and_capture_identity("socket")
            .unwrap();
        assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o7777, PRIVATE_MODE);
        assert!(directory
            .endpoint_identity_matches("socket", identity)
            .unwrap());
        drop(listener);
        assert!(directory
            .unlink_endpoint_if_matches("socket", identity)
            .unwrap());
        assert!(!directory
            .endpoint_identity_matches("socket", identity)
            .unwrap());
    }

    #[test]
    fn unpublished_socket_cleanup_rejects_other_entry_kinds() {
        let (_root, root_path) = private_root();
        let directory = UnixSocketDirectory::open(&root_path).unwrap();
        let socket = root_path.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();

        assert!(directory.remove_unpublished_socket("socket").unwrap());
        assert!(!socket.exists());
        drop(listener);

        File::create(root_path.join("regular")).unwrap();
        assert!(matches!(
            directory.remove_unpublished_socket("regular"),
            Err(UnixSocketFsError::InvalidEntry)
        ));
    }
}
