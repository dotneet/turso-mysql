//! Fixed-size metadata stored beside a MySQL-owned SQLite database.
//!
//! The metadata is separate from both the SQLite main file and its WAL. That
//! keeps the SQLite bytes portable while preserving the proof the MySQL
//! frontend needs before it opens a descriptor pair.

const MAGIC: &[u8; 17] = b"TURSO_MYSQL_META\0";
const VERSION: u8 = 1;
const OWNER_MYSQL: u8 = 1;
const NAME_POLICY_LOWER_CASE_TABLE_NAMES_1: u8 = 1;
const RESERVED_BYTES: usize = 4;
const IDENTITY_BYTES: usize = 16;
const CHECKSUM_BYTES: usize = 4;
const HEADER_BYTES: usize = MAGIC.len() + 4 + RESERVED_BYTES;
const CHECKSUM_OFFSET: usize = HEADER_BYTES + IDENTITY_BYTES;
pub(super) const ENCODED_BYTES: usize = CHECKSUM_OFFSET + CHECKSUM_BYTES;

/// The SQLite artifact whose identity this sidecar proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetadataArtifactRole {
    Main,
    Wal,
}

impl MetadataArtifactRole {
    const fn as_byte(self) -> u8 {
        match self {
            Self::Main => 1,
            Self::Wal => 2,
        }
    }

    fn from_byte(value: u8) -> Result<Self, DatabaseMetadataError> {
        match value {
            1 => Ok(Self::Main),
            2 => Ok(Self::Wal),
            _ => Err(DatabaseMetadataError::InvalidArtifactRole),
        }
    }
}

/// MySQL ownership, policy, role, and durable identity for one SQLite file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DatabaseMetadata {
    durable_identity: [u8; IDENTITY_BYTES],
    role: MetadataArtifactRole,
}

impl DatabaseMetadata {
    pub(super) fn new(
        durable_identity: [u8; IDENTITY_BYTES],
        role: MetadataArtifactRole,
    ) -> Result<Self, DatabaseMetadataError> {
        if durable_identity.iter().all(|byte| *byte == 0) {
            return Err(DatabaseMetadataError::ZeroDurableIdentity);
        }
        Ok(Self {
            durable_identity,
            role,
        })
    }

    pub(super) fn durable_identity(self) -> [u8; IDENTITY_BYTES] {
        self.durable_identity
    }

    pub(super) const fn role(self) -> MetadataArtifactRole {
        self.role
    }

    pub(super) fn encode(self) -> [u8; ENCODED_BYTES] {
        let mut encoded = [0; ENCODED_BYTES];
        encoded[..MAGIC.len()].copy_from_slice(MAGIC);
        encoded[MAGIC.len()] = VERSION;
        encoded[MAGIC.len() + 1] = OWNER_MYSQL;
        encoded[MAGIC.len() + 2] = NAME_POLICY_LOWER_CASE_TABLE_NAMES_1;
        encoded[MAGIC.len() + 3] = self.role.as_byte();
        encoded[HEADER_BYTES..CHECKSUM_OFFSET].copy_from_slice(&self.durable_identity);
        let checksum = crc32(&encoded[..CHECKSUM_OFFSET]).to_be_bytes();
        encoded[CHECKSUM_OFFSET..].copy_from_slice(&checksum);
        encoded
    }

    pub(super) fn decode(encoded: &[u8]) -> Result<Self, DatabaseMetadataError> {
        if encoded.len() != ENCODED_BYTES {
            return Err(DatabaseMetadataError::InvalidLength);
        }
        if &encoded[..MAGIC.len()] != MAGIC {
            return Err(DatabaseMetadataError::InvalidMagic);
        }
        if encoded[MAGIC.len()] != VERSION {
            return Err(DatabaseMetadataError::UnsupportedVersion);
        }
        if encoded[MAGIC.len() + 1] != OWNER_MYSQL {
            return Err(DatabaseMetadataError::InvalidOwner);
        }
        if encoded[MAGIC.len() + 2] != NAME_POLICY_LOWER_CASE_TABLE_NAMES_1 {
            return Err(DatabaseMetadataError::InvalidNamePolicy);
        }
        let role = MetadataArtifactRole::from_byte(encoded[MAGIC.len() + 3])?;
        if encoded[MAGIC.len() + 4..HEADER_BYTES]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(DatabaseMetadataError::NonZeroReservedBytes);
        }
        let expected_checksum = u32::from_be_bytes(
            encoded[CHECKSUM_OFFSET..]
                .try_into()
                .expect("the fixed metadata length includes a four-byte checksum"),
        );
        if crc32(&encoded[..CHECKSUM_OFFSET]) != expected_checksum {
            return Err(DatabaseMetadataError::InvalidChecksum);
        }
        let durable_identity = encoded[HEADER_BYTES..CHECKSUM_OFFSET]
            .try_into()
            .expect("the fixed metadata length includes a 128-bit identity");
        Self::new(durable_identity, role)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DatabaseMetadataError {
    InvalidLength,
    InvalidMagic,
    UnsupportedVersion,
    InvalidOwner,
    InvalidNamePolicy,
    InvalidArtifactRole,
    NonZeroReservedBytes,
    InvalidChecksum,
    ZeroDurableIdentity,
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{
        crc32, DatabaseMetadata, DatabaseMetadataError, MetadataArtifactRole, CHECKSUM_OFFSET,
        HEADER_BYTES,
    };

    const IDENTITY: [u8; 16] = [
        0x9f, 0x19, 0xe1, 0x56, 0x45, 0xa4, 0x49, 0x8e, 0x8e, 0x64, 0x76, 0x76, 0xbc, 0x9d, 0xec,
        0x5f,
    ];

    fn metadata(role: MetadataArtifactRole) -> DatabaseMetadata {
        DatabaseMetadata::new(IDENTITY, role).unwrap()
    }

    fn with_checksum(mut encoded: Vec<u8>) -> Vec<u8> {
        let checksum = crc32(&encoded[..CHECKSUM_OFFSET]).to_be_bytes();
        encoded[CHECKSUM_OFFSET..].copy_from_slice(&checksum);
        encoded
    }

    #[test]
    fn metadata_round_trips_a_nonzero_128_bit_identity_for_each_artifact_role() {
        for role in [MetadataArtifactRole::Main, MetadataArtifactRole::Wal] {
            let metadata = metadata(role);
            let encoded = metadata.encode();
            assert_eq!(DatabaseMetadata::decode(&encoded), Ok(metadata));
            assert_eq!(metadata.durable_identity(), IDENTITY);
            assert_eq!(metadata.role(), role);
        }
    }

    #[test]
    fn metadata_rejects_zero_durable_identity_when_created_or_decoded() {
        assert_eq!(
            DatabaseMetadata::new([0; 16], MetadataArtifactRole::Main),
            Err(DatabaseMetadataError::ZeroDurableIdentity)
        );

        let mut encoded = metadata(MetadataArtifactRole::Main).encode().to_vec();
        encoded[HEADER_BYTES..CHECKSUM_OFFSET].fill(0);
        assert_eq!(
            DatabaseMetadata::decode(&with_checksum(encoded)),
            Err(DatabaseMetadataError::ZeroDurableIdentity)
        );
    }

    #[test]
    fn metadata_rejects_torn_extra_and_corrupted_bytes() {
        let encoded = metadata(MetadataArtifactRole::Main).encode();
        for length in 0..encoded.len() {
            assert_eq!(
                DatabaseMetadata::decode(&encoded[..length]),
                Err(DatabaseMetadataError::InvalidLength),
                "torn length {length}"
            );
        }
        let mut extra = encoded.to_vec();
        extra.push(0);
        assert_eq!(
            DatabaseMetadata::decode(&extra),
            Err(DatabaseMetadataError::InvalidLength)
        );

        for index in 0..encoded.len() {
            let mut corrupt = encoded;
            corrupt[index] ^= 0x80;
            assert!(
                DatabaseMetadata::decode(&corrupt).is_err(),
                "corruption at byte {index} was accepted"
            );
        }
    }

    #[test]
    fn metadata_rejects_unknown_versions_owner_policy_role_and_reserved_bits() {
        let encoded = metadata(MetadataArtifactRole::Main).encode();
        let cases = [
            (
                super::MAGIC.len(),
                DatabaseMetadataError::UnsupportedVersion,
            ),
            (super::MAGIC.len() + 1, DatabaseMetadataError::InvalidOwner),
            (
                super::MAGIC.len() + 2,
                DatabaseMetadataError::InvalidNamePolicy,
            ),
            (
                super::MAGIC.len() + 3,
                DatabaseMetadataError::InvalidArtifactRole,
            ),
            (
                super::MAGIC.len() + 4,
                DatabaseMetadataError::NonZeroReservedBytes,
            ),
        ];
        for (offset, expected_error) in cases {
            let mut malformed = encoded.to_vec();
            malformed[offset] = 0xff;
            assert_eq!(
                DatabaseMetadata::decode(&with_checksum(malformed)),
                Err(expected_error)
            );
        }
    }

    #[test]
    fn metadata_rejects_a_checksum_that_does_not_cover_the_header_and_identity() {
        let mut encoded = metadata(MetadataArtifactRole::Wal).encode();
        encoded[CHECKSUM_OFFSET] ^= 1;
        assert_eq!(
            DatabaseMetadata::decode(&encoded),
            Err(DatabaseMetadataError::InvalidChecksum)
        );
    }

    #[test]
    fn metadata_uses_the_standard_ieee_crc32_polynomial() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }
}
