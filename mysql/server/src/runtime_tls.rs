//! Validated TLS material for a network runtime.
//!
//! Configuration only carries paths. This module is the side-effecting
//! boundary that opens those paths, validates their contents, and builds the
//! immutable rustls configuration used by future TCP listeners.

use std::{
    error::Error,
    fmt,
    fs::File,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
    sync::Arc,
};

use rustls::pki_types::{
    pem::{PemObject, SectionKind},
    CertificateDer, PrivateKeyDer,
};
use zeroize::Zeroizing;

use crate::TlsConfig;

/// Maximum encoded bytes accepted from one configured certificate or key file.
///
/// A server certificate chain and private key are normally much smaller. The
/// bound prevents a configuration path from making startup retain an
/// unbounded amount of attacker-controlled data.
pub const MAX_TLS_MATERIAL_BYTES: usize = 1024 * 1024;

const OPEN_FLAGS: i32 = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// An immutable server-side TLS configuration loaded from configured files.
///
/// The private key is retained by rustls because each accepted connection uses
/// the same signing key. This wrapper deliberately does not expose the
/// certificate or key bytes and redacts the underlying rustls configuration.
pub struct TlsServerConfig {
    config: Arc<rustls::ServerConfig>,
}

impl TlsServerConfig {
    /// Opens, validates, and loads the certificate chain and private key.
    ///
    /// The final file and every parent directory are opened without following
    /// symlinks. The first release uses server-authenticated TLS only; MySQL
    /// account authentication remains the application-level identity check.
    pub fn load(config: &TlsConfig) -> Result<Self, TlsMaterialError> {
        let certificate = read_material(config.certificate_path(), MaterialKind::Certificate)?;
        let certificates = parse_certificates(&certificate)?;
        let private_key = read_material(config.private_key_path(), MaterialKind::PrivateKey)?;
        let key = parse_private_key(&private_key)?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|_| TlsMaterialError::ProviderConfiguration)?
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .map_err(|_| TlsMaterialError::CertificateAndKeyRejected)?;

        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// Returns a clone of the immutable rustls configuration for a connection.
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.config)
    }
}

impl fmt::Debug for TlsServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsServerConfig")
            .field("config", &"<redacted>")
            .finish()
    }
}

/// Coarse startup failures for configured TLS material.
///
/// Paths, operating-system details, certificate bytes, and key bytes are not
/// retained in this error so it is safe to surface at a runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMaterialError {
    /// A configured path could not be traversed or opened.
    OpenFailed,
    /// A configured path contains a component that this loader does not admit.
    PathRejected,
    /// The final entry is not a regular file.
    NotRegularFile,
    /// The path is not owned by a trusted UID, or a file/directory is writable
    /// by a group or other user.
    PermissionsRejected,
    /// The file exceeded MAX_TLS_MATERIAL_BYTES.
    TooLarge,
    /// Reading a regular file failed.
    ReadFailed,
    /// No certificate PEM item was present or a certificate PEM item was malformed.
    CertificatePemRejected,
    /// No private-key PEM item was present or a private-key PEM item was malformed.
    PrivateKeyPemRejected,
    /// More than one private key was configured.
    MultiplePrivateKeys,
    /// The rustls provider could not be configured with the selected versions.
    ProviderConfiguration,
    /// Rustls rejected the certificate chain and private-key pairing.
    CertificateAndKeyRejected,
}

impl fmt::Display for TlsMaterialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenFailed => f.write_str("TLS material could not be opened"),
            Self::PathRejected => f.write_str("TLS material path is not permitted"),
            Self::NotRegularFile => f.write_str("TLS material is not a regular file"),
            Self::PermissionsRejected => f.write_str("TLS material permissions are not permitted"),
            Self::TooLarge => f.write_str("TLS material is too large"),
            Self::ReadFailed => f.write_str("TLS material could not be read"),
            Self::CertificatePemRejected => f.write_str("TLS certificate PEM is invalid"),
            Self::PrivateKeyPemRejected => f.write_str("TLS private-key PEM is invalid"),
            Self::MultiplePrivateKeys => f.write_str("TLS private-key file contains multiple keys"),
            Self::ProviderConfiguration => f.write_str("TLS provider configuration is invalid"),
            Self::CertificateAndKeyRejected => {
                f.write_str("TLS certificate and private key were rejected")
            }
        }
    }
}

impl Error for TlsMaterialError {}

#[derive(Clone, Copy)]
enum MaterialKind {
    Certificate,
    PrivateKey,
}

fn parse_certificates(bytes: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsMaterialError> {
    validate_pem_labels(bytes, MaterialKind::Certificate)?;
    let mut certificates = Vec::new();
    for item in CertificateDer::pem_slice_iter(bytes) {
        certificates.push(item.map_err(|_| TlsMaterialError::CertificatePemRejected)?);
    }
    if certificates.is_empty() {
        return Err(TlsMaterialError::CertificatePemRejected);
    }
    Ok(certificates)
}

fn parse_private_key(bytes: &[u8]) -> Result<PrivateKeyDer<'static>, TlsMaterialError> {
    validate_pem_labels(bytes, MaterialKind::PrivateKey)?;
    let mut key = None;
    for item in <(SectionKind, Vec<u8>)>::pem_slice_iter(bytes) {
        let (section, der) = item.map_err(|_| TlsMaterialError::PrivateKeyPemRejected)?;
        let parsed = match section {
            SectionKind::RsaPrivateKey => PrivateKeyDer::Pkcs1(der.into()),
            SectionKind::EcPrivateKey => PrivateKeyDer::Sec1(der.into()),
            SectionKind::PrivateKey => PrivateKeyDer::Pkcs8(der.into()),
            _ => return Err(TlsMaterialError::PrivateKeyPemRejected),
        };
        if key.replace(parsed).is_some() {
            return Err(TlsMaterialError::MultiplePrivateKeys);
        }
    }
    key.ok_or(TlsMaterialError::PrivateKeyPemRejected)
}

/// `PemObject` intentionally skips unknown sections, but material files must
/// not hide another kind of credential or an unsupported section beside the
/// configured object.
fn validate_pem_labels(bytes: &[u8], kind: MaterialKind) -> Result<(), TlsMaterialError> {
    let mut section_count = 0;
    for line in bytes.split(|byte| *byte == b'\n' || *byte == b'\r') {
        if !line.starts_with(b"-----BEGIN") {
            continue;
        }
        let Some(label) = line
            .strip_prefix(b"-----BEGIN ")
            .and_then(|label| label.strip_suffix(b"-----"))
        else {
            return Err(match kind {
                MaterialKind::Certificate => TlsMaterialError::CertificatePemRejected,
                MaterialKind::PrivateKey => TlsMaterialError::PrivateKeyPemRejected,
            });
        };
        let supported = match kind {
            MaterialKind::Certificate => label == b"CERTIFICATE",
            MaterialKind::PrivateKey => {
                matches!(
                    label,
                    b"PRIVATE KEY" | b"RSA PRIVATE KEY" | b"EC PRIVATE KEY"
                )
            }
        };
        if !supported {
            return Err(match kind {
                MaterialKind::Certificate => TlsMaterialError::CertificatePemRejected,
                MaterialKind::PrivateKey => TlsMaterialError::PrivateKeyPemRejected,
            });
        }
        section_count += 1;
    }

    match kind {
        MaterialKind::Certificate if section_count == 0 => {
            Err(TlsMaterialError::CertificatePemRejected)
        }
        MaterialKind::PrivateKey if section_count == 0 => {
            Err(TlsMaterialError::PrivateKeyPemRejected)
        }
        MaterialKind::PrivateKey if section_count > 1 => Err(TlsMaterialError::MultiplePrivateKeys),
        _ => Ok(()),
    }
}

fn read_material(path: &Path, kind: MaterialKind) -> Result<Zeroizing<Vec<u8>>, TlsMaterialError> {
    let mut file = open_material(path)?;
    let metadata = file.metadata().map_err(|_| TlsMaterialError::ReadFailed)?;
    if !metadata.file_type().is_file() {
        return Err(TlsMaterialError::NotRegularFile);
    }
    match kind {
        MaterialKind::Certificate
            if (metadata.uid() != 0 && metadata.uid() != effective_uid())
                || metadata.mode() & 0o022 != 0 =>
        {
            return Err(TlsMaterialError::PermissionsRejected);
        }
        MaterialKind::PrivateKey
            if metadata.uid() != effective_uid() || metadata.mode() & 0o7777 != 0o600 =>
        {
            return Err(TlsMaterialError::PermissionsRejected);
        }
        _ => {}
    }
    if metadata.len() > MAX_TLS_MATERIAL_BYTES as u64 {
        return Err(TlsMaterialError::TooLarge);
    }

    let capacity = usize::try_from(metadata.len()).map_err(|_| TlsMaterialError::TooLarge)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
    let mut limited = (&mut file).take((MAX_TLS_MATERIAL_BYTES as u64) + 1);
    limited
        .read_to_end(&mut bytes)
        .map_err(|_| TlsMaterialError::ReadFailed)?;
    if bytes.len() > MAX_TLS_MATERIAL_BYTES {
        return Err(TlsMaterialError::TooLarge);
    }
    if bytes.is_empty() {
        return Err(match kind {
            MaterialKind::Certificate => TlsMaterialError::CertificatePemRejected,
            MaterialKind::PrivateKey => TlsMaterialError::PrivateKeyPemRejected,
        });
    }
    Ok(bytes)
}

fn open_material(path: &Path) -> Result<File, TlsMaterialError> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(TlsMaterialError::PathRejected);
    }

    let components = components.collect::<Vec<_>>();
    let Some((last, parents)) = components.split_last() else {
        return Err(TlsMaterialError::PathRejected);
    };
    let Component::Normal(last) = last else {
        return Err(TlsMaterialError::PathRejected);
    };

    let mut directory = open_root_directory()?;
    validate_trusted_directory(&directory)?;
    for component in parents {
        let Component::Normal(component) = component else {
            return Err(TlsMaterialError::PathRejected);
        };
        directory = open_directory_child(&directory, component.as_bytes())?;
        validate_trusted_directory(&directory)?;
    }

    let name =
        std::ffi::CString::new(last.as_bytes()).map_err(|_| TlsMaterialError::PathRejected)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | OPEN_FLAGS,
        )
    };
    if fd < 0 {
        return Err(TlsMaterialError::OpenFailed);
    }
    // SAFETY: openat returned a new owned descriptor on success.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_root_directory() -> Result<File, TlsMaterialError> {
    let name = c"/";
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | OPEN_FLAGS,
        )
    };
    if fd < 0 {
        return Err(TlsMaterialError::OpenFailed);
    }
    // SAFETY: open returned a new owned descriptor on success.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_child(directory: &File, name: &[u8]) -> Result<File, TlsMaterialError> {
    let name = std::ffi::CString::new(name).map_err(|_| TlsMaterialError::PathRejected)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | OPEN_FLAGS,
        )
    };
    if fd < 0 {
        return Err(TlsMaterialError::OpenFailed);
    }
    // SAFETY: openat returned a new owned descriptor on success.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_trusted_directory(directory: &File) -> Result<(), TlsMaterialError> {
    let metadata = directory
        .metadata()
        .map_err(|_| TlsMaterialError::ReadFailed)?;
    if !metadata.file_type().is_dir()
        || (metadata.uid() != 0 && metadata.uid() != effective_uid())
        || metadata.mode() & 0o022 != 0
    {
        return Err(TlsMaterialError::PermissionsRejected);
    }
    Ok(())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not access Rust memory.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{symlink, PermissionsExt},
    };

    use tempfile::TempDir;

    use super::{TlsMaterialError, TlsServerConfig, MAX_TLS_MATERIAL_BYTES};
    use crate::TlsConfig;

    const CERTIFICATE_CHAIN: &str = r#"-----BEGIN CERTIFICATE-----
MIIBszCCAVmgAwIBAgIUUg3keFcU1xXWK8BNVb1KynPulV8wCgYIKoZIzj0EAwIw
JjEkMCIGA1UEAwwbUnVzdGxzIFJvYnVzdCBSb290IC0gUnVuZyAyMCAXDTc1MDEw
MTAwMDAwMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxm
IHNpZ25lZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEud6w4gtZ0xbw
J3E69SSMy5TZfdIifl9L5ZY+hgEe4UiUsBWS32f6Y5NR5Jo8FO1f6o13b3+FvVHR
EHCGdvppL6NoMGYwFQYDVR0RBA4wDIIKZm9vYmFyLmNvbTAdBgNVHSUEFjAUBggr
BgEFBQcDAQYIKwYBBQUHAwIwHQYDVR0OBBYEFELvxbj5tD75n4pYFvJyr+c8qVEi
MA8GA1UdEwEB/wQFMAMBAQAwCgYIKoZIzj0EAwIDSAAwRQIhALxSSdUsrRFnwNMu
/doBqI8i8u5HdohVAheFTDwObkOMAiASSjULUtkWSD15u/7Sr01Wm9J1MpqW1pob
BVqU3CNRlA==
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
MIIBiTCCATCgAwIBAgIUHWiVYIvMMWoZEFYvSz46COf2FqowCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSUnVzdGxzIFJvYnVzdCBSb290MCAXDTc1MDEwMTAwMDAwMFoY
DzQwOTYwMTAxMDAwMDAwWjAmMSQwIgYDVQQDDBtSdXN0bHMgUm9idXN0IFJvb3Qg
LSBSdW5nIDIwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAATAOCcBD7dXjmAZ3te5
D47cCJ9ec93PWv7BKYIL826CJsKfXQOGrBTthLm77hXLhHu6uv8E5QXNLZpfowLQ
Do1ao0MwQTAPBgNVHQ8BAf8EBQMDB4QAMB0GA1UdDgQWBBRdza76r11Ok9vRmlg6
Nn/wL/N+jTAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIFmZrXeK
hnfkahocvkhhNT3cDv1LWf6WBoFaCiBwZXFPAiARaKRiSCMG7PCHmSqFe82TBVmL
odHGogAVax1Dh/aYAA==
-----END CERTIFICATE-----
"#;
    const PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgTbAQpfjAT46fgF4B
mP15n37woNG5ZNJmwcqsred/7tmhRANCAAS53rDiC1nTFvAncTr1JIzLlNl90iJ+
X0vllj6GAR7hSJSwFZLfZ/pjk1HkmjwU7V/qjXdvf4W9UdEQcIZ2+mkv
-----END PRIVATE KEY-----
"#;
    const UNKNOWN_PEM_SECTION: &str = "-----BEGIN FOO-----\nYWJj\n-----END FOO-----\n";

    fn fixture() -> (TempDir, TlsConfig) {
        let target_root =
            fs::canonicalize(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                .expect("target directory");
        let directory = tempfile::tempdir_in(target_root).expect("fixture directory");
        let certificate = directory.path().join("server-chain.pem");
        let private_key = directory.path().join("server-key.pem");
        fs::write(&certificate, CERTIFICATE_CHAIN).expect("certificate fixture");
        fs::write(&private_key, PRIVATE_KEY).expect("key fixture");
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600))
            .expect("key fixture permissions");
        let config = TlsConfig::new(&certificate, &private_key).expect("TLS paths");
        (directory, config)
    }

    #[test]
    fn loads_chain_and_matching_key_with_explicit_tls_policy() {
        let (_directory, config) = fixture();
        let loaded = TlsServerConfig::load(&config).expect("valid TLS material");
        let debug = format!("{loaded:?}");
        assert_eq!(debug, "TlsServerConfig { config: \"<redacted>\" }");
        assert_eq!(loaded.server_config().max_early_data_size, 0);
    }

    #[test]
    fn allows_a_non_writable_certificate_with_normal_read_permissions() {
        let (_directory, config) = fixture();
        fs::set_permissions(config.certificate_path(), fs::Permissions::from_mode(0o644))
            .expect("certificate permissions");
        TlsServerConfig::load(&config).expect("certificate read permissions are safe");
    }

    #[test]
    fn rejects_a_key_that_does_not_match_the_end_certificate() {
        let (directory, config) = fixture();
        let root_only = directory.path().join("root.pem");
        let root_certificate = CERTIFICATE_CHAIN
            .split_once("-----END CERTIFICATE-----\n")
            .expect("two certificate fixture sections")
            .1;
        fs::write(&root_only, root_certificate).expect("root certificate fixture");
        let root_config = TlsConfig::new(root_only, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&root_config),
            Err(TlsMaterialError::CertificateAndKeyRejected)
        ));
    }

    #[test]
    fn rejects_symlinked_material() {
        let (directory, config) = fixture();
        let certificate_link = directory.path().join("certificate-link.pem");
        symlink(config.certificate_path(), &certificate_link).expect("certificate symlink");
        let linked_config =
            TlsConfig::new(certificate_link, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&linked_config),
            Err(TlsMaterialError::OpenFailed)
        ));

        let real_directory = directory.path().join("real");
        fs::create_dir(&real_directory).expect("real directory");
        let directory_link = directory.path().join("directory-link");
        symlink(&real_directory, &directory_link).expect("directory symlink");
        let nested_certificate = directory_link.join("server-chain.pem");
        let nested_config =
            TlsConfig::new(nested_certificate, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&nested_config),
            Err(TlsMaterialError::OpenFailed)
        ));
    }

    #[test]
    fn rejects_non_regular_and_oversized_material() {
        let (directory, config) = fixture();
        let directory_config =
            TlsConfig::new(directory.path(), config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&directory_config),
            Err(TlsMaterialError::NotRegularFile)
        ));

        let oversized = directory.path().join("oversized.pem");
        fs::write(&oversized, vec![b'x'; MAX_TLS_MATERIAL_BYTES + 1]).expect("large fixture");
        let oversized_config =
            TlsConfig::new(oversized, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&oversized_config),
            Err(TlsMaterialError::TooLarge)
        ));

        fs::set_permissions(config.private_key_path(), fs::Permissions::from_mode(0o640))
            .expect("insecure key permissions");
        assert!(matches!(
            TlsServerConfig::load(&config),
            Err(TlsMaterialError::PermissionsRejected)
        ));
    }

    #[test]
    fn rejects_material_below_a_writable_ancestor() {
        let (directory, _config) = fixture();
        let writable = directory.path().join("writable");
        fs::create_dir(&writable).expect("writable directory");
        let certificate = writable.join("server-chain.pem");
        let private_key = writable.join("server-key.pem");
        fs::write(&certificate, CERTIFICATE_CHAIN).expect("certificate fixture");
        fs::write(&private_key, PRIVATE_KEY).expect("key fixture");
        fs::set_permissions(&private_key, fs::Permissions::from_mode(0o600))
            .expect("key fixture permissions");
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777))
            .expect("writable directory permissions");
        let nested_config = TlsConfig::new(certificate, private_key).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&nested_config),
            Err(TlsMaterialError::PermissionsRejected)
        ));
    }

    #[test]
    fn rejects_malformed_pem_and_multiple_private_keys() {
        let (directory, config) = fixture();
        let malformed = directory.path().join("malformed.pem");
        fs::write(&malformed, b"not PEM").expect("malformed fixture");
        let malformed_config =
            TlsConfig::new(&malformed, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&malformed_config),
            Err(TlsMaterialError::CertificatePemRejected)
        ));

        let missing = directory.path().join("missing.pem");
        let missing_config = TlsConfig::new(missing, config.private_key_path()).expect("TLS paths");
        let error = TlsServerConfig::load(&missing_config).expect_err("missing certificate");
        assert_eq!(error, TlsMaterialError::OpenFailed);
        assert!(!format!("{error:?}").contains("missing.pem"));

        let malformed_key = directory.path().join("malformed-key.pem");
        fs::write(&malformed_key, b"not PEM").expect("malformed key fixture");
        fs::set_permissions(&malformed_key, fs::Permissions::from_mode(0o600))
            .expect("malformed key permissions");
        let malformed_key_config =
            TlsConfig::new(config.certificate_path(), &malformed_key).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&malformed_key_config),
            Err(TlsMaterialError::PrivateKeyPemRejected)
        ));

        let multiple_keys = directory.path().join("multiple-keys.pem");
        fs::write(&multiple_keys, format!("{PRIVATE_KEY}{PRIVATE_KEY}")).expect("key fixture");
        fs::set_permissions(&multiple_keys, fs::Permissions::from_mode(0o600))
            .expect("multiple key permissions");
        let multiple_config =
            TlsConfig::new(config.certificate_path(), multiple_keys).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&multiple_config),
            Err(TlsMaterialError::MultiplePrivateKeys)
        ));

        let mixed_key = directory.path().join("key-with-certificate.pem");
        fs::write(&mixed_key, format!("{PRIVATE_KEY}{CERTIFICATE_CHAIN}"))
            .expect("mixed key fixture");
        fs::set_permissions(&mixed_key, fs::Permissions::from_mode(0o600))
            .expect("mixed key permissions");
        let mixed_config = TlsConfig::new(config.certificate_path(), mixed_key).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&mixed_config),
            Err(TlsMaterialError::PrivateKeyPemRejected)
        ));

        let certificate_with_key = directory.path().join("certificate-with-key.pem");
        fs::write(
            &certificate_with_key,
            format!("{CERTIFICATE_CHAIN}{PRIVATE_KEY}"),
        )
        .expect("certificate with key fixture");
        let certificate_with_key_config =
            TlsConfig::new(certificate_with_key, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&certificate_with_key_config),
            Err(TlsMaterialError::CertificatePemRejected)
        ));

        let certificate_with_unknown = directory.path().join("certificate-with-unknown.pem");
        fs::write(
            &certificate_with_unknown,
            format!("{CERTIFICATE_CHAIN}{UNKNOWN_PEM_SECTION}"),
        )
        .expect("certificate with unknown section fixture");
        let certificate_with_unknown_config =
            TlsConfig::new(certificate_with_unknown, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&certificate_with_unknown_config),
            Err(TlsMaterialError::CertificatePemRejected)
        ));

        let key_with_unknown = directory.path().join("key-with-unknown.pem");
        fs::write(
            &key_with_unknown,
            format!("{PRIVATE_KEY}{UNKNOWN_PEM_SECTION}"),
        )
        .expect("key with unknown section fixture");
        fs::set_permissions(&key_with_unknown, fs::Permissions::from_mode(0o600))
            .expect("key with unknown section permissions");
        let key_with_unknown_config =
            TlsConfig::new(config.certificate_path(), key_with_unknown).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&key_with_unknown_config),
            Err(TlsMaterialError::PrivateKeyPemRejected)
        ));

        let unsupported_key = directory.path().join("key-with-unsupported-label.pem");
        fs::write(
            &unsupported_key,
            format!(
                "{PRIVATE_KEY}-----BEGIN ENCRYPTED PRIVATE KEY-----\n-----END ENCRYPTED PRIVATE KEY-----\n"
            ),
        )
        .expect("unsupported key fixture");
        fs::set_permissions(&unsupported_key, fs::Permissions::from_mode(0o600))
            .expect("unsupported key permissions");
        let unsupported_config =
            TlsConfig::new(config.certificate_path(), unsupported_key).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&unsupported_config),
            Err(TlsMaterialError::PrivateKeyPemRejected)
        ));

        let malformed_chain = directory.path().join("malformed-chain.pem");
        let first_certificate = CERTIFICATE_CHAIN
            .split_once("-----END CERTIFICATE-----\n")
            .expect("two certificate fixture sections")
            .0;
        let malformed_chain_contents = format!(
            "{first_certificate}-----END CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\nnot-base64\n-----END CERTIFICATE-----\n"
        );
        fs::write(&malformed_chain, malformed_chain_contents).expect("malformed chain fixture");
        let malformed_chain_config =
            TlsConfig::new(malformed_chain, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&malformed_chain_config),
            Err(TlsMaterialError::CertificatePemRejected)
        ));
    }

    #[test]
    fn rejects_parent_path_components() {
        let (directory, config) = fixture();
        let parent = directory
            .path()
            .join("..")
            .join(directory.path().file_name().expect("fixture name"))
            .join("server-chain.pem");
        let parent_config = TlsConfig::new(parent, config.private_key_path()).expect("TLS paths");
        assert!(matches!(
            TlsServerConfig::load(&parent_config),
            Err(TlsMaterialError::PathRejected)
        ));
    }
}
