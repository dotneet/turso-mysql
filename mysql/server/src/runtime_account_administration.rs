//! Durable account changes requested by an authenticated MySQL session.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    provision_account, AccountGrantChange, AccountStoreCheckpoint, AccountStoreCheckpointAuthority,
    AccountStoreCheckpointReader, AccountStoreCheckpointRequest, AuthenticatedPrincipal,
    CheckpointAuthorityId, CheckpointPersistence, CheckpointReadError, DatabaseAction,
    DatabaseAuthorizer, FrontendErrorKind, GlobalPrivileges, OfflineAccountProvisioner,
    OfflineProvisioningError, ProtectedPassword, RuntimeAccountReload, RuntimeAccountStore,
    TablePrivileges,
};

/// One account change that has already passed SQL parsing and authorization.
pub enum AdminMutation<'a> {
    CreateUser {
        username: &'a str,
        password: &'a mut [u8],
    },
    GrantTableSelect {
        username: &'a str,
        database: &'a str,
        table: &'a str,
    },
    RevokeTableSelect {
        username: &'a str,
        database: &'a str,
        table: &'a str,
    },
}

/// Applies an account change only after the external checkpoint is durable.
pub trait AccountAdministration: Send + Sync {
    fn apply(
        &self,
        principal: &AuthenticatedPrincipal,
        mutation: AdminMutation<'_>,
    ) -> Result<(), FrontendErrorKind>;
}

/// The writer must read and compare-and-persist the same authority identity.
pub trait AccountStoreAdminAuthority:
    AccountStoreCheckpointReader + AccountStoreCheckpointAuthority + Send + Sync
{
}

impl<T> AccountStoreAdminAuthority for T where
    T: AccountStoreCheckpointReader + AccountStoreCheckpointAuthority + Send + Sync
{
}

pub(crate) struct RuntimeAccountAdministration {
    root: PathBuf,
    authority_id: CheckpointAuthorityId,
    authority: Mutex<AdminAuthority>,
    accounts: Arc<RuntimeAccountStore>,
    coordination_timeout: Duration,
}

impl RuntimeAccountAdministration {
    pub(crate) fn new(
        root: PathBuf,
        authority_id: CheckpointAuthorityId,
        authority: Box<dyn AccountStoreAdminAuthority>,
        accounts: Arc<RuntimeAccountStore>,
        coordination_timeout: Duration,
    ) -> Self {
        Self {
            root,
            authority_id,
            authority: Mutex::new(AdminAuthority(authority)),
            accounts,
            coordination_timeout,
        }
    }
}

impl AccountAdministration for RuntimeAccountAdministration {
    fn apply(
        &self,
        principal: &AuthenticatedPrincipal,
        mutation: AdminMutation<'_>,
    ) -> Result<(), FrontendErrorKind> {
        match self.accounts.reload_once() {
            RuntimeAccountReload::Healthy(_) => {}
            RuntimeAccountReload::Degraded(_) => return Err(FrontendErrorKind::AccessDenied),
        }
        self.accounts
            .authorize(principal, DatabaseAction::ManageAccounts)
            .map_err(|_| FrontendErrorKind::AccessDenied)?;
        let deadline = Instant::now()
            .checked_add(self.coordination_timeout)
            .ok_or(FrontendErrorKind::Internal)?;
        let mut authority = self
            .authority
            .lock()
            .map_err(|_| FrontendErrorKind::Internal)?;
        match mutation {
            AdminMutation::CreateUser { username, password } => {
                let account = provision_account(
                    username,
                    ProtectedPassword::new(password),
                    true,
                    GlobalPrivileges::new(true, false),
                )
                .map_err(map_provisioning_error)?;
                OfflineAccountProvisioner::add_account_with_grants_crash_safe_authorized(
                    &self.root,
                    self.authority_id.clone(),
                    account,
                    ([], []),
                    principal.account_id(),
                    &mut *authority,
                    deadline,
                )
                .map_err(map_provisioning_error)?;
            }
            AdminMutation::GrantTableSelect {
                username,
                database,
                table,
            } => {
                OfflineAccountProvisioner::change_grant_crash_safe(
                    &self.root,
                    self.authority_id.clone(),
                    username,
                    AccountGrantChange::GrantTable {
                        database: database.to_owned(),
                        table: table.to_owned(),
                        privileges: TablePrivileges::new(true),
                    },
                    principal.account_id(),
                    &mut *authority,
                    deadline,
                )
                .map_err(map_provisioning_error)?;
            }
            AdminMutation::RevokeTableSelect {
                username,
                database,
                table,
            } => {
                OfflineAccountProvisioner::change_grant_crash_safe(
                    &self.root,
                    self.authority_id.clone(),
                    username,
                    AccountGrantChange::RevokeTable {
                        database: database.to_owned(),
                        table: table.to_owned(),
                        privileges: TablePrivileges::new(true),
                    },
                    principal.account_id(),
                    &mut *authority,
                    deadline,
                )
                .map_err(map_provisioning_error)?;
            }
        }
        match self.accounts.reload_once() {
            RuntimeAccountReload::Healthy(_) => Ok(()),
            RuntimeAccountReload::Degraded(_) => Err(FrontendErrorKind::Internal),
        }
    }
}

fn map_provisioning_error(error: OfflineProvisioningError) -> FrontendErrorKind {
    match error {
        OfflineProvisioningError::InvalidUsername(_) => FrontendErrorKind::Syntax,
        OfflineProvisioningError::AccountAlreadyExists => FrontendErrorKind::DuplicateObject,
        OfflineProvisioningError::AccountMissing
        | OfflineProvisioningError::PrivilegeMissing
        | OfflineProvisioningError::NotAuthorized => FrontendErrorKind::AccessDenied,
        _ => FrontendErrorKind::Internal,
    }
}

struct AdminAuthority(Box<dyn AccountStoreAdminAuthority>);

impl AccountStoreCheckpointReader for AdminAuthority {
    fn request_checkpoint(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
        self.0.request_checkpoint(authority)
    }
}

impl AccountStoreCheckpointAuthority for AdminAuthority {
    fn serves_authority(&self, authority: &CheckpointAuthorityId) -> bool {
        self.0.serves_authority(authority)
    }

    fn compare_and_persist(
        &mut self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> CheckpointPersistence {
        self.0.compare_and_persist(expected, replacement)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;
    use crate::{
        AccountDefinition, AccountGenerationBuilder, AccountId, AuthenticatedPrincipal,
        AuthorizationError, CredentialProvider, DatabaseAuthorizer, RuntimeConfig, RuntimeLimits,
        RuntimeTimeouts, TableAction, UnixSocketConfig, MIN_WRITE_LIMIT,
    };

    #[derive(Clone, Default)]
    struct TestAuthority {
        checkpoint: Arc<Mutex<Option<AccountStoreCheckpoint>>>,
    }

    impl AccountStoreCheckpointReader for TestAuthority {
        fn request_checkpoint(
            &self,
            _authority: &CheckpointAuthorityId,
        ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
            let checkpoint = *self.checkpoint.lock().unwrap();
            Ok(AccountStoreCheckpointRequest::completed(
                checkpoint.ok_or(CheckpointReadError::Missing),
            ))
        }
    }

    impl AccountStoreCheckpointAuthority for TestAuthority {
        fn serves_authority(&self, authority: &CheckpointAuthorityId) -> bool {
            authority.as_str() == "admin-test"
        }

        fn compare_and_persist(
            &mut self,
            expected: Option<&AccountStoreCheckpoint>,
            replacement: &AccountStoreCheckpoint,
        ) -> CheckpointPersistence {
            let mut checkpoint = self.checkpoint.lock().unwrap();
            if checkpoint.as_ref() == Some(replacement) {
                return CheckpointPersistence::Durable;
            }
            if checkpoint.as_ref() != expected {
                return CheckpointPersistence::Conflict;
            }
            *checkpoint = Some(*replacement);
            CheckpointPersistence::Durable
        }
    }

    #[test]
    fn create_grant_revoke_publish_checkpoint_and_reload_the_live_store() {
        let root = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let account_root = root.path().join("accounts");
        let data_root = root.path().join("data");
        fs::create_dir(&account_root).unwrap();
        fs::create_dir(&data_root).unwrap();
        fs::set_permissions(&account_root, fs::Permissions::from_mode(0o700)).unwrap();
        let authority_id = CheckpointAuthorityId::new("admin-test").unwrap();
        let mut authority = TestAuthority::default();
        let initial = AccountGenerationBuilder::new().with_account(
            AccountDefinition::new("admin", AccountId::from_bytes([0x11; 32]), true, [0x22; 32])
                .with_global_privileges(
                    GlobalPrivileges::new(true, true).with_manage_accounts(true),
                ),
        );
        OfflineAccountProvisioner::initialize(&account_root, initial, &mut authority).unwrap();
        let config = RuntimeConfig::new(
            None,
            Some(UnixSocketConfig::new(root.path(), "mysql.sock").unwrap()),
            &data_root,
            &account_root,
            authority_id.clone(),
            Duration::from_secs(5),
            RuntimeLimits::new(16, 16, MIN_WRITE_LIMIT, 16).unwrap(),
            RuntimeTimeouts::new(
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(60),
                Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .unwrap(),
        )
        .unwrap();
        let accounts =
            Arc::new(RuntimeAccountStore::open(&config, Arc::new(authority.clone())).unwrap());
        let administration = RuntimeAccountAdministration::new(
            account_root.clone(),
            authority_id,
            Box::new(authority.clone()),
            Arc::clone(&accounts),
            Duration::from_secs(5),
        );
        let admin_principal =
            AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([0x11; 32]));

        let mut password = b"reader-secret".to_vec();
        administration
            .apply(
                &admin_principal,
                AdminMutation::CreateUser {
                    username: "reader",
                    password: &mut password,
                },
            )
            .unwrap();
        assert!(password.iter().all(|byte| *byte == 0));
        assert_eq!(accounts.revision(), Ok(1));
        let mut repeated_password = b"another-secret".to_vec();
        assert_eq!(
            administration.apply(
                &admin_principal,
                AdminMutation::CreateUser {
                    username: "reader",
                    password: &mut repeated_password,
                }
            ),
            Err(FrontendErrorKind::DuplicateObject)
        );
        assert!(repeated_password.iter().all(|byte| *byte == 0));
        assert_eq!(accounts.revision(), Ok(1));
        let account = accounts.lookup("reader").unwrap().unwrap();
        let principal =
            AuthenticatedPrincipal::from_account_id_for_testing(account.account_id().clone());
        let table = TableAction::Select {
            database: "reports",
            table: "records",
        };
        assert_eq!(
            accounts.authorize_table(&principal, table),
            Err(AuthorizationError::Denied)
        );
        assert_eq!(
            administration.apply(
                &principal,
                AdminMutation::GrantTableSelect {
                    username: "reader",
                    database: "reports",
                    table: "records",
                }
            ),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(accounts.revision(), Ok(1));

        administration
            .apply(
                &admin_principal,
                AdminMutation::GrantTableSelect {
                    username: "reader",
                    database: "reports",
                    table: "records",
                },
            )
            .unwrap();
        assert_eq!(accounts.revision(), Ok(2));
        assert_eq!(accounts.authorize_table(&principal, table), Ok(()));

        administration
            .apply(
                &admin_principal,
                AdminMutation::RevokeTableSelect {
                    username: "reader",
                    database: "reports",
                    table: "records",
                },
            )
            .unwrap();
        assert_eq!(accounts.revision(), Ok(3));
        assert_eq!(
            accounts.authorize_table(&principal, table),
            Err(AuthorizationError::Denied)
        );
        let checkpoint = authority.checkpoint.lock().unwrap().unwrap();
        let reopened = crate::PersistentAccountStore::open(&account_root, &checkpoint).unwrap();
        assert_eq!(reopened.revision(), Ok(3));
        assert_eq!(
            administration.apply(
                &admin_principal,
                AdminMutation::GrantTableSelect {
                    username: "missing",
                    database: "reports",
                    table: "records",
                }
            ),
            Err(FrontendErrorKind::AccessDenied)
        );
        assert_eq!(accounts.revision(), Ok(3));
    }
}
