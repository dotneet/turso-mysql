//! Strict durable encoding for the account and privilege snapshot.
//!
//! The format is only a corruption-detection boundary. The trailing CRC32 is
//! not an authenticity check and must not be used as an authorization proof.

use std::{collections::BTreeSet, error::Error, fmt};

use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{MAX_CLIENT_USERNAME_LENGTH, SHA256_DIGEST_LENGTH};

const MAGIC: [u8; 8] = *b"TURSAUTH";
const VERSION: u16 = 1;
const HEADER_LENGTH: usize = 68;
const CHECKSUM_LENGTH: usize = 4;
const ACCOUNT_FIXED_LENGTH: usize = 2 + 2 + 1 + 1 + 2 + SHA256_DIGEST_LENGTH * 2;
const DATABASE_GRANT_FIXED_LENGTH: usize = 2 + 1 + 1;
const MAX_ACCOUNTS: usize = 8192;
const MAX_DATABASE_GRANTS: usize = 65_536;
const MAX_RETIRED: usize = 65_536;
const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

const ENABLED_FLAG: u8 = 0x01;
const GLOBAL_CONNECT_FLAG: u8 = 0x02;
const GLOBAL_LIST_FLAG: u8 = 0x04;
const ACCOUNT_FLAGS: u8 = ENABLED_FLAG | GLOBAL_CONNECT_FLAG | GLOBAL_LIST_FLAG;
pub(crate) const DATABASE_GRANT_CONNECT_BIT: u8 = 0x01;
pub(crate) const DATABASE_GRANT_QUERY_BIT: u8 = 0x02;
pub(crate) const DATABASE_GRANT_CREATE_BIT: u8 = 0x04;
pub(crate) const DATABASE_GRANT_DROP_BIT: u8 = 0x08;
const DATABASE_GRANT_BITS: u8 = DATABASE_GRANT_CONNECT_BIT
    | DATABASE_GRANT_QUERY_BIT
    | DATABASE_GRANT_CREATE_BIT
    | DATABASE_GRANT_DROP_BIT;

/// The durable account and privilege snapshot.
#[derive(PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct StoredAuthSnapshot {
    pub(crate) store_id: [u8; SHA256_DIGEST_LENGTH],
    pub(crate) revision: u64,
    pub(crate) retired_account_ids: Vec<[u8; SHA256_DIGEST_LENGTH]>,
    pub(crate) accounts: Vec<StoredAccountRecord>,
}

/// One account and all privileges stored in the snapshot.
#[derive(PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct StoredAccountRecord {
    pub(crate) username: String,
    pub(crate) account_id: [u8; SHA256_DIGEST_LENGTH],
    pub(crate) enabled: bool,
    pub(crate) verifier: Box<[u8; SHA256_DIGEST_LENGTH]>,
    pub(crate) global_connect: bool,
    pub(crate) global_list: bool,
    pub(crate) database_grants: Vec<StoredDatabaseGrant>,
}

/// Privileges for one canonical logical database.
#[derive(Debug, Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub(crate) struct StoredDatabaseGrant {
    pub(crate) database_name: String,
    pub(crate) bits: u8,
}

impl fmt::Debug for StoredAuthSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAuthSnapshot")
            .field("store_id", &"<redacted>")
            .field("revision", &self.revision)
            .field("retired_account_ids", &"<redacted>")
            .field("accounts", &self.accounts)
            .finish()
    }
}

impl fmt::Debug for StoredAccountRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAccountRecord")
            .field("username", &self.username)
            .field("account_id", &"<redacted>")
            .field("enabled", &self.enabled)
            .field("verifier", &"<redacted>")
            .field("global_connect", &self.global_connect)
            .field("global_list", &self.global_list)
            .field("database_grants", &self.database_grants)
            .finish()
    }
}

/// Errors raised by the strict snapshot format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountStoreFormatError {
    /// The input or encoded snapshot exceeds the format limit.
    TooLarge,
    /// The input ended before a complete field was available.
    Truncated,
    /// The header or record length is inconsistent with the input.
    InvalidLength,
    /// Bytes remained after the complete snapshot.
    TrailingBytes,
    /// The header magic is not this format's magic.
    InvalidMagic,
    /// The format version is not supported.
    UnsupportedVersion,
    /// A reserved field was not zero.
    NonZeroReserved,
    /// The account count exceeds the supported limit.
    TooManyAccounts,
    /// The retired-account count exceeds the supported limit.
    TooManyRetiredAccountIds,
    /// The username is empty, too long, or contains NUL.
    InvalidUsername,
    /// A username is not strictly greater than the preceding username.
    UnsortedUsername,
    /// A username appears more than once.
    DuplicateUsername,
    /// An account ID is all zero.
    ZeroAccountId,
    /// The store ID is all zero.
    ZeroStoreId,
    /// A retired account ID is all zero.
    ZeroRetiredAccountId,
    /// An account ID appears more than once.
    DuplicateAccountId,
    /// A retired account ID appears more than once.
    DuplicateRetiredAccountId,
    /// Retired account IDs are not strictly sorted.
    UnsortedRetiredAccountId,
    /// An active account reuses a retired account ID.
    ActiveRetiredAccountIdOverlap,
    /// A boolean or privilege bit field contains an invalid value.
    InvalidFlags,
    /// A database name is invalid or is not canonical.
    InvalidDatabaseName,
    /// A database grant is not strictly ordered or is duplicated.
    UnsortedDatabaseGrant,
    /// A database grant has no known privilege bit set.
    InvalidDatabaseGrant,
    /// One account has more database grants than the wire count can hold.
    TooManyDatabaseGrants,
    /// A text field is not valid UTF-8.
    InvalidUtf8,
    /// The CRC32 does not match the header and body.
    ChecksumMismatch,
}

impl fmt::Display for AccountStoreFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLarge => "account snapshot is too large",
            Self::Truncated => "account snapshot is truncated",
            Self::InvalidLength => "account snapshot has an invalid length",
            Self::TrailingBytes => "account snapshot has trailing bytes",
            Self::InvalidMagic => "account snapshot has invalid magic",
            Self::UnsupportedVersion => "account snapshot version is unsupported",
            Self::NonZeroReserved => "account snapshot has non-zero reserved fields",
            Self::TooManyAccounts => "account snapshot has too many accounts",
            Self::TooManyRetiredAccountIds => "account snapshot has too many retired account IDs",
            Self::InvalidUsername => "account snapshot has an invalid username",
            Self::UnsortedUsername => "account snapshot usernames are not sorted",
            Self::DuplicateUsername => "account snapshot has a duplicate username",
            Self::ZeroAccountId => "account snapshot has a zero account ID",
            Self::ZeroStoreId => "account snapshot has a zero store ID",
            Self::ZeroRetiredAccountId => "account snapshot has a zero retired account ID",
            Self::DuplicateAccountId => "account snapshot has a duplicate account ID",
            Self::DuplicateRetiredAccountId => {
                "account snapshot has a duplicate retired account ID"
            }
            Self::UnsortedRetiredAccountId => "account snapshot retired account IDs are not sorted",
            Self::ActiveRetiredAccountIdOverlap => {
                "account snapshot has an active and retired account ID overlap"
            }
            Self::InvalidFlags => "account snapshot has invalid flags",
            Self::InvalidDatabaseName => "account snapshot has an invalid database name",
            Self::UnsortedDatabaseGrant => "account snapshot database grants are not sorted",
            Self::InvalidDatabaseGrant => "account snapshot has an invalid database grant",
            Self::TooManyDatabaseGrants => "account snapshot has too many database grants",
            Self::InvalidUtf8 => "account snapshot has invalid UTF-8",
            Self::ChecksumMismatch => "account snapshot checksum mismatch",
        };
        f.write_str(message)
    }
}

impl Error for AccountStoreFormatError {}

impl StoredAuthSnapshot {
    /// Encodes a canonical snapshot using big-endian integer fields.
    pub(crate) fn encode(&self) -> Result<Zeroizing<Vec<u8>>, AccountStoreFormatError> {
        validate_store_id(&self.store_id)?;
        if self.accounts.len() > MAX_ACCOUNTS {
            return Err(AccountStoreFormatError::TooManyAccounts);
        }
        if self.retired_account_ids.len() > MAX_RETIRED {
            return Err(AccountStoreFormatError::TooManyRetiredAccountIds);
        }
        let mut retired_account_ids = self.retired_account_ids.clone();
        retired_account_ids.sort_unstable();
        validate_retired_account_ids(&retired_account_ids)?;

        let mut accounts = self.accounts.iter().collect::<Vec<_>>();
        accounts.sort_by(|left, right| left.username.as_bytes().cmp(right.username.as_bytes()));

        let mut seen_account_ids = BTreeSet::new();
        let mut previous_username: Option<&str> = None;
        let mut total_database_grants = 0usize;
        let mut body_length = retired_account_ids
            .len()
            .checked_mul(SHA256_DIGEST_LENGTH)
            .ok_or(AccountStoreFormatError::TooLarge)?;
        for account in &accounts {
            validate_account(account, &mut seen_account_ids)?;
            if retired_account_ids
                .binary_search(&account.account_id)
                .is_ok()
            {
                return Err(AccountStoreFormatError::ActiveRetiredAccountIdOverlap);
            }
            if previous_username == Some(account.username.as_str()) {
                return Err(AccountStoreFormatError::DuplicateUsername);
            }
            previous_username = Some(account.username.as_str());

            let mut grants = account.database_grants.iter().collect::<Vec<_>>();
            if grants.len() > u16::MAX as usize {
                return Err(AccountStoreFormatError::TooManyDatabaseGrants);
            }
            total_database_grants = total_database_grants
                .checked_add(grants.len())
                .ok_or(AccountStoreFormatError::TooManyDatabaseGrants)?;
            if total_database_grants > MAX_DATABASE_GRANTS {
                return Err(AccountStoreFormatError::TooManyDatabaseGrants);
            }
            grants.sort_by(|left, right| {
                left.database_name
                    .as_bytes()
                    .cmp(right.database_name.as_bytes())
            });
            validate_grants(&grants)?;

            let account_length = account_length(account, &grants)?;
            body_length = body_length
                .checked_add(4)
                .and_then(|length| length.checked_add(account_length))
                .ok_or(AccountStoreFormatError::TooLarge)?;
        }

        let encoded_length = HEADER_LENGTH
            .checked_add(body_length)
            .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
            .ok_or(AccountStoreFormatError::TooLarge)?;
        if encoded_length > MAX_SNAPSHOT_BYTES || body_length > u32::MAX as usize {
            return Err(AccountStoreFormatError::TooLarge);
        }

        let mut body = Zeroizing::new(Vec::with_capacity(body_length));
        body.extend(retired_account_ids.iter().flatten());
        for account in &accounts {
            let mut grants = account.database_grants.iter().collect::<Vec<_>>();
            grants.sort_by(|left, right| {
                left.database_name
                    .as_bytes()
                    .cmp(right.database_name.as_bytes())
            });
            let account_length = account_length(account, &grants)?;
            push_u32(&mut body, account_length as u32);
            push_u16(&mut body, account.username.len() as u16);
            push_u16(&mut body, grants.len() as u16);
            body.push(account_flags(account));
            body.push(0);
            push_u16(&mut body, 0);
            body.extend_from_slice(&account.account_id);
            body.extend_from_slice(account.verifier.as_ref());
            body.extend_from_slice(account.username.as_bytes());
            for grant in grants {
                push_u16(&mut body, grant.database_name.len() as u16);
                body.push(grant.bits);
                body.push(0);
                body.extend_from_slice(grant.database_name.as_bytes());
            }
        }
        debug_assert_eq!(body.len(), body_length);

        let mut encoded = Zeroizing::new(Vec::with_capacity(encoded_length));
        encoded.extend_from_slice(&MAGIC);
        push_u16(&mut encoded, VERSION);
        push_u16(&mut encoded, HEADER_LENGTH as u16);
        encoded.extend_from_slice(&self.store_id);
        push_u64(&mut encoded, self.revision);
        push_u32(&mut encoded, self.accounts.len() as u32);
        push_u32(&mut encoded, retired_account_ids.len() as u32);
        push_u32(&mut encoded, body_length as u32);
        push_u32(&mut encoded, 0);
        encoded.extend_from_slice(&body);
        let checksum = crc32(&encoded);
        push_u32(&mut encoded, checksum);
        Ok(encoded)
    }

    /// Decodes and strictly validates one complete snapshot.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, AccountStoreFormatError> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(AccountStoreFormatError::TooLarge);
        }
        if bytes.len() < HEADER_LENGTH + CHECKSUM_LENGTH {
            return Err(AccountStoreFormatError::Truncated);
        }

        if bytes[..8] != MAGIC {
            return Err(AccountStoreFormatError::InvalidMagic);
        }
        if read_u16(bytes, 8)? != VERSION {
            return Err(AccountStoreFormatError::UnsupportedVersion);
        }
        if read_u16(bytes, 10)? as usize != HEADER_LENGTH {
            return Err(AccountStoreFormatError::InvalidLength);
        }

        let store_id = read_array(bytes, 12)?;
        validate_store_id(&store_id)?;
        let revision = read_u64(bytes, 44)?;
        let account_count = read_u32(bytes, 52)? as usize;
        if account_count > MAX_ACCOUNTS {
            return Err(AccountStoreFormatError::TooManyAccounts);
        }
        let retired_count = read_u32(bytes, 56)? as usize;
        if retired_count > MAX_RETIRED {
            return Err(AccountStoreFormatError::TooManyRetiredAccountIds);
        }
        let body_length = read_u32(bytes, 60)? as usize;
        if read_u32(bytes, 64)? != 0 {
            return Err(AccountStoreFormatError::NonZeroReserved);
        }

        let expected_length = HEADER_LENGTH
            .checked_add(body_length)
            .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
            .ok_or(AccountStoreFormatError::InvalidLength)?;
        if expected_length > MAX_SNAPSHOT_BYTES {
            return Err(AccountStoreFormatError::TooLarge);
        }
        if bytes.len() < expected_length {
            return Err(AccountStoreFormatError::Truncated);
        }
        if bytes.len() > expected_length {
            return Err(AccountStoreFormatError::TrailingBytes);
        }

        let checksum_offset = expected_length - CHECKSUM_LENGTH;
        let expected_checksum = read_u32(bytes, checksum_offset)?;
        if crc32(&bytes[..checksum_offset]) != expected_checksum {
            return Err(AccountStoreFormatError::ChecksumMismatch);
        }

        let body = &bytes[HEADER_LENGTH..checksum_offset];
        let mut reader = Reader::new(body);
        let retired_bytes = retired_count
            .checked_mul(SHA256_DIGEST_LENGTH)
            .ok_or(AccountStoreFormatError::InvalidLength)?;
        let minimum_account_bytes = account_count
            .checked_mul(4 + ACCOUNT_FIXED_LENGTH)
            .ok_or(AccountStoreFormatError::InvalidLength)?;
        if retired_bytes
            .checked_add(minimum_account_bytes)
            .is_none_or(|minimum| minimum > body.len())
        {
            return Err(AccountStoreFormatError::InvalidLength);
        }
        let mut retired_account_ids = Vec::with_capacity(retired_count);
        let mut previous_retired_id: Option<[u8; SHA256_DIGEST_LENGTH]> = None;
        for _ in 0..retired_count {
            let retired_id = reader.read_array::<SHA256_DIGEST_LENGTH>()?;
            if retired_id.iter().all(|byte| *byte == 0) {
                return Err(AccountStoreFormatError::ZeroRetiredAccountId);
            }
            if let Some(previous) = previous_retired_id {
                match previous.cmp(&retired_id) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => {
                        return Err(AccountStoreFormatError::DuplicateRetiredAccountId);
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(AccountStoreFormatError::UnsortedRetiredAccountId);
                    }
                }
            }
            previous_retired_id = Some(retired_id);
            retired_account_ids.push(retired_id);
        }
        let mut accounts = Vec::with_capacity(account_count);
        let mut seen_account_ids = BTreeSet::new();
        let mut previous_username: Option<String> = None;
        let mut total_database_grants = 0usize;

        for _ in 0..account_count {
            let account_length = reader.read_u32()? as usize;
            if account_length < ACCOUNT_FIXED_LENGTH || account_length > reader.remaining() {
                return Err(AccountStoreFormatError::InvalidLength);
            }
            let record_bytes = reader.read_exact(account_length)?;
            let account = decode_account(record_bytes, &mut seen_account_ids)?;
            if retired_account_ids
                .binary_search(&account.account_id)
                .is_ok()
            {
                return Err(AccountStoreFormatError::ActiveRetiredAccountIdOverlap);
            }
            total_database_grants = total_database_grants
                .checked_add(account.database_grants.len())
                .ok_or(AccountStoreFormatError::TooManyDatabaseGrants)?;
            if total_database_grants > MAX_DATABASE_GRANTS {
                return Err(AccountStoreFormatError::TooManyDatabaseGrants);
            }
            if let Some(previous) = previous_username.as_deref() {
                match previous.as_bytes().cmp(account.username.as_bytes()) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => {
                        return Err(AccountStoreFormatError::DuplicateUsername);
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(AccountStoreFormatError::UnsortedUsername);
                    }
                }
            }
            previous_username = Some(account.username.clone());
            accounts.push(account);
        }
        if reader.remaining() != 0 {
            return Err(AccountStoreFormatError::InvalidLength);
        }

        Ok(Self {
            store_id,
            revision,
            retired_account_ids,
            accounts,
        })
    }
}

fn validate_account(
    account: &StoredAccountRecord,
    seen_account_ids: &mut BTreeSet<[u8; SHA256_DIGEST_LENGTH]>,
) -> Result<(), AccountStoreFormatError> {
    validate_username(&account.username)?;
    if account.account_id.iter().all(|byte| *byte == 0) {
        return Err(AccountStoreFormatError::ZeroAccountId);
    }
    if !seen_account_ids.insert(account.account_id) {
        return Err(AccountStoreFormatError::DuplicateAccountId);
    }
    Ok(())
}

fn validate_store_id(store_id: &[u8; SHA256_DIGEST_LENGTH]) -> Result<(), AccountStoreFormatError> {
    if store_id.iter().all(|byte| *byte == 0) {
        Err(AccountStoreFormatError::ZeroStoreId)
    } else {
        Ok(())
    }
}

fn validate_retired_account_ids(
    retired_account_ids: &[[u8; SHA256_DIGEST_LENGTH]],
) -> Result<(), AccountStoreFormatError> {
    let mut previous: Option<&[u8; SHA256_DIGEST_LENGTH]> = None;
    for retired_id in retired_account_ids {
        if retired_id.iter().all(|byte| *byte == 0) {
            return Err(AccountStoreFormatError::ZeroRetiredAccountId);
        }
        if let Some(previous) = previous {
            match previous.cmp(retired_id) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(AccountStoreFormatError::DuplicateRetiredAccountId);
                }
                std::cmp::Ordering::Greater => {
                    return Err(AccountStoreFormatError::UnsortedRetiredAccountId);
                }
            }
        }
        previous = Some(retired_id);
    }
    Ok(())
}

fn validate_username(username: &str) -> Result<(), AccountStoreFormatError> {
    if username.is_empty()
        || username.len() > MAX_CLIENT_USERNAME_LENGTH
        || username.as_bytes().contains(&0)
    {
        return Err(AccountStoreFormatError::InvalidUsername);
    }
    Ok(())
}

fn validate_grants(grants: &[&StoredDatabaseGrant]) -> Result<(), AccountStoreFormatError> {
    let mut previous_name: Option<&str> = None;
    for grant in grants {
        if grant.bits == 0 || grant.bits & !DATABASE_GRANT_BITS != 0 {
            return Err(AccountStoreFormatError::InvalidDatabaseGrant);
        }
        validate_database_name(&grant.database_name)?;
        if let Some(previous) = previous_name {
            match previous.as_bytes().cmp(grant.database_name.as_bytes()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    return Err(AccountStoreFormatError::UnsortedDatabaseGrant);
                }
                std::cmp::Ordering::Greater => {
                    return Err(AccountStoreFormatError::UnsortedDatabaseGrant);
                }
            }
        }
        previous_name = Some(&grant.database_name);
    }
    Ok(())
}

fn account_flags(account: &StoredAccountRecord) -> u8 {
    ((account.enabled as u8) * ENABLED_FLAG)
        | ((account.global_connect as u8) * GLOBAL_CONNECT_FLAG)
        | ((account.global_list as u8) * GLOBAL_LIST_FLAG)
}

fn account_length(
    account: &StoredAccountRecord,
    grants: &[&StoredDatabaseGrant],
) -> Result<usize, AccountStoreFormatError> {
    let mut length = ACCOUNT_FIXED_LENGTH
        .checked_add(account.username.len())
        .ok_or(AccountStoreFormatError::TooLarge)?;
    for grant in grants {
        length = length
            .checked_add(DATABASE_GRANT_FIXED_LENGTH)
            .and_then(|length| length.checked_add(grant.database_name.len()))
            .ok_or(AccountStoreFormatError::TooLarge)?;
    }
    if length > u32::MAX as usize {
        return Err(AccountStoreFormatError::TooLarge);
    }
    Ok(length)
}

fn decode_account(
    bytes: &[u8],
    seen_account_ids: &mut BTreeSet<[u8; SHA256_DIGEST_LENGTH]>,
) -> Result<StoredAccountRecord, AccountStoreFormatError> {
    let mut reader = Reader::new(bytes);
    let username_length = reader.read_u16()? as usize;
    let grant_count = reader.read_u16()? as usize;
    let flags = reader.read_u8()?;
    if flags & !ACCOUNT_FLAGS != 0 {
        return Err(AccountStoreFormatError::InvalidFlags);
    }
    if reader.read_u8()? != 0 || reader.read_u16()? != 0 {
        return Err(AccountStoreFormatError::NonZeroReserved);
    }

    let account_id = reader.read_array::<SHA256_DIGEST_LENGTH>()?;
    if account_id.iter().all(|byte| *byte == 0) {
        return Err(AccountStoreFormatError::ZeroAccountId);
    }
    if !seen_account_ids.insert(account_id) {
        return Err(AccountStoreFormatError::DuplicateAccountId);
    }

    let mut verifier = Box::new([0; SHA256_DIGEST_LENGTH]);
    verifier.copy_from_slice(reader.read_exact(SHA256_DIGEST_LENGTH)?);

    let username_bytes = reader.read_exact(username_length)?;
    let username = String::from_utf8(username_bytes.to_vec())
        .map_err(|_| AccountStoreFormatError::InvalidUtf8)?;
    validate_username(&username)?;

    let mut database_grants = Vec::with_capacity(grant_count);
    let mut previous_name: Option<String> = None;
    for _ in 0..grant_count {
        let name_length = reader.read_u16()? as usize;
        let bits = reader.read_u8()?;
        if bits == 0 || bits & !DATABASE_GRANT_BITS != 0 {
            return Err(AccountStoreFormatError::InvalidDatabaseGrant);
        }
        if reader.read_u8()? != 0 {
            return Err(AccountStoreFormatError::NonZeroReserved);
        }
        let name = String::from_utf8(reader.read_exact(name_length)?.to_vec())
            .map_err(|_| AccountStoreFormatError::InvalidUtf8)?;
        validate_database_name(&name)?;
        if let Some(previous) = previous_name.as_deref() {
            match previous.as_bytes().cmp(name.as_bytes()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => {
                    return Err(AccountStoreFormatError::UnsortedDatabaseGrant);
                }
            }
        }
        previous_name = Some(name.clone());
        database_grants.push(StoredDatabaseGrant {
            database_name: name,
            bits,
        });
    }
    if reader.remaining() != 0 {
        return Err(AccountStoreFormatError::InvalidLength);
    }

    Ok(StoredAccountRecord {
        username,
        account_id,
        enabled: flags & ENABLED_FLAG != 0,
        verifier,
        global_connect: flags & GLOBAL_CONNECT_FLAG != 0,
        global_list: flags & GLOBAL_LIST_FLAG != 0,
        database_grants,
    })
}

fn validate_database_name(name: &str) -> Result<(), AccountStoreFormatError> {
    let canonical = turso_mysql::canonicalize_database_name(name)
        .map_err(|_| AccountStoreFormatError::InvalidDatabaseName)?;

    if canonical != name {
        return Err(AccountStoreFormatError::InvalidDatabaseName);
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], AccountStoreFormatError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(AccountStoreFormatError::InvalidLength)?;
        if end > self.bytes.len() {
            return Err(AccountStoreFormatError::Truncated);
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, AccountStoreFormatError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, AccountStoreFormatError> {
        Ok(u16::from_be_bytes(
            self.read_exact(2)?.try_into().expect("length checked"),
        ))
    }

    fn read_u32(&mut self) -> Result<u32, AccountStoreFormatError> {
        Ok(u32::from_be_bytes(
            self.read_exact(4)?.try_into().expect("length checked"),
        ))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], AccountStoreFormatError> {
        Ok(self.read_exact(N)?.try_into().expect("length checked"))
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, AccountStoreFormatError> {
    Ok(u16::from_be_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(AccountStoreFormatError::Truncated)?
            .try_into()
            .expect("length checked"),
    ))
}

fn read_array<const N: usize>(
    bytes: &[u8],
    offset: usize,
) -> Result<[u8; N], AccountStoreFormatError> {
    Ok(bytes
        .get(offset..offset + N)
        .ok_or(AccountStoreFormatError::Truncated)?
        .try_into()
        .expect("length checked"))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, AccountStoreFormatError> {
    Ok(u32::from_be_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(AccountStoreFormatError::Truncated)?
            .try_into()
            .expect("length checked"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, AccountStoreFormatError> {
    Ok(u64::from_be_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(AccountStoreFormatError::Truncated)?
            .try_into()
            .expect("length checked"),
    ))
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(username: &str, id: u8, database_grants: Vec<(&str, u8)>) -> StoredAccountRecord {
        StoredAccountRecord {
            username: username.to_owned(),
            account_id: [id; SHA256_DIGEST_LENGTH],
            enabled: true,
            verifier: Box::new([id.wrapping_add(0x80); SHA256_DIGEST_LENGTH]),
            global_connect: id % 2 == 0,
            global_list: id % 3 == 0,
            database_grants: database_grants
                .into_iter()
                .map(|(database_name, bits)| StoredDatabaseGrant {
                    database_name: database_name.to_owned(),
                    bits,
                })
                .collect(),
        }
    }

    fn snapshot() -> StoredAuthSnapshot {
        StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 42,
            retired_account_ids: vec![[0x33; SHA256_DIGEST_LENGTH]],
            accounts: vec![
                account("bob", 2, vec![("zeta", 8), ("alpha", 1)]),
                account("alice", 1, vec![("reports", 2), ("app", 1)]),
            ],
        }
    }

    fn with_recomputed_checksum(mut bytes: Vec<u8>) -> Vec<u8> {
        let checksum_offset = bytes.len() - CHECKSUM_LENGTH;
        let checksum = crc32(&bytes[..checksum_offset]);
        bytes[checksum_offset..].copy_from_slice(&checksum.to_be_bytes());
        bytes
    }

    fn encode_bytes(snapshot: &StoredAuthSnapshot) -> Vec<u8> {
        snapshot.encode().unwrap().to_vec()
    }

    #[test]
    fn round_trip_sorts_accounts_and_database_grants() {
        let encoded = encode_bytes(&snapshot());
        let decoded = StoredAuthSnapshot::decode(&encoded).unwrap();
        assert_eq!(
            decoded
                .accounts
                .iter()
                .map(|account| account.username.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert_eq!(
            decoded.accounts[0]
                .database_grants
                .iter()
                .map(|grant| grant.database_name.as_str())
                .collect::<Vec<_>>(),
            ["app", "reports"]
        );
        assert_eq!(
            decoded.accounts[1]
                .database_grants
                .iter()
                .map(|grant| grant.database_name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
        assert_eq!(
            decoded.retired_account_ids,
            vec![[0x33; SHA256_DIGEST_LENGTH]]
        );
        assert_eq!(
            StoredAuthSnapshot::decode(&encoded)
                .unwrap()
                .encode()
                .unwrap()
                .as_slice(),
            encoded.as_slice()
        );
    }

    #[test]
    fn encode_sorts_retired_ids_but_decode_requires_sorted_ids() {
        let mut value = snapshot();
        value.retired_account_ids =
            vec![[0x44; SHA256_DIGEST_LENGTH], [0x33; SHA256_DIGEST_LENGTH]];
        let encoded = encode_bytes(&value);
        let retired_start = HEADER_LENGTH;
        assert_eq!(
            &encoded[retired_start..retired_start + SHA256_DIGEST_LENGTH],
            &[0x33; SHA256_DIGEST_LENGTH]
        );
        assert_eq!(
            &encoded
                [retired_start + SHA256_DIGEST_LENGTH..retired_start + 2 * SHA256_DIGEST_LENGTH],
            &[0x44; SHA256_DIGEST_LENGTH]
        );

        let mut unsorted = encoded;
        let first = retired_start;
        let second = retired_start + SHA256_DIGEST_LENGTH;
        let first_id: [u8; SHA256_DIGEST_LENGTH] = unsorted[first..first + SHA256_DIGEST_LENGTH]
            .try_into()
            .unwrap();
        let second_id: [u8; SHA256_DIGEST_LENGTH] = unsorted[second..second + SHA256_DIGEST_LENGTH]
            .try_into()
            .unwrap();
        unsorted[first..first + SHA256_DIGEST_LENGTH].copy_from_slice(&second_id);
        unsorted[second..second + SHA256_DIGEST_LENGTH].copy_from_slice(&first_id);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(unsorted)),
            Err(AccountStoreFormatError::UnsortedRetiredAccountId)
        );
    }

    #[test]
    fn store_and_retired_ids_are_validated_and_active_ids_cannot_be_retired() {
        let mut zero_store = snapshot();
        zero_store.store_id = [0; SHA256_DIGEST_LENGTH];
        assert_eq!(
            zero_store.encode(),
            Err(AccountStoreFormatError::ZeroStoreId)
        );

        let mut zero_retired = snapshot();
        zero_retired.retired_account_ids = vec![[0; SHA256_DIGEST_LENGTH]];
        assert_eq!(
            zero_retired.encode(),
            Err(AccountStoreFormatError::ZeroRetiredAccountId)
        );

        let mut duplicate_retired = snapshot();
        duplicate_retired.retired_account_ids =
            vec![[0x33; SHA256_DIGEST_LENGTH], [0x33; SHA256_DIGEST_LENGTH]];
        assert_eq!(
            duplicate_retired.encode(),
            Err(AccountStoreFormatError::DuplicateRetiredAccountId)
        );

        let mut overlap = snapshot();
        overlap.retired_account_ids = vec![[1; SHA256_DIGEST_LENGTH]];
        assert_eq!(
            overlap.encode(),
            Err(AccountStoreFormatError::ActiveRetiredAccountIdOverlap)
        );
    }

    #[test]
    fn wire_header_and_checksum_use_big_endian_fields() {
        let encoded = encode_bytes(&snapshot());
        assert_eq!(&encoded[..8], b"TURSAUTH");
        assert_eq!(&encoded[8..10], &VERSION.to_be_bytes());
        assert_eq!(&encoded[10..12], &(HEADER_LENGTH as u16).to_be_bytes());
        assert_eq!(&encoded[12..44], &[0x55; SHA256_DIGEST_LENGTH]);
        assert_eq!(&encoded[44..52], &42u64.to_be_bytes());
        assert_eq!(&encoded[52..56], &2u32.to_be_bytes());
        assert_eq!(&encoded[56..60], &1u32.to_be_bytes());
        assert_eq!(
            &encoded[60..64],
            &((encoded.len() - HEADER_LENGTH - CHECKSUM_LENGTH) as u32).to_be_bytes()
        );
        assert_eq!(&encoded[64..68], &0u32.to_be_bytes());
        let checksum_offset = encoded.len() - CHECKSUM_LENGTH;
        assert_eq!(
            &encoded[checksum_offset..],
            &crc32(&encoded[..checksum_offset]).to_be_bytes()
        );
    }

    #[test]
    fn crc32_matches_the_standard_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn debug_redacts_verifier_and_account_id() {
        let snapshot = snapshot();
        let debug = format!("{snapshot:?}");
        assert!(debug.contains("verifier: \"<redacted>\""));
        assert!(debug.contains("account_id: \"<redacted>\""));
        assert!(!debug.contains("81818181"));
    }

    #[test]
    fn malformed_headers_are_rejected() {
        let encoded = encode_bytes(&snapshot());

        let mut bad_magic = encoded.clone();
        bad_magic[0] ^= 1;
        assert_eq!(
            StoredAuthSnapshot::decode(&bad_magic),
            Err(AccountStoreFormatError::InvalidMagic)
        );

        let mut bad_version = encoded.clone();
        bad_version[8..10].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            StoredAuthSnapshot::decode(&bad_version),
            Err(AccountStoreFormatError::UnsupportedVersion)
        );

        let mut bad_store_id = encoded.clone();
        bad_store_id[12..12 + SHA256_DIGEST_LENGTH].fill(0);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(bad_store_id)),
            Err(AccountStoreFormatError::ZeroStoreId)
        );

        let mut too_many_retired = vec![0; HEADER_LENGTH + CHECKSUM_LENGTH];
        too_many_retired[..8].copy_from_slice(&MAGIC);
        too_many_retired[8..10].copy_from_slice(&VERSION.to_be_bytes());
        too_many_retired[10..12].copy_from_slice(&(HEADER_LENGTH as u16).to_be_bytes());
        too_many_retired[12] = 1;
        too_many_retired[56..60].copy_from_slice(&((MAX_RETIRED + 1) as u32).to_be_bytes());
        let too_many_retired = with_recomputed_checksum(too_many_retired);
        assert_eq!(
            StoredAuthSnapshot::decode(&too_many_retired),
            Err(AccountStoreFormatError::TooManyRetiredAccountIds)
        );

        let mut bad_reserved = encoded.clone();
        bad_reserved[64..68].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(
            StoredAuthSnapshot::decode(&bad_reserved),
            Err(AccountStoreFormatError::NonZeroReserved)
        );

        let mut bad_checksum = encoded.clone();
        let index = bad_checksum.len() - 1;
        bad_checksum[index] ^= 1;
        assert_eq!(
            StoredAuthSnapshot::decode(&bad_checksum),
            Err(AccountStoreFormatError::ChecksumMismatch)
        );

        assert_eq!(
            StoredAuthSnapshot::decode(&encoded[..encoded.len() - 1]),
            Err(AccountStoreFormatError::Truncated)
        );
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            StoredAuthSnapshot::decode(&trailing),
            Err(AccountStoreFormatError::TrailingBytes)
        );
    }

    #[test]
    fn malformed_retired_ids_are_rejected() {
        let encoded = encode_bytes(&snapshot());
        let retired_start = HEADER_LENGTH;

        let mut zero = encoded.clone();
        zero[retired_start..retired_start + SHA256_DIGEST_LENGTH].fill(0);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(zero)),
            Err(AccountStoreFormatError::ZeroRetiredAccountId)
        );

        let mut overlap = encoded;
        let first_account = retired_start + SHA256_DIGEST_LENGTH + 4;
        let first_id = first_account + 8;
        let active_id: [u8; SHA256_DIGEST_LENGTH] = overlap
            [first_id..first_id + SHA256_DIGEST_LENGTH]
            .try_into()
            .unwrap();
        overlap[retired_start..retired_start + SHA256_DIGEST_LENGTH].copy_from_slice(&active_id);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(overlap)),
            Err(AccountStoreFormatError::ActiveRetiredAccountIdOverlap)
        );

        let mut duplicate = snapshot();
        duplicate.retired_account_ids =
            vec![[0x33; SHA256_DIGEST_LENGTH], [0x44; SHA256_DIGEST_LENGTH]];
        let mut duplicate = encode_bytes(&duplicate);
        let second = retired_start + SHA256_DIGEST_LENGTH;
        duplicate[second..second + SHA256_DIGEST_LENGTH]
            .copy_from_slice(&[0x33; SHA256_DIGEST_LENGTH]);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(duplicate)),
            Err(AccountStoreFormatError::DuplicateRetiredAccountId)
        );

        let mut unsorted = snapshot();
        unsorted.retired_account_ids =
            vec![[0x33; SHA256_DIGEST_LENGTH], [0x44; SHA256_DIGEST_LENGTH]];
        let mut unsorted = encode_bytes(&unsorted);
        let first_id: [u8; SHA256_DIGEST_LENGTH] = unsorted
            [retired_start..retired_start + SHA256_DIGEST_LENGTH]
            .try_into()
            .unwrap();
        let second_id: [u8; SHA256_DIGEST_LENGTH] = unsorted[second..second + SHA256_DIGEST_LENGTH]
            .try_into()
            .unwrap();
        unsorted[retired_start..retired_start + SHA256_DIGEST_LENGTH].copy_from_slice(&second_id);
        unsorted[second..second + SHA256_DIGEST_LENGTH].copy_from_slice(&first_id);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(unsorted)),
            Err(AccountStoreFormatError::UnsortedRetiredAccountId)
        );
    }

    #[test]
    fn malformed_records_are_rejected() {
        let encoded = encode_bytes(&snapshot());
        let body_start = HEADER_LENGTH;
        let first_record = body_start + SHA256_DIGEST_LENGTH + 4;
        let first_username_start = first_record + ACCOUNT_FIXED_LENGTH;
        let first_record_length =
            read_u32(&encoded, body_start + SHA256_DIGEST_LENGTH).unwrap() as usize;
        let second_record = first_record + first_record_length + 4;

        let mut invalid_flags = encoded.clone();
        invalid_flags[first_record + 4] = 0x80;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(invalid_flags)),
            Err(AccountStoreFormatError::InvalidFlags)
        );

        let mut zero_id = encoded.clone();
        let id_start = first_record + 8;
        zero_id[id_start..id_start + SHA256_DIGEST_LENGTH].fill(0);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(zero_id)),
            Err(AccountStoreFormatError::ZeroAccountId)
        );

        let mut nul_username = encoded.clone();
        nul_username[first_username_start] = 0;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(nul_username)),
            Err(AccountStoreFormatError::InvalidUsername)
        );

        let mut invalid_utf8 = encoded.clone();
        invalid_utf8[first_username_start] = 0xff;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(invalid_utf8)),
            Err(AccountStoreFormatError::InvalidUtf8)
        );

        let mut bad_account_reserved = encoded.clone();
        bad_account_reserved[first_record + 5] = 1;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(bad_account_reserved)),
            Err(AccountStoreFormatError::NonZeroReserved)
        );

        let first_grant = first_username_start + 5;
        let mut unknown_grant_bits = encoded.clone();
        unknown_grant_bits[first_grant + 2] = 0x80;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(unknown_grant_bits)),
            Err(AccountStoreFormatError::InvalidDatabaseGrant)
        );

        let mut nonzero_grant_reserved = encoded.clone();
        nonzero_grant_reserved[first_grant + 3] = 1;
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(nonzero_grant_reserved)),
            Err(AccountStoreFormatError::NonZeroReserved)
        );

        let mut noncanonical_database = encoded.clone();
        noncanonical_database[first_grant + 4] = b'A';
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(noncanonical_database)),
            Err(AccountStoreFormatError::InvalidDatabaseName)
        );

        let mut unsorted_accounts = encoded.clone();
        unsorted_accounts[first_username_start..first_username_start + 5].copy_from_slice(b"zzzzz");
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(unsorted_accounts)),
            Err(AccountStoreFormatError::UnsortedUsername)
        );

        let mut duplicate_account_id = encoded.clone();
        let first_id = first_record + 8;
        let second_id = second_record + 8;
        let first_id_bytes: [u8; SHA256_DIGEST_LENGTH] = duplicate_account_id
            [first_id..first_id + SHA256_DIGEST_LENGTH]
            .try_into()
            .expect("fixed account ID length");
        duplicate_account_id[second_id..second_id + SHA256_DIGEST_LENGTH]
            .copy_from_slice(&first_id_bytes);
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(duplicate_account_id)),
            Err(AccountStoreFormatError::DuplicateAccountId)
        );

        let mut bad_record_length = encoded;
        bad_record_length[body_start + SHA256_DIGEST_LENGTH..body_start + SHA256_DIGEST_LENGTH + 4]
            .copy_from_slice(&((first_record_length + 1) as u32).to_be_bytes());
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(bad_record_length)),
            Err(AccountStoreFormatError::InvalidLength)
        );
    }

    #[test]
    fn duplicate_usernames_and_database_grants_are_rejected() {
        let duplicate_users = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![account("aaa", 1, Vec::new()), account("bbb", 2, Vec::new())],
        };
        let mut duplicate_users_bytes = encode_bytes(&duplicate_users);
        let records_start =
            HEADER_LENGTH + duplicate_users.retired_account_ids.len() * SHA256_DIGEST_LENGTH;
        let first_length = read_u32(&duplicate_users_bytes, records_start).unwrap() as usize;
        let second_record = records_start + 4 + first_length + 4;
        duplicate_users_bytes
            [second_record + ACCOUNT_FIXED_LENGTH..second_record + ACCOUNT_FIXED_LENGTH + 3]
            .copy_from_slice(b"aaa");
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(duplicate_users_bytes)),
            Err(AccountStoreFormatError::DuplicateUsername)
        );

        let grants = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![account("alice", 1, vec![("aaa", 1), ("bbb", 1)])],
        };
        let mut duplicate_grants_bytes = encode_bytes(&grants);
        let record = HEADER_LENGTH + grants.retired_account_ids.len() * SHA256_DIGEST_LENGTH + 4;
        let username = record + ACCOUNT_FIXED_LENGTH;
        let first_grant = username + 5;
        let second_grant = first_grant + DATABASE_GRANT_FIXED_LENGTH + 3;
        duplicate_grants_bytes[second_grant + 4..second_grant + 7].copy_from_slice(b"aaa");
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(duplicate_grants_bytes)),
            Err(AccountStoreFormatError::UnsortedDatabaseGrant)
        );

        let mut unsorted_grants_bytes = encode_bytes(&grants);
        unsorted_grants_bytes[first_grant + 4..first_grant + 7].copy_from_slice(b"zzz");
        assert_eq!(
            StoredAuthSnapshot::decode(&with_recomputed_checksum(unsorted_grants_bytes)),
            Err(AccountStoreFormatError::UnsortedDatabaseGrant)
        );
    }

    #[test]
    fn invalid_input_is_rejected_before_persistence() {
        let invalid_name = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![account("alice", 1, vec![("Reports", 1)])],
        };
        assert_eq!(
            invalid_name.encode(),
            Err(AccountStoreFormatError::InvalidDatabaseName)
        );

        let invalid_grant = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![account("alice", 1, vec![("reports", 0)])],
        };
        assert_eq!(
            invalid_grant.encode(),
            Err(AccountStoreFormatError::InvalidDatabaseGrant)
        );

        let duplicate_username = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![
                account("alice", 1, Vec::new()),
                account("alice", 2, Vec::new()),
            ],
        };
        assert_eq!(
            duplicate_username.encode(),
            Err(AccountStoreFormatError::DuplicateUsername)
        );

        let duplicate_id = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![],
            accounts: vec![
                account("alice", 1, Vec::new()),
                account("bob", 1, Vec::new()),
            ],
        };
        assert_eq!(
            duplicate_id.encode(),
            Err(AccountStoreFormatError::DuplicateAccountId)
        );

        let mut too_many = Vec::new();
        for index in 1..=MAX_ACCOUNTS + 1 {
            too_many.push(account(&format!("user{index}"), index as u8, Vec::new()));
        }
        assert_eq!(
            (StoredAuthSnapshot {
                store_id: [0x55; SHA256_DIGEST_LENGTH],
                revision: 1,
                retired_account_ids: vec![],
                accounts: too_many,
            })
            .encode(),
            Err(AccountStoreFormatError::TooManyAccounts)
        );

        let too_many_retired = StoredAuthSnapshot {
            store_id: [0x55; SHA256_DIGEST_LENGTH],
            revision: 1,
            retired_account_ids: vec![[0x33; SHA256_DIGEST_LENGTH]; MAX_RETIRED + 1],
            accounts: Vec::new(),
        };
        assert_eq!(
            too_many_retired.encode(),
            Err(AccountStoreFormatError::TooManyRetiredAccountIds)
        );
    }

    #[test]
    fn oversized_input_is_rejected_without_parsing() {
        assert_eq!(
            StoredAuthSnapshot::decode(&vec![0; MAX_SNAPSHOT_BYTES + 1]),
            Err(AccountStoreFormatError::TooLarge)
        );
    }
}
