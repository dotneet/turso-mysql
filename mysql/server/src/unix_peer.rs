//! Peer effective-UID checks for accepted Unix streams.

use std::{
    fmt,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
};

/// Verifies that a Unix peer has one expected effective UID.
pub struct UnixPeerVerifier {
    expected_effective_uid: libc::uid_t,
}

impl UnixPeerVerifier {
    /// Captures the effective UID that accepted peers must match.
    pub(crate) fn capture_for_startup() -> Result<Self, UnixPeerError> {
        Self::for_effective_uid(effective_uid())
    }

    /// Creates a verifier for a configured Unix account.
    pub fn for_effective_uid(expected_effective_uid: u32) -> Result<Self, UnixPeerError> {
        ensure_supported_platform()?;
        Ok(Self {
            expected_effective_uid,
        })
    }

    /// Rejects a stream unless the operating system reports the expected UID.
    pub fn verify(&self, stream: &UnixStream) -> Result<(), UnixPeerError> {
        verify_effective_uid(
            self.expected_effective_uid,
            peer_effective_uid(stream.as_raw_fd())?,
        )
    }
}

fn ensure_supported_platform() -> Result<(), UnixPeerError> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        Err(UnixPeerError::UnsupportedPlatform)
    }
}

impl fmt::Debug for UnixPeerVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixPeerVerifier")
            .field("expected_effective_uid", &"<redacted>")
            .finish()
    }
}

/// A peer credential check failed without exposing a UID or PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixPeerError {
    /// The operating system did not provide credentials for this stream.
    CredentialsUnavailable,
    /// The peer effective UID differs from the configured expectation.
    EffectiveUidMismatch,
    /// This Unix target has no reviewed peer-credential implementation.
    UnsupportedPlatform,
}

impl fmt::Display for UnixPeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CredentialsUnavailable => f.write_str("Unix peer credentials are unavailable"),
            Self::EffectiveUidMismatch => f.write_str("Unix peer effective UID is not allowed"),
            Self::UnsupportedPlatform => {
                f.write_str("Unix peer credentials are unsupported on this platform")
            }
        }
    }
}

impl std::error::Error for UnixPeerError {}

fn verify_effective_uid(
    expected_effective_uid: libc::uid_t,
    peer_effective_uid: libc::uid_t,
) -> Result<(), UnixPeerError> {
    if peer_effective_uid == expected_effective_uid {
        Ok(())
    } else {
        Err(UnixPeerError::EffectiveUidMismatch)
    }
}

fn effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no arguments and does not access Rust-managed memory.
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn peer_effective_uid(raw_fd: RawFd) -> Result<libc::uid_t, UnixPeerError> {
    let expected_length: libc::socklen_t = std::mem::size_of::<libc::ucred>()
        .try_into()
        .map_err(|_| UnixPeerError::CredentialsUnavailable)?;
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut actual_length = expected_length;
    // SAFETY: `raw_fd` comes from an accepted `UnixStream`; `credentials` and
    // `actual_length` are writable for their exact declared sizes. The output
    // is read only after `getsockopt` succeeds and reports the full struct.
    let result = unsafe {
        libc::getsockopt(
            raw_fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast::<libc::c_void>(),
            &mut actual_length,
        )
    };
    if result != 0 || actual_length != expected_length {
        return Err(UnixPeerError::CredentialsUnavailable);
    }
    // SAFETY: the successful call above filled exactly one complete `ucred`.
    let credentials = unsafe { credentials.assume_init() };
    Ok(credentials.uid)
}

#[cfg(target_os = "macos")]
fn peer_effective_uid(raw_fd: RawFd) -> Result<libc::uid_t, UnixPeerError> {
    let mut uid = std::mem::MaybeUninit::<libc::uid_t>::uninit();
    let mut gid = std::mem::MaybeUninit::<libc::gid_t>::uninit();
    // SAFETY: `raw_fd` comes from an accepted `UnixStream`; `uid` and `gid`
    // provide writable storage for `getpeereid`. Their values are read only
    // after the function reports success.
    let result = unsafe { libc::getpeereid(raw_fd, uid.as_mut_ptr(), gid.as_mut_ptr()) };
    if result != 0 {
        return Err(UnixPeerError::CredentialsUnavailable);
    }
    // SAFETY: successful `getpeereid` initialized both output values.
    Ok(unsafe { uid.assume_init() })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_effective_uid(_raw_fd: RawFd) -> Result<libc::uid_t, UnixPeerError> {
    Err(UnixPeerError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn accepted_stream_reports_the_startup_effective_uid() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let verifier = UnixPeerVerifier::capture_for_startup().unwrap();

        assert_eq!(verifier.verify(&stream), Ok(()));
        assert!(format!("{verifier:?}").contains("<redacted>"));
    }

    #[test]
    fn different_effective_uids_are_rejected_without_reporting_them() {
        assert_eq!(
            verify_effective_uid(0, 1),
            Err(UnixPeerError::EffectiveUidMismatch)
        );
        assert!(!format!("{:?}", UnixPeerError::EffectiveUidMismatch).contains('0'));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn configured_effective_uid_is_checked_against_kernel_credentials() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let matching = UnixPeerVerifier::for_effective_uid(effective_uid()).unwrap();
        assert_eq!(matching.verify(&stream), Ok(()));

        let different = effective_uid().wrapping_add(1);
        let mismatching = UnixPeerVerifier::for_effective_uid(different).unwrap();
        assert_eq!(
            mismatching.verify(&stream),
            Err(UnixPeerError::EffectiveUidMismatch)
        );
    }

    #[test]
    fn invalid_descriptor_never_produces_credentials() {
        assert!(matches!(
            peer_effective_uid(-1),
            Err(UnixPeerError::CredentialsUnavailable | UnixPeerError::UnsupportedPlatform)
        ));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_targets_reject_the_runtime_at_startup() {
        assert!(matches!(
            UnixPeerVerifier::capture_for_startup(),
            Err(UnixPeerError::UnsupportedPlatform)
        ));
    }
}
