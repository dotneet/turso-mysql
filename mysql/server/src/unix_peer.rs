//! Peer effective-UID checks for accepted Unix streams.

use std::{
    fmt,
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
};

/// Captures the server effective UID once and verifies accepted local peers.
pub(crate) struct UnixPeerVerifier {
    expected_effective_uid: libc::uid_t,
}

impl UnixPeerVerifier {
    /// Captures the effective UID that accepted peers must match.
    pub(crate) fn capture_for_startup() -> Result<Self, UnixPeerError> {
        ensure_supported_platform()?;
        // SAFETY: `geteuid` has no arguments, does not retain state, and returns
        // the effective UID of this process.
        let expected_effective_uid = unsafe { libc::geteuid() };
        Ok(Self {
            expected_effective_uid,
        })
    }

    /// Rejects a stream unless the operating system reports the startup UID.
    pub(crate) fn verify(&self, stream: &UnixStream) -> Result<(), UnixPeerError> {
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
pub(crate) enum UnixPeerError {
    /// The operating system did not provide credentials for this stream.
    CredentialsUnavailable,
    /// The peer effective UID differs from the one captured at startup.
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
