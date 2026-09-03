//! Descriptor-backed Unix socket ownership for the local checkpoint authority.
//!
//! The configured directory is opened component by component and retained for
//! the listener lifetime. This prevents a pathname replacement between the
//! validation immediately before bind and later endpoint cleanup.

use std::{
    ffi::CString,
    fs::File,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

const OWNER_LOCK_NAME: &[u8] = b".turso-mysql-checkpoint-authority.lock";
const OWNER_LOCK_MODE: u32 = 0o600;
const SOCKET_DIRECTORY_MODE: u32 = 0o710;
const SOCKET_MODE: u32 = 0o660;
const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Fail-closed categories for local authority socket filesystem operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnixSocketFsError {
    /// The current Unix platform has no reviewed implementation.
    UnsupportedPlatform,
    /// A configured path or endpoint name is malformed.
    InvalidPath,
    /// A configured directory or one of its ancestors is not trusted.
    InvalidDirectory,
    /// A filesystem entry has an unexpected type, owner, group, or mode.
    InvalidEntry,
    /// The endpoint name was already occupied.
    EndpointExists,
    /// Another service instance owns the authority socket lock.
    LockHeld,
    /// An operating-system operation failed without a safe finer category.
    Backend,
}

impl std::fmt::Display for UnixSocketFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedPlatform => f.write_str("Unix checkpoint authority is unsupported"),
            Self::InvalidPath => f.write_str("Unix checkpoint authority path is invalid"),
            Self::InvalidDirectory => f.write_str("Unix checkpoint authority directory is invalid"),
            Self::InvalidEntry => {
                f.write_str("Unix checkpoint authority filesystem entry is invalid")
            }
            Self::EndpointExists => {
                f.write_str("Unix checkpoint authority endpoint already exists")
            }
            Self::LockHeld => f.write_str("Unix checkpoint authority is already owned"),
            Self::Backend => f.write_str("Unix checkpoint authority filesystem operation failed"),
        }
    }
}

impl std::error::Error for UnixSocketFsError {}

/// Opaque identity for one bound authority socket.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SocketEndpointIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

impl std::fmt::Debug for SocketEndpointIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<checkpoint authority socket identity>")
    }
}

/// Retains the private directory below which the authority socket is managed.
pub(crate) struct UnixSocketDirectory {
    directory: File,
    owner_uid: libc::uid_t,
    owner_gid: libc::gid_t,
}

impl UnixSocketDirectory {
    /// Opens an absolute socket directory without following any component.
    pub(crate) fn open(path: &Path) -> Result<Self, UnixSocketFsError> {
        ensure_supported_platform()?;
        let components = checked_components(path)?;
        let owner_uid = effective_uid();
        let owner_gid = effective_gid();
        let mut directory = open_root_directory()?;
        validate_trusted_ancestor(&directory, owner_uid)?;
        for component in components {
            directory = open_directory_child(&directory, &component)?;
            validate_trusted_ancestor(&directory, owner_uid)?;
        }
        validate_socket_directory(&directory, owner_uid, owner_gid)?;
        Ok(Self {
            directory,
            owner_uid,
            owner_gid,
        })
    }

    /// Acquires the service-wide private lock without waiting for another owner.
    pub(crate) fn acquire_owner_lock(&self) -> Result<SocketOwnerLock, UnixSocketFsError> {
        let lock = self.open_owner_lock()?;
        loop {
            // SAFETY: `lock` owns a valid descriptor and flock does not access
            // Rust-managed memory through a pointer.
            let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                validate_private_lock(&lock, self.owner_uid, self.owner_gid)?;
                return Ok(SocketOwnerLock { _lock: lock });
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
                    return Err(UnixSocketFsError::LockHeld);
                }
                _ => return Err(UnixSocketFsError::Backend),
            }
        }
    }

    /// Checks every condition that must hold immediately before `UnixListener::bind`.
    ///
    /// The caller keeps its owner lock alive across this call and bind.
    pub(crate) fn prepare_bind(
        &self,
        configured_directory: &Path,
        filename: &str,
    ) -> Result<PathBuf, UnixSocketFsError> {
        let socket_path = checked_socket_path(configured_directory, filename)?;
        self.revalidate()?;
        self.path_still_resolves_to_self(configured_directory)?;
        self.remove_stale_endpoint(filename)?;
        self.ensure_endpoint_absent(filename)?;
        Ok(socket_path)
    }

    /// Rechecks the retained directory before a sensitive lifecycle transition.
    pub(crate) fn revalidate(&self) -> Result<(), UnixSocketFsError> {
        validate_socket_directory(&self.directory, self.owner_uid, self.owner_gid)
    }

    /// Confirms the configured pathname still names the retained directory.
    pub(crate) fn path_still_resolves_to_self(&self, path: &Path) -> Result<(), UnixSocketFsError> {
        let components = checked_components(path)?;
        let mut resolved = open_root_directory()?;
        validate_trusted_ancestor(&resolved, self.owner_uid)?;
        for component in components {
            resolved = open_directory_child(&resolved, &component)?;
            validate_trusted_ancestor(&resolved, self.owner_uid)?;
        }
        validate_socket_directory(&resolved, self.owner_uid, self.owner_gid)?;
        if file_identity(&resolved)? == file_identity(&self.directory)? {
            Ok(())
        } else {
            Err(UnixSocketFsError::InvalidDirectory)
        }
    }

    /// Rejects every existing filesystem entry at the configured endpoint name.
    pub(crate) fn ensure_endpoint_absent(&self, filename: &str) -> Result<(), UnixSocketFsError> {
        if self.stat_endpoint(filename)?.is_some() {
            Err(UnixSocketFsError::EndpointExists)
        } else {
            Ok(())
        }
    }

    fn remove_stale_endpoint(&self, filename: &str) -> Result<(), UnixSocketFsError> {
        let Some(endpoint) = self.stat_endpoint(filename)? else {
            return Ok(());
        };
        if endpoint.file_type != libc::S_IFSOCK
            || endpoint.owner_uid != self.owner_uid
            || endpoint.owner_gid != self.owner_gid
            || endpoint.mode != SOCKET_MODE
        {
            return Err(UnixSocketFsError::EndpointExists);
        }
        if !self.endpoint_identity_matches(filename, endpoint.identity)? {
            return Err(UnixSocketFsError::EndpointExists);
        }
        if !self.unlink_endpoint_name(filename)? {
            return Err(UnixSocketFsError::EndpointExists);
        }
        self.directory
            .sync_all()
            .map_err(|_| UnixSocketFsError::Backend)
    }

    /// Makes a newly bound endpoint group-readable/writable and captures its identity.
    pub(crate) fn configure_bound_endpoint(
        &self,
        filename: &str,
    ) -> Result<SocketEndpointIdentity, UnixSocketFsError> {
        let before = self
            .stat_endpoint(filename)?
            .ok_or(UnixSocketFsError::InvalidEntry)?;
        if before.file_type != libc::S_IFSOCK
            || before.owner_uid != self.owner_uid
            || before.owner_gid != self.owner_gid
        {
            return Err(UnixSocketFsError::InvalidEntry);
        }

        let name = checked_name(filename.as_bytes())?;
        // SAFETY: the retained directory is not writable by client accounts;
        // another service-UID process is inside this authority's trust boundary.
        let permissions = unsafe {
            libc::fchmodat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                SOCKET_MODE as libc::mode_t,
                0,
            )
        };
        if permissions != 0 {
            return Err(UnixSocketFsError::Backend);
        }

        let after = self
            .stat_endpoint(filename)?
            .ok_or(UnixSocketFsError::InvalidEntry)?;
        if after.file_type != libc::S_IFSOCK
            || after.owner_uid != self.owner_uid
            || after.owner_gid != self.owner_gid
            || after.mode != SOCKET_MODE
            || after.identity != before.identity
        {
            return Err(UnixSocketFsError::InvalidEntry);
        }
        Ok(after.identity)
    }

    /// Removes a bound socket only when the same endpoint remains at its name.
    pub(crate) fn unlink_endpoint_if_matches(
        &self,
        filename: &str,
        expected: SocketEndpointIdentity,
    ) -> Result<bool, UnixSocketFsError> {
        if !self.endpoint_identity_matches(filename, expected)? {
            return Ok(false);
        }
        self.unlink_endpoint_name(filename)
    }

    fn unlink_endpoint_name(&self, filename: &str) -> Result<bool, UnixSocketFsError> {
        let name = checked_name(filename.as_bytes())?;
        // SAFETY: the name is one component resolved below the retained
        // directory descriptor. unlinkat never follows a symlink.
        let result = unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) };
        if result == 0 {
            Ok(true)
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(UnixSocketFsError::Backend)
        }
    }

    /// Removes an unpublished socket after a bind-stage failure.
    pub(crate) fn remove_unpublished_socket(
        &self,
        filename: &str,
    ) -> Result<bool, UnixSocketFsError> {
        let Some(endpoint) = self.stat_endpoint(filename)? else {
            return Ok(false);
        };
        if endpoint.file_type != libc::S_IFSOCK || endpoint.owner_uid != self.owner_uid {
            return Err(UnixSocketFsError::InvalidEntry);
        }
        if !self.endpoint_is_same_socket(filename, endpoint.identity)? {
            return Ok(false);
        }
        self.unlink_endpoint_name(filename)
    }

    fn endpoint_is_same_socket(
        &self,
        filename: &str,
        expected: SocketEndpointIdentity,
    ) -> Result<bool, UnixSocketFsError> {
        let Some(endpoint) = self.stat_endpoint(filename)? else {
            return Ok(false);
        };
        Ok(endpoint.file_type == libc::S_IFSOCK
            && endpoint.owner_uid == self.owner_uid
            && endpoint.identity == expected)
    }

    fn endpoint_identity_matches(
        &self,
        filename: &str,
        expected: SocketEndpointIdentity,
    ) -> Result<bool, UnixSocketFsError> {
        let Some(endpoint) = self.stat_endpoint(filename)? else {
            return Ok(false);
        };
        Ok(endpoint.file_type == libc::S_IFSOCK
            && endpoint.owner_uid == self.owner_uid
            && endpoint.owner_gid == self.owner_gid
            && endpoint.mode == SOCKET_MODE
            && endpoint.identity == expected)
    }

    fn open_owner_lock(&self) -> Result<File, UnixSocketFsError> {
        let name = checked_name(OWNER_LOCK_NAME)?;
        for _ in 0..16 {
            // SAFETY: the name is one NUL-terminated component below the
            // retained descriptor. O_EXCL makes guessed names harmless.
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
                    OWNER_LOCK_MODE as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: `fd` is a fresh descriptor owned by this value.
                let lock = unsafe { File::from_raw_fd(fd) };
                set_mode(&lock, OWNER_LOCK_MODE as libc::mode_t)?;
                validate_private_lock(&lock, self.owner_uid, self.owner_gid)?;
                return Ok(lock);
            }

            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if error.raw_os_error() != Some(libc::EEXIST) {
                return Err(UnixSocketFsError::Backend);
            }

            // SAFETY: O_NOFOLLOW rejects links and O_NONBLOCK prevents an
            // attacker-created FIFO from blocking service startup.
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
                let lock = unsafe { File::from_raw_fd(fd) };
                validate_private_lock(&lock, self.owner_uid, self.owner_gid)?;
                return Ok(lock);
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
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
        // SAFETY: fstatat initialized `stat` after its successful return.
        let stat = unsafe { stat.assume_init() };
        Ok(Some(EndpointStat {
            identity: SocketEndpointIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            },
            owner_uid: stat.st_uid,
            owner_gid: stat.st_gid,
            file_type: (stat.st_mode as libc::mode_t) & libc::S_IFMT,
            mode: (stat.st_mode as u32) & 0o7777,
        }))
    }
}

impl std::fmt::Debug for UnixSocketDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixSocketDirectory")
            .field("directory", &"<retained>")
            .finish()
    }
}

/// Holds the authority owner lock for the listener lifetime.
pub(crate) struct SocketOwnerLock {
    _lock: File,
}

impl std::fmt::Debug for SocketOwnerLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketOwnerLock(<held>)")
    }
}

struct EndpointStat {
    identity: SocketEndpointIdentity,
    owner_uid: libc::uid_t,
    owner_gid: libc::gid_t,
    file_type: libc::mode_t,
    mode: u32,
}

/// Validates and joins a configured socket directory and one simple name.
pub(crate) fn checked_socket_path(
    directory: &Path,
    filename: &str,
) -> Result<PathBuf, UnixSocketFsError> {
    checked_components(directory)?;
    checked_name(filename.as_bytes())?;
    let socket_path = directory.join(filename);
    if socket_path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH_BYTES {
        return Err(UnixSocketFsError::InvalidPath);
    }
    Ok(socket_path)
}

fn ensure_supported_platform() -> Result<(), UnixSocketFsError> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        Err(UnixSocketFsError::UnsupportedPlatform)
    }
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
    let root = CString::new("/").expect("root path has no NUL");
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
    // SAFETY: the component is one NUL-terminated name resolved below the
    // retained parent descriptor.
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

fn validate_socket_directory(
    directory: &File,
    owner_uid: libc::uid_t,
    owner_gid: libc::gid_t,
) -> Result<(), UnixSocketFsError> {
    let metadata = directory
        .metadata()
        .map_err(|_| UnixSocketFsError::InvalidDirectory)?;
    if metadata.is_dir()
        && metadata.uid() == owner_uid
        && metadata.gid() == owner_gid
        && metadata.mode() & 0o7777 == SOCKET_DIRECTORY_MODE
    {
        Ok(())
    } else {
        Err(UnixSocketFsError::InvalidDirectory)
    }
}

fn validate_trusted_ancestor(
    directory: &File,
    owner_uid: libc::uid_t,
) -> Result<(), UnixSocketFsError> {
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

fn validate_private_lock(
    lock: &File,
    owner_uid: libc::uid_t,
    owner_gid: libc::gid_t,
) -> Result<(), UnixSocketFsError> {
    let metadata = lock
        .metadata()
        .map_err(|_| UnixSocketFsError::InvalidEntry)?;
    if metadata.is_file()
        && metadata.uid() == owner_uid
        && metadata.gid() == owner_gid
        && metadata.mode() & 0o7777 == OWNER_LOCK_MODE
    {
        Ok(())
    } else {
        Err(UnixSocketFsError::InvalidEntry)
    }
}

fn file_identity(file: &File) -> Result<SocketEndpointIdentity, UnixSocketFsError> {
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `file` owns a valid descriptor and `stat` points to writable storage.
    let result = unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) };
    if result < 0 {
        return Err(UnixSocketFsError::InvalidDirectory);
    }
    // SAFETY: fstat initialized `stat` after its successful return.
    let stat = unsafe { stat.assume_init() };
    Ok(SocketEndpointIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}

fn set_mode(file: &File, mode: libc::mode_t) -> Result<(), UnixSocketFsError> {
    // SAFETY: `file` owns this descriptor and fchmod does not dereference Rust memory.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } == 0 {
        Ok(())
    } else {
        Err(UnixSocketFsError::Backend)
    }
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid reads process state and has no memory arguments.
    unsafe { libc::geteuid() }
}

fn effective_gid() -> libc::gid_t {
    // SAFETY: getegid reads process state and has no memory arguments.
    unsafe { libc::getegid() }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        os::unix::{
            fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        path::PathBuf,
    };

    fn private_root() -> tempfile::TempDir {
        let root = tempfile::Builder::new()
            .prefix("ca-sock-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        fs::set_permissions(
            root.path(),
            fs::Permissions::from_mode(SOCKET_DIRECTORY_MODE),
        )
        .unwrap();
        root
    }

    fn private_child(parent: &Path, name: &str) -> PathBuf {
        let child = parent.join(name);
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(SOCKET_DIRECTORY_MODE)).unwrap();
        child
    }

    #[test]
    fn accepts_a_retained_private_directory_and_rejects_dot_components() {
        let root = private_root();
        let child = private_child(root.path(), "socket");
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
    fn rejects_symlinks_wrong_modes_and_writable_ancestors() {
        let root = private_root();
        let target = private_child(root.path(), "target");
        symlink(&target, root.path().join("link")).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&root.path().join("link")),
            Err(UnixSocketFsError::InvalidDirectory)
        ));

        let wrong_mode = private_child(root.path(), "wrong-mode");
        fs::set_permissions(&wrong_mode, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&wrong_mode),
            Err(UnixSocketFsError::InvalidDirectory)
        ));

        let ancestor = private_child(root.path(), "ancestor");
        let child = private_child(&ancestor, "child");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o730)).unwrap();
        assert!(matches!(
            UnixSocketDirectory::open(&child),
            Err(UnixSocketFsError::InvalidDirectory)
        ));
    }

    #[test]
    fn bind_preparation_rejects_existing_entries_and_long_paths() {
        let root = private_root();
        let directory = UnixSocketDirectory::open(root.path()).unwrap();
        let _lock = directory.acquire_owner_lock().unwrap();

        File::create(root.path().join("occupied")).unwrap();
        assert!(matches!(
            directory.prepare_bind(root.path(), "occupied"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        fs::remove_file(root.path().join("occupied")).unwrap();

        let listener = UnixListener::bind(root.path().join("socket")).unwrap();
        assert!(matches!(
            directory.prepare_bind(root.path(), "socket"),
            Err(UnixSocketFsError::EndpointExists)
        ));
        drop(listener);
        fs::remove_file(root.path().join("socket")).unwrap();

        let too_long = "x".repeat(MAX_SOCKET_PATH_BYTES + 1);
        assert_eq!(
            checked_socket_path(Path::new("/"), &too_long),
            Err(UnixSocketFsError::InvalidPath)
        );
    }

    #[test]
    fn owner_lock_is_private_and_exclusive() {
        let root = private_root();
        let directory = UnixSocketDirectory::open(root.path()).unwrap();
        let owner = directory.acquire_owner_lock().unwrap();
        let metadata =
            fs::metadata(root.path().join(".turso-mysql-checkpoint-authority.lock")).unwrap();
        assert_eq!(metadata.mode() & 0o7777, OWNER_LOCK_MODE);

        let second = UnixSocketDirectory::open(root.path()).unwrap();
        assert!(matches!(
            second.acquire_owner_lock(),
            Err(UnixSocketFsError::LockHeld)
        ));
        drop(owner);
        assert!(second.acquire_owner_lock().is_ok());
    }

    #[test]
    fn removes_only_a_stale_endpoint_after_taking_the_owner_lock() {
        let root = private_root();
        let socket_name = "authority.sock";
        let socket_path = root.path().join(socket_name);

        let directory = UnixSocketDirectory::open(root.path()).unwrap();
        let owner = directory.acquire_owner_lock().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        directory.configure_bound_endpoint(socket_name).unwrap();
        drop(listener);
        drop(owner);

        let restarted = UnixSocketDirectory::open(root.path()).unwrap();
        let _restarted_owner = restarted.acquire_owner_lock().unwrap();
        assert_eq!(
            restarted.prepare_bind(root.path(), socket_name).unwrap(),
            socket_path
        );
        assert!(!socket_path.exists());

        let replacement = UnixListener::bind(&socket_path).unwrap();
        assert_eq!(
            restarted.prepare_bind(root.path(), socket_name),
            Err(UnixSocketFsError::EndpointExists)
        );
        assert!(socket_path.exists());
        drop(replacement);
    }

    #[test]
    fn owner_lock_rejects_a_preexisting_nonprivate_file() {
        let root = private_root();
        File::create(root.path().join(".turso-mysql-checkpoint-authority.lock")).unwrap();
        fs::set_permissions(
            root.path().join(".turso-mysql-checkpoint-authority.lock"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();

        let directory = UnixSocketDirectory::open(root.path()).unwrap();
        assert!(matches!(
            directory.acquire_owner_lock(),
            Err(UnixSocketFsError::InvalidEntry)
        ));
    }

    #[test]
    fn captures_exact_socket_permissions_and_does_not_delete_a_replacement() {
        let root = private_root();
        let directory = UnixSocketDirectory::open(root.path()).unwrap();
        let _lock = directory.acquire_owner_lock().unwrap();
        let socket_name = "authority.sock";
        let socket_path = directory.prepare_bind(root.path(), socket_name).unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();
        let identity = directory.configure_bound_endpoint(socket_name).unwrap();
        let metadata = fs::metadata(&socket_path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.uid(), effective_uid());
        assert_eq!(metadata.gid(), effective_gid());
        assert_eq!(metadata.mode() & 0o7777, SOCKET_MODE);

        drop(listener);
        fs::remove_file(&socket_path).unwrap();
        let replacement = UnixListener::bind(&socket_path).unwrap();
        let replacement_identity = directory.configure_bound_endpoint(socket_name).unwrap();
        assert!(!directory
            .unlink_endpoint_if_matches(socket_name, identity)
            .unwrap());
        assert!(socket_path.exists());
        assert!(directory
            .unlink_endpoint_if_matches(socket_name, replacement_identity)
            .unwrap());
        drop(replacement);
    }

    #[test]
    fn path_revalidation_detects_an_ancestor_that_becomes_writable() {
        let root = private_root();
        let ancestor = private_child(root.path(), "a");
        let socket = private_child(&ancestor, "s");
        let directory = UnixSocketDirectory::open(&socket).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o730)).unwrap();

        assert_eq!(
            directory.prepare_bind(&socket, "a"),
            Err(UnixSocketFsError::InvalidDirectory)
        );
    }
}
