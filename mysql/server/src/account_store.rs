//! One immutable account and privilege generation for classic MySQL sessions.
//!
//! Each credential lookup and authorization decision observes one complete
//! generation. Authorization reads the latest generation, so a replacement
//! affects the next authorization decision. Replacing a generation never lets
//! a recreated username inherit the permissions of the account that used the
//! name before it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, RwLock},
};

use crate::account_store_format::{
    StoredAccountRecord, StoredAuthSnapshot, StoredDatabaseGrant, DATABASE_GRANT_CONNECT_BIT,
    DATABASE_GRANT_CREATE_BIT, DATABASE_GRANT_DROP_BIT, DATABASE_GRANT_QUERY_BIT,
};
use crate::{
    validate_username, AccountId, AuthenticatedPrincipal, AuthorizationError, CredentialProvider,
    CredentialProviderConfigError, CredentialProviderError, CredentialSnapshot, DatabaseAction,
    DatabaseAuthorizer, StoredCredential, SHA256_DIGEST_LENGTH,
};
use turso_mysql::canonicalize_database_name;
use zeroize::Zeroize;

/// The largest complete account generation this in-memory boundary accepts.
pub const MAX_ACCOUNT_DEFINITIONS: usize = 8_192;
/// The largest number of database-specific grants in one account generation.
pub const MAX_DATABASE_GRANTS: usize = 65_536;
/// The largest number of retired account identities retained for reuse checks.
pub const MAX_RETIRED_ACCOUNT_IDS: usize = 65_536;

/// One account supplied by a protected account backend.
pub struct AccountDefinition {
    username: String,
    account_id: AccountId,
    enabled: bool,
    full_verifier: Box<[u8; SHA256_DIGEST_LENGTH]>,
    global_privileges: GlobalPrivileges,
}

impl AccountDefinition {
    /// Creates one account from an exact username and a full verifier.
    pub fn new(
        username: impl Into<String>,
        account_id: AccountId,
        enabled: bool,
        full_verifier: [u8; SHA256_DIGEST_LENGTH],
    ) -> Self {
        Self {
            username: username.into(),
            account_id,
            enabled,
            full_verifier: Box::new(full_verifier),
            global_privileges: GlobalPrivileges::default(),
        }
    }

    /// Adds the account-wide permissions used before a database is selected.
    pub const fn with_global_privileges(mut self, privileges: GlobalPrivileges) -> Self {
        self.global_privileges = privileges;
        self
    }
}

impl fmt::Debug for AccountDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountDefinition")
            .field("username", &self.username)
            .field("account_id", &self.account_id)
            .field("enabled", &self.enabled)
            .field("full_verifier", &"<redacted>")
            .field("global_privileges", &self.global_privileges)
            .finish()
    }
}

impl Drop for AccountDefinition {
    fn drop(&mut self) {
        self.full_verifier.zeroize();
    }
}

/// Account-wide permissions that do not name a database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalPrivileges {
    connect: bool,
    list: bool,
}

impl GlobalPrivileges {
    /// Creates account-wide permissions.
    pub const fn new(connect: bool, list: bool) -> Self {
        Self { connect, list }
    }
}

/// One database-specific permission record supplied by a protected backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseGrant {
    account_id: AccountId,
    database: String,
    privileges: DatabasePrivileges,
}

impl DatabaseGrant {
    /// Creates one grant. The generation builder validates the database name.
    pub fn new(
        account_id: AccountId,
        database: impl Into<String>,
        privileges: DatabasePrivileges,
    ) -> Self {
        Self {
            account_id,
            database: database.into(),
            privileges,
        }
    }
}

/// Permissions for one canonical logical database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabasePrivileges {
    connect: bool,
    query: bool,
    create: bool,
    drop: bool,
}

impl DatabasePrivileges {
    /// Creates database-specific permissions.
    pub const fn new(connect: bool, query: bool, create: bool, drop: bool) -> Self {
        Self {
            connect,
            query,
            create,
            drop,
        }
    }

    const fn is_empty(self) -> bool {
        !self.connect && !self.query && !self.create && !self.drop
    }
}

/// A complete replacement generation for accounts and their database grants.
#[derive(Default)]
pub struct AccountGenerationBuilder {
    accounts: Vec<AccountDefinition>,
    grants: Vec<DatabaseGrant>,
}

impl AccountGenerationBuilder {
    /// Creates an empty generation that denies every credential lookup.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one account definition.
    pub fn add_account(&mut self, account: AccountDefinition) -> &mut Self {
        self.accounts.push(account);
        self
    }

    /// Adds one database-specific grant.
    pub fn add_grant(&mut self, grant: DatabaseGrant) -> &mut Self {
        self.grants.push(grant);
        self
    }

    /// Adds one account definition while building fluently.
    pub fn with_account(mut self, account: AccountDefinition) -> Self {
        self.add_account(account);
        self
    }

    /// Adds one database grant while building fluently.
    pub fn with_grant(mut self, grant: DatabaseGrant) -> Self {
        self.add_grant(grant);
        self
    }

    fn build(self, revision: u64) -> Result<AccountGeneration, AccountStoreConfigError> {
        self.build_with_history(revision, HashSet::new(), HashSet::new())
    }

    fn build_with_history(
        self,
        revision: u64,
        mut retired_account_ids: HashSet<AccountId>,
        previous_account_ids: HashSet<AccountId>,
    ) -> Result<AccountGeneration, AccountStoreConfigError> {
        if self.accounts.len() > MAX_ACCOUNT_DEFINITIONS {
            return Err(AccountStoreConfigError::TooManyAccounts {
                actual: self.accounts.len(),
                limit: MAX_ACCOUNT_DEFINITIONS,
            });
        }
        if self.grants.len() > MAX_DATABASE_GRANTS {
            return Err(AccountStoreConfigError::TooManyDatabaseGrants {
                actual: self.grants.len(),
                limit: MAX_DATABASE_GRANTS,
            });
        }
        let mut accounts_by_username = BTreeMap::new();
        let mut account_ids = HashSet::new();
        let mut authorizations = HashMap::new();
        let mut grant_counts = HashMap::new();

        for account in self.accounts {
            validate_username(&account.username)
                .map_err(AccountStoreConfigError::InvalidUsername)?;
            if account.account_id.is_zero() {
                return Err(AccountStoreConfigError::ZeroAccountId {
                    username: account.username.clone(),
                });
            }
            if retired_account_ids.contains(&account.account_id) {
                return Err(AccountStoreConfigError::RetiredAccountId);
            }
            if accounts_by_username.contains_key(&account.username) {
                return Err(AccountStoreConfigError::DuplicateUsername {
                    username: account.username.clone(),
                });
            }
            if !account_ids.insert(account.account_id.clone()) {
                return Err(AccountStoreConfigError::DuplicateAccountId);
            }

            authorizations.insert(
                account.account_id.clone(),
                AccountAuthorization {
                    enabled: account.enabled,
                    global_privileges: account.global_privileges,
                    database_privileges: BTreeMap::new(),
                },
            );
            accounts_by_username.insert(
                account.username.clone(),
                StoredAccount {
                    account_id: account.account_id.clone(),
                    credential: Box::new(StoredCredential::from_full_verifier(
                        account.enabled,
                        *account.full_verifier,
                    )),
                },
            );
        }

        let mut granted_databases = HashSet::new();
        for grant in self.grants {
            validate_canonical_database_name(&grant.database)?;
            if grant.privileges.is_empty() {
                return Err(AccountStoreConfigError::EmptyDatabasePrivileges {
                    database: grant.database,
                });
            }
            if !granted_databases.insert((grant.account_id.clone(), grant.database.clone())) {
                return Err(AccountStoreConfigError::DuplicateDatabaseGrant {
                    database: grant.database,
                });
            }
            let authorization = authorizations
                .get_mut(&grant.account_id)
                .ok_or(AccountStoreConfigError::UnknownGrantOwner)?;
            let grant_count = grant_counts
                .entry(grant.account_id.clone())
                .or_insert(0usize);
            *grant_count += 1;
            if *grant_count > u16::MAX as usize {
                return Err(AccountStoreConfigError::TooManyDatabaseGrants {
                    actual: *grant_count,
                    limit: u16::MAX as usize,
                });
            }
            authorization
                .database_privileges
                .insert(grant.database, grant.privileges);
        }

        retired_account_ids.extend(
            previous_account_ids
                .into_iter()
                .filter(|account_id| !account_ids.contains(account_id)),
        );
        if retired_account_ids.len() > MAX_RETIRED_ACCOUNT_IDS {
            return Err(AccountStoreConfigError::TooManyRetiredAccountIds);
        }

        Ok(AccountGeneration {
            revision,
            accounts_by_username,
            authorizations,
            retired_account_ids,
        })
    }
}

impl fmt::Debug for AccountGenerationBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountGenerationBuilder")
            .field("account_count", &self.accounts.len())
            .field("grant_count", &self.grants.len())
            .finish()
    }
}

/// A rejected account or privilege generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStoreConfigError {
    /// The requested account count exceeds the bounded in-memory generation.
    TooManyAccounts {
        /// Number of supplied account records.
        actual: usize,
        /// Largest accepted account count.
        limit: usize,
    },
    /// The requested grant count exceeds the bounded in-memory generation.
    TooManyDatabaseGrants {
        /// Number of supplied database grants.
        actual: usize,
        /// Largest accepted grant count.
        limit: usize,
    },
    /// A username cannot occur in a classic handshake.
    InvalidUsername(CredentialProviderConfigError),
    /// The all-zero account ID is reserved as an invalid persistent identity.
    ZeroAccountId { username: String },
    /// Two records use the exact same handshake username.
    DuplicateUsername { username: String },
    /// Two account records use the same canonical account ID.
    DuplicateAccountId,
    /// A grant names an invalid or noncanonical database.
    InvalidDatabaseName { database: String },
    /// A grant has no permission bits set.
    EmptyDatabasePrivileges { database: String },
    /// A stored grant contains bits this version cannot authorize.
    InvalidDatabasePrivileges { database: String },
    /// More than one grant names the same account and database.
    DuplicateDatabaseGrant { database: String },
    /// A grant does not belong to any account in this generation.
    UnknownGrantOwner,
    /// A deleted account identity cannot be assigned to a later account.
    RetiredAccountId,
    /// The retained deleted-account identity set reached its fixed limit.
    TooManyRetiredAccountIds,
}

impl fmt::Display for AccountStoreConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyAccounts { actual, limit } => {
                write!(
                    f,
                    "account generation has {actual} accounts, above limit {limit}"
                )
            }
            Self::TooManyDatabaseGrants { actual, limit } => {
                write!(
                    f,
                    "account generation has {actual} database grants, above limit {limit}"
                )
            }
            Self::InvalidUsername(error) => write!(f, "invalid account username: {error}"),
            Self::ZeroAccountId { username } => {
                write!(f, "account {username:?} has an all-zero account ID")
            }
            Self::DuplicateUsername { username } => {
                write!(f, "duplicate account username {username:?}")
            }
            Self::DuplicateAccountId => f.write_str("duplicate account ID"),
            Self::InvalidDatabaseName { database } => {
                write!(
                    f,
                    "database grant uses invalid or noncanonical name {database:?}"
                )
            }
            Self::EmptyDatabasePrivileges { database } => {
                write!(f, "database grant for {database:?} has no permissions")
            }
            Self::InvalidDatabasePrivileges { database } => {
                write!(f, "database grant for {database:?} has invalid permissions")
            }
            Self::DuplicateDatabaseGrant { database } => {
                write!(f, "duplicate database grant for {database:?}")
            }
            Self::UnknownGrantOwner => f.write_str("database grant belongs to an unknown account"),
            Self::RetiredAccountId => f.write_str("account generation reuses a retired account ID"),
            Self::TooManyRetiredAccountIds => {
                f.write_str("account generation has too many retired account IDs")
            }
        }
    }
}

impl Error for AccountStoreConfigError {}

/// An attempted replacement did not install a new generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStoreReplaceError {
    /// The caller based an update on a generation that is no longer current.
    Conflict {
        /// Revision observed by the caller.
        expected: u64,
        /// Revision installed when the replacement was attempted.
        actual: u64,
    },
    /// The proposed complete generation is invalid.
    InvalidGeneration(AccountStoreConfigError),
    /// The in-process store could not read or replace its current generation.
    Unavailable,
}

impl fmt::Display for AccountStoreReplaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { expected, actual } => {
                write!(
                    f,
                    "account generation conflict: expected revision {expected}, found {actual}"
                )
            }
            Self::InvalidGeneration(error) => write!(f, "invalid account generation: {error}"),
            Self::Unavailable => f.write_str("account store unavailable"),
        }
    }
}

impl Error for AccountStoreReplaceError {}

/// A cloneable account backend that exposes one immutable credential and grant generation.
#[derive(Clone)]
pub struct AccountStore {
    current: Arc<RwLock<Arc<AccountGeneration>>>,
}

impl AccountStore {
    /// Creates a store at revision zero after validating every account and grant.
    pub fn new(builder: AccountGenerationBuilder) -> Result<Self, AccountStoreConfigError> {
        Ok(Self {
            current: Arc::new(RwLock::new(Arc::new(builder.build(0)?))),
        })
    }

    /// Returns the current generation revision.
    pub fn revision(&self) -> Result<u64, AccountStoreReplaceError> {
        Ok(self
            .current
            .read()
            .map_err(|_| AccountStoreReplaceError::Unavailable)?
            .revision)
    }

    /// Validates and atomically replaces every account and grant at one revision.
    pub fn replace(
        &self,
        expected_revision: u64,
        builder: AccountGenerationBuilder,
    ) -> Result<u64, AccountStoreReplaceError> {
        let replacement = self.build_replacement(expected_revision, builder)?;
        let revision = replacement.revision;
        let mut current = self
            .current
            .write()
            .map_err(|_| AccountStoreReplaceError::Unavailable)?;
        let actual = current.revision;
        if actual != expected_revision {
            return Err(AccountStoreReplaceError::Conflict {
                expected: expected_revision,
                actual,
            });
        }
        *current = Arc::new(replacement);
        Ok(revision)
    }

    pub(crate) fn build_replacement(
        &self,
        expected_revision: u64,
        builder: AccountGenerationBuilder,
    ) -> Result<AccountGeneration, AccountStoreReplaceError> {
        let history = self
            .current
            .read()
            .map(|current| Arc::clone(&current))
            .map_err(|_| AccountStoreReplaceError::Unavailable)?;
        if history.revision != expected_revision {
            return Err(AccountStoreReplaceError::Conflict {
                expected: expected_revision,
                actual: history.revision,
            });
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or(AccountStoreReplaceError::Unavailable)?;
        let replacement = builder
            .build_with_history(
                revision,
                history.retired_account_ids.clone(),
                history.authorizations.keys().cloned().collect(),
            )
            .map_err(AccountStoreReplaceError::InvalidGeneration)?;
        Ok(replacement)
    }

    pub(crate) fn from_generation(generation: AccountGeneration) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(generation))),
        }
    }

    fn current_generation(&self) -> Result<Arc<AccountGeneration>, ()> {
        self.current
            .read()
            .map(|generation| Arc::clone(&generation))
            .map_err(|_| ())
    }
}

impl fmt::Debug for AccountStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let revision = self.revision().ok();
        f.debug_struct("AccountStore")
            .field("revision", &revision)
            .finish()
    }
}

impl CredentialProvider for AccountStore {
    fn lookup(
        &self,
        username: &str,
    ) -> Result<Option<CredentialSnapshot>, CredentialProviderError> {
        let generation = self
            .current_generation()
            .map_err(|_| CredentialProviderError::BackendUnavailable)?;
        Ok(generation
            .accounts_by_username
            .get(username)
            .map(|account| {
                CredentialSnapshot::new(account.account_id.clone(), account.credential.duplicate())
            }))
    }
}

impl DatabaseAuthorizer for AccountStore {
    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        let generation = self
            .current_generation()
            .map_err(|_| AuthorizationError::Unavailable)?;
        generation.authorize(principal.account_id(), action)
    }
}

pub(crate) struct AccountGeneration {
    revision: u64,
    accounts_by_username: BTreeMap<String, StoredAccount>,
    authorizations: HashMap<AccountId, AccountAuthorization>,
    retired_account_ids: HashSet<AccountId>,
}

impl AccountGeneration {
    pub(crate) fn from_builder(
        builder: AccountGenerationBuilder,
        revision: u64,
    ) -> Result<Self, AccountStoreConfigError> {
        builder.build(revision)
    }

    pub(crate) fn from_snapshot(
        snapshot: StoredAuthSnapshot,
    ) -> Result<Self, AccountStoreConfigError> {
        let revision = snapshot.revision;
        let mut retired_account_ids = HashSet::new();
        for account_id in &snapshot.retired_account_ids {
            let account_id = AccountId::from_bytes(*account_id);
            if account_id.is_zero() || !retired_account_ids.insert(account_id) {
                return Err(AccountStoreConfigError::RetiredAccountId);
            }
        }
        let mut builder = AccountGenerationBuilder::new();
        for account in &snapshot.accounts {
            let account_id = AccountId::from_bytes(account.account_id);
            builder.add_account(
                AccountDefinition::new(
                    account.username.clone(),
                    account_id.clone(),
                    account.enabled,
                    *account.verifier,
                )
                .with_global_privileges(GlobalPrivileges::new(
                    account.global_connect,
                    account.global_list,
                )),
            );
            for grant in &account.database_grants {
                if grant.bits == 0
                    || grant.bits
                        & !(DATABASE_GRANT_CONNECT_BIT
                            | DATABASE_GRANT_QUERY_BIT
                            | DATABASE_GRANT_CREATE_BIT
                            | DATABASE_GRANT_DROP_BIT)
                        != 0
                {
                    return Err(AccountStoreConfigError::InvalidDatabasePrivileges {
                        database: grant.database_name.clone(),
                    });
                }
                builder.add_grant(DatabaseGrant::new(
                    account_id.clone(),
                    grant.database_name.clone(),
                    DatabasePrivileges::new(
                        grant.bits & DATABASE_GRANT_CONNECT_BIT != 0,
                        grant.bits & DATABASE_GRANT_QUERY_BIT != 0,
                        grant.bits & DATABASE_GRANT_CREATE_BIT != 0,
                        grant.bits & DATABASE_GRANT_DROP_BIT != 0,
                    ),
                ));
            }
        }
        builder.build_with_history(revision, retired_account_ids, HashSet::new())
    }

    pub(crate) fn snapshot(&self, store_id: [u8; SHA256_DIGEST_LENGTH]) -> StoredAuthSnapshot {
        let accounts = self
            .accounts_by_username
            .iter()
            .map(|(username, account)| {
                let authorization = self
                    .authorizations
                    .get(&account.account_id)
                    .expect("every account has authorization state");
                let database_grants = authorization
                    .database_privileges
                    .iter()
                    .map(|(database_name, privileges)| StoredDatabaseGrant {
                        database_name: database_name.clone(),
                        bits: database_privilege_bits(*privileges),
                    })
                    .collect();
                StoredAccountRecord {
                    username: username.clone(),
                    account_id: *account.account_id.as_bytes(),
                    enabled: authorization.enabled,
                    verifier: Box::new(*account.credential.verifier_material()),
                    global_connect: authorization.global_privileges.connect,
                    global_list: authorization.global_privileges.list,
                    database_grants,
                }
            })
            .collect();
        StoredAuthSnapshot {
            store_id,
            revision: self.revision,
            retired_account_ids: self
                .retired_account_ids
                .iter()
                .map(|account_id| *account_id.as_bytes())
                .collect(),
            accounts,
        }
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    fn authorize(
        &self,
        account_id: &AccountId,
        action: DatabaseAction<'_>,
    ) -> Result<(), AuthorizationError> {
        let authorization = self
            .authorizations
            .get(account_id)
            .filter(|authorization| authorization.enabled)
            .ok_or(AuthorizationError::Denied)?;
        if !authorization.global_privileges.connect {
            return Err(AuthorizationError::Denied);
        }

        match action {
            DatabaseAction::Connect { database: None } => Ok(()),
            DatabaseAction::Connect {
                database: Some(database),
            } => authorization.require_database(database, DatabasePermission::Connect),
            DatabaseAction::Query { database } => {
                authorization.require_database(database, DatabasePermission::Query)
            }
            DatabaseAction::Create { database } => {
                authorization.require_database(database, DatabasePermission::Create)
            }
            DatabaseAction::Drop { database } => {
                authorization.require_database(database, DatabasePermission::Drop)
            }
            DatabaseAction::List if authorization.global_privileges.list => Ok(()),
            DatabaseAction::List => Err(AuthorizationError::Denied),
        }
    }
}

fn database_privilege_bits(privileges: DatabasePrivileges) -> u8 {
    (u8::from(privileges.connect) * DATABASE_GRANT_CONNECT_BIT)
        | (u8::from(privileges.query) * DATABASE_GRANT_QUERY_BIT)
        | (u8::from(privileges.create) * DATABASE_GRANT_CREATE_BIT)
        | (u8::from(privileges.drop) * DATABASE_GRANT_DROP_BIT)
}

struct StoredAccount {
    account_id: AccountId,
    credential: Box<StoredCredential>,
}

struct AccountAuthorization {
    enabled: bool,
    global_privileges: GlobalPrivileges,
    database_privileges: BTreeMap<String, DatabasePrivileges>,
}

impl AccountAuthorization {
    fn require_database(
        &self,
        database: &str,
        required: DatabasePermission,
    ) -> Result<(), AuthorizationError> {
        let privileges = self
            .database_privileges
            .get(database)
            .ok_or(AuthorizationError::Denied)?;
        if required.is_granted_by(*privileges) {
            Ok(())
        } else {
            Err(AuthorizationError::Denied)
        }
    }
}

#[derive(Clone, Copy)]
enum DatabasePermission {
    Connect,
    Query,
    Create,
    Drop,
}

impl DatabasePermission {
    const fn is_granted_by(self, privileges: DatabasePrivileges) -> bool {
        match self {
            Self::Connect => privileges.connect,
            Self::Query => privileges.query,
            Self::Create => privileges.create,
            Self::Drop => privileges.drop,
        }
    }
}

fn validate_canonical_database_name(database: &str) -> Result<(), AccountStoreConfigError> {
    if canonicalize_database_name(database).is_ok_and(|canonical| canonical == database) {
        Ok(())
    } else {
        Err(AccountStoreConfigError::InvalidDatabaseName {
            database: database.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Barrier,
        },
        thread,
    };

    use super::*;

    fn account_id(byte: u8) -> AccountId {
        AccountId::from_bytes([byte; SHA256_DIGEST_LENGTH])
    }

    fn principal(byte: u8) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::from_account_id_for_testing(account_id(byte))
    }

    fn account(username: &str, id: u8, verifier: u8) -> AccountDefinition {
        AccountDefinition::new(
            username,
            account_id(id),
            true,
            [verifier; SHA256_DIGEST_LENGTH],
        )
        .with_global_privileges(GlobalPrivileges::new(true, false))
    }

    fn query_grant(id: u8, database: &str) -> DatabaseGrant {
        DatabaseGrant::new(
            account_id(id),
            database,
            DatabasePrivileges::new(false, true, false, false),
        )
    }

    #[test]
    fn lookup_and_authorization_share_one_complete_generation() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new()
                .with_account(
                    account("alice", 1, 0x11)
                        .with_global_privileges(GlobalPrivileges::new(true, true)),
                )
                .with_grant(DatabaseGrant::new(
                    account_id(1),
                    "reports",
                    DatabasePrivileges::new(true, true, true, true),
                )),
        )
        .unwrap();

        let snapshot = store.lookup("alice").unwrap().unwrap();
        assert_eq!(
            snapshot.credential().verifier_material(),
            &[0x11; SHA256_DIGEST_LENGTH]
        );
        assert_eq!(snapshot.credential().fast_cache_verifier(), None);
        let principal = principal(1);
        for action in [
            DatabaseAction::Connect { database: None },
            DatabaseAction::Connect {
                database: Some("reports"),
            },
            DatabaseAction::Query {
                database: "reports",
            },
            DatabaseAction::Create {
                database: "reports",
            },
            DatabaseAction::Drop {
                database: "reports",
            },
            DatabaseAction::List,
        ] {
            assert_eq!(store.authorize(&principal, action), Ok(()));
        }
    }

    #[test]
    fn authorization_requires_global_connect_and_the_exact_action_permission() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new()
                .with_account(account("alice", 1, 0x11))
                .with_account(
                    account("bob", 2, 0x22)
                        .with_global_privileges(GlobalPrivileges::new(false, true)),
                )
                .with_account(
                    AccountDefinition::new(
                        "carol",
                        account_id(3),
                        false,
                        [0x33; SHA256_DIGEST_LENGTH],
                    )
                    .with_global_privileges(GlobalPrivileges::new(true, true)),
                )
                .with_grant(query_grant(1, "reports"))
                .with_grant(DatabaseGrant::new(
                    account_id(2),
                    "reports",
                    DatabasePrivileges::new(true, true, true, true),
                ))
                .with_grant(DatabaseGrant::new(
                    account_id(3),
                    "reports",
                    DatabasePrivileges::new(true, true, true, true),
                )),
        )
        .unwrap();

        assert_eq!(
            store.authorize(&principal(1), DatabaseAction::Connect { database: None }),
            Ok(())
        );
        assert_eq!(
            store.authorize(
                &principal(1),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Ok(())
        );
        for action in [
            DatabaseAction::Connect {
                database: Some("reports"),
            },
            DatabaseAction::Create {
                database: "reports",
            },
            DatabaseAction::Drop {
                database: "reports",
            },
            DatabaseAction::List,
        ] {
            assert_eq!(
                store.authorize(&principal(1), action),
                Err(AuthorizationError::Denied)
            );
        }

        let all_actions = [
            DatabaseAction::Connect { database: None },
            DatabaseAction::Connect {
                database: Some("reports"),
            },
            DatabaseAction::Query {
                database: "reports",
            },
            DatabaseAction::Create {
                database: "reports",
            },
            DatabaseAction::Drop {
                database: "reports",
            },
            DatabaseAction::List,
        ];
        for principal in [principal(2), principal(3), principal(99)] {
            for action in all_actions {
                assert_eq!(
                    store.authorize(&principal, action),
                    Err(AuthorizationError::Denied)
                );
            }
        }
    }

    #[test]
    fn replacement_validates_every_record_and_advances_one_revision() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new().with_account(account("alice", 1, 0x11)),
        )
        .unwrap();
        assert_eq!(store.revision(), Ok(0));
        assert_eq!(
            store.replace(
                0,
                AccountGenerationBuilder::new()
                    .with_account(account("alice", 1, 0x22))
                    .with_grant(query_grant(1, "Reports")),
            ),
            Err(AccountStoreReplaceError::InvalidGeneration(
                AccountStoreConfigError::InvalidDatabaseName {
                    database: "Reports".to_owned(),
                }
            ))
        );
        assert_eq!(store.revision(), Ok(0));
        assert_eq!(
            store.replace(4, AccountGenerationBuilder::new()),
            Err(AccountStoreReplaceError::Conflict {
                expected: 4,
                actual: 0,
            })
        );
        assert_eq!(
            store.replace(
                0,
                AccountGenerationBuilder::new().with_account(account("alice", 1, 0x22))
            ),
            Ok(1)
        );
        assert_eq!(store.revision(), Ok(1));
    }

    #[test]
    fn builder_rejects_ambiguous_or_untrusted_records() {
        let duplicate_username = AccountGenerationBuilder::new()
            .with_account(account("alice", 1, 0x11))
            .with_account(account("alice", 2, 0x22));
        assert!(matches!(
            AccountStore::new(duplicate_username),
            Err(AccountStoreConfigError::DuplicateUsername { .. })
        ));

        let duplicate_id = AccountGenerationBuilder::new()
            .with_account(account("alice", 1, 0x11))
            .with_account(account("bob", 1, 0x22));
        assert!(matches!(
            AccountStore::new(duplicate_id),
            Err(AccountStoreConfigError::DuplicateAccountId)
        ));

        let zero_id = AccountGenerationBuilder::new().with_account(account("alice", 0, 0x11));
        assert!(matches!(
            AccountStore::new(zero_id),
            Err(AccountStoreConfigError::ZeroAccountId { .. })
        ));

        let unknown_owner = AccountGenerationBuilder::new()
            .with_account(account("alice", 1, 0x11))
            .with_grant(query_grant(2, "reports"));
        assert!(matches!(
            AccountStore::new(unknown_owner),
            Err(AccountStoreConfigError::UnknownGrantOwner)
        ));
    }

    #[test]
    fn revoking_a_grant_changes_the_next_authorization_only() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new()
                .with_account(account("alice", 1, 0x11))
                .with_grant(query_grant(1, "reports")),
        )
        .unwrap();
        let principal = principal(1);
        assert_eq!(
            store.authorize(
                &principal,
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Ok(())
        );

        store
            .replace(
                0,
                AccountGenerationBuilder::new().with_account(account("alice", 1, 0x22)),
            )
            .unwrap();
        assert_eq!(
            store.authorize(
                &principal,
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Err(AuthorizationError::Denied)
        );
    }

    #[test]
    fn a_lookup_snapshot_keeps_its_old_full_verifier_after_replacement() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new().with_account(account("alice", 1, 0x11)),
        )
        .unwrap();
        let old = store.lookup("alice").unwrap().unwrap();

        store
            .replace(
                0,
                AccountGenerationBuilder::new().with_account(account("alice", 1, 0x22)),
            )
            .unwrap();
        let current = store.lookup("alice").unwrap().unwrap();
        assert_eq!(
            old.credential().verifier_material(),
            &[0x11; SHA256_DIGEST_LENGTH]
        );
        assert_eq!(
            current.credential().verifier_material(),
            &[0x22; SHA256_DIGEST_LENGTH]
        );
        assert_eq!(old.credential().fast_cache_verifier(), None);
        assert_eq!(current.credential().fast_cache_verifier(), None);
    }

    #[test]
    fn recreating_a_username_never_reuses_its_old_grants() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new()
                .with_account(account("alice", 1, 0x11))
                .with_grant(query_grant(1, "reports")),
        )
        .unwrap();
        assert_eq!(
            store.authorize(
                &principal(1),
                DatabaseAction::Query {
                    database: "reports"
                }
            ),
            Ok(())
        );

        store
            .replace(
                0,
                AccountGenerationBuilder::new().with_account(account("alice", 2, 0x22)),
            )
            .unwrap();
        for principal in [principal(1), principal(2)] {
            assert_eq!(
                store.authorize(
                    &principal,
                    DatabaseAction::Query {
                        database: "reports"
                    }
                ),
                Err(AuthorizationError::Denied)
            );
        }
    }

    #[test]
    fn removed_account_ids_cannot_be_reused_but_can_follow_a_username_rename() {
        let renamed = AccountStore::new(
            AccountGenerationBuilder::new().with_account(account("alice", 1, 0x11)),
        )
        .unwrap();
        assert_eq!(
            renamed.replace(
                0,
                AccountGenerationBuilder::new().with_account(account("bob", 1, 0x22)),
            ),
            Ok(1)
        );

        let store = AccountStore::new(
            AccountGenerationBuilder::new().with_account(account("alice", 1, 0x11)),
        )
        .unwrap();
        assert_eq!(store.replace(0, AccountGenerationBuilder::new()), Ok(1));
        assert_eq!(
            store.replace(
                1,
                AccountGenerationBuilder::new().with_account(account("bob", 1, 0x22)),
            ),
            Err(AccountStoreReplaceError::InvalidGeneration(
                AccountStoreConfigError::RetiredAccountId
            ))
        );
    }

    #[test]
    fn concurrent_readers_observe_only_complete_old_or_new_generations() {
        let store = AccountStore::new(
            AccountGenerationBuilder::new()
                .with_account(account("alice", 1, 0x11))
                .with_grant(query_grant(1, "reports")),
        )
        .unwrap();
        let reader_store = store.clone();
        let start = Arc::new(Barrier::new(2));
        let reader_start = Arc::clone(&start);
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::clone(&done);
        let reader = thread::spawn(move || {
            reader_start.wait();
            while !reader_done.load(Ordering::Acquire) {
                let snapshot = reader_store.lookup("alice").unwrap().unwrap();
                let verifier = snapshot.credential().verifier_material();
                assert!(
                    verifier == &[0x11; SHA256_DIGEST_LENGTH]
                        || verifier == &[0x22; SHA256_DIGEST_LENGTH]
                );
                let decision = reader_store.authorize(
                    &principal(1),
                    DatabaseAction::Query {
                        database: "reports",
                    },
                );
                assert!(decision == Ok(()) || decision == Err(AuthorizationError::Denied));
            }
        });

        start.wait();
        for revision in 0..64 {
            let verifier = if revision % 2 == 0 { 0x22 } else { 0x11 };
            let builder = if revision % 2 == 0 {
                AccountGenerationBuilder::new().with_account(account("alice", 1, verifier))
            } else {
                AccountGenerationBuilder::new()
                    .with_account(account("alice", 1, verifier))
                    .with_grant(query_grant(1, "reports"))
            };
            store.replace(revision, builder).unwrap();
        }
        done.store(true, Ordering::Release);
        reader.join().unwrap();
    }
}
