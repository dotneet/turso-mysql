// Copyright 2026 the Turso authors. All rights reserved. MIT license.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use turso_mysql_server::{
    provision_account, AccountDefinition, AccountGenerationBuilder, AccountId,
    CheckpointAuthorityId, CredentialProvider, DatabasePrivileges, GlobalPrivileges,
    OfflineAccountProvisioner, PersistentAccountStoreError, ProtectedPassword, ReloadOutcome,
    RuntimeAccountReload, RuntimeAccountStore, RuntimeAccountStoreError, RuntimeConfig,
    RuntimeLimits, RuntimeTimeouts, UnixSocketConfig, MIN_WRITE_LIMIT,
};

use crate::{
    AuthorityId, CheckpointAuthority, CheckpointAuthorityConfig, CheckpointAuthorityShutdown,
    UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
};

const AUTHORITY_NAME: &str = "runtime-control-plane";
const ACCOUNT_SNAPSHOT_NAME: &str = ".turso-mysql-authz-v1";

struct TestRoots {
    _parent: tempfile::TempDir,
    state: std::path::PathBuf,
    socket: std::path::PathBuf,
    winner_accounts: std::path::PathBuf,
    accounts: std::path::PathBuf,
}

impl TestRoots {
    fn new() -> Self {
        let parent = tempfile::Builder::new()
            .prefix("ca-e2e-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let state = private_child(parent.path(), "state", 0o700);
        let socket = private_child(parent.path(), "socket", 0o710);
        let winner_accounts = private_child(parent.path(), "winner-accounts", 0o700);
        let accounts = private_child(parent.path(), "accounts", 0o700);
        Self {
            _parent: parent,
            state,
            socket,
            winner_accounts,
            accounts,
        }
    }
}

fn private_child(parent: &Path, name: &str, mode: u32) -> std::path::PathBuf {
    let path = parent.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot access Rust-managed memory.
    unsafe { libc::geteuid() }
}

fn effective_gid() -> u32 {
    // SAFETY: getegid has no arguments and cannot access Rust-managed memory.
    unsafe { libc::getegid() }
}

fn authority_id() -> CheckpointAuthorityId {
    CheckpointAuthorityId::new(AUTHORITY_NAME).unwrap()
}

fn wire_authority_id() -> AuthorityId {
    AuthorityId::new(AUTHORITY_NAME).unwrap()
}

fn service_config(roots: &TestRoots) -> CheckpointAuthorityConfig {
    CheckpointAuthorityConfig::new(
        wire_authority_id(),
        &roots.state,
        &roots.socket,
        "authority.sock",
        effective_gid(),
        effective_uid(),
        Duration::from_secs(1),
    )
    .unwrap()
}

fn start_service(
    roots: &TestRoots,
) -> (
    std::path::PathBuf,
    CheckpointAuthorityShutdown,
    thread::JoinHandle<()>,
) {
    let service = CheckpointAuthority::bind_for_test(service_config(roots)).unwrap();
    let endpoint = service.socket_path().to_owned();
    let shutdown = service.shutdown_handle();
    let run = thread::spawn(move || {
        service.run().unwrap();
    });
    (endpoint, shutdown, run)
}

fn client_config(endpoint: &Path) -> UnixCheckpointAuthorityClientConfig {
    UnixCheckpointAuthorityClientConfig::new_for_test_same_uid(
        endpoint,
        wire_authority_id(),
        Duration::from_secs(1),
    )
    .unwrap()
}

fn runtime_config(account_root: &Path) -> RuntimeConfig {
    RuntimeConfig::new(
        None,
        Some(UnixSocketConfig::new("/run/turso", "mysql.sock").unwrap()),
        "/var/lib/turso/data",
        account_root,
        authority_id(),
        Duration::from_secs(5),
        RuntimeLimits::new(8, 8, MIN_WRITE_LIMIT, 8).unwrap(),
        RuntimeTimeouts::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
    )
    .unwrap()
}

fn generation(revision_material: u8) -> AccountGenerationBuilder {
    AccountGenerationBuilder::new().with_account(
        AccountDefinition::new(
            "alice",
            AccountId::from_bytes([7; 32]),
            true,
            [revision_material; 32],
        )
        .with_global_privileges(GlobalPrivileges::new(true, false)),
    )
}

#[test]
fn real_service_drives_provisioning_runtime_reload_and_restart() {
    let roots = TestRoots::new();
    let (endpoint, shutdown, run) = start_service(&roots);
    let mut provision_client =
        UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    let mut provisioner = OfflineAccountProvisioner::initialize(
        &roots.accounts,
        generation(0x11),
        &mut provision_client,
    )
    .unwrap();
    let runtime_reader =
        Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    let runtime =
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), runtime_reader).unwrap();
    assert_eq!(runtime.revision(), Ok(0));

    assert_eq!(
        provisioner
            .replace(generation(0x22), &mut provision_client)
            .unwrap(),
        1
    );
    assert_eq!(
        runtime.reload_once(),
        RuntimeAccountReload::Healthy(ReloadOutcome::Reloaded { revision: 1 })
    );
    assert_eq!(runtime.revision(), Ok(1));

    shutdown.shutdown();
    run.join().unwrap();
    let (endpoint, shutdown, run) = start_service(&roots);
    let restarted_reader =
        Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    let restarted =
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), restarted_reader).unwrap();
    assert_eq!(restarted.revision(), Ok(1));
    shutdown.shutdown();
    run.join().unwrap();
}

#[test]
fn real_service_crash_safe_add_account_with_grant_reloads_and_restarts_exactly() {
    let roots = TestRoots::new();
    let (endpoint, shutdown, run) = start_service(&roots);
    let mut provision_client =
        UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    OfflineAccountProvisioner::initialize(&roots.accounts, generation(0x11), &mut provision_client)
        .unwrap();

    let runtime_reader =
        Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    let runtime =
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), runtime_reader).unwrap();
    assert_eq!(runtime.revision(), Ok(0));
    assert!(runtime.lookup("alice").unwrap().is_some());
    assert!(runtime.lookup("bob").unwrap().is_none());

    let mut password = *b"correct horse battery staple";
    let account = provision_account(
        "bob",
        ProtectedPassword::new(&mut password),
        true,
        GlobalPrivileges::new(true, false),
    )
    .unwrap();
    let grants = [account.grant("reports", DatabasePrivileges::new(true, true, false, false))];
    let provisioner = OfflineAccountProvisioner::add_account_crash_safe(
        &roots.accounts,
        authority_id(),
        account,
        grants,
        &mut provision_client,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(provisioner.revision(), Ok(1));
    assert!(provisioner
        .store()
        .unwrap()
        .lookup("bob")
        .unwrap()
        .is_some());

    assert_eq!(
        runtime.reload_once(),
        RuntimeAccountReload::Healthy(ReloadOutcome::Reloaded { revision: 1 })
    );
    assert_eq!(runtime.revision(), Ok(1));
    assert!(runtime.lookup("bob").unwrap().is_some());

    shutdown.shutdown();
    run.join().unwrap();
    let (endpoint, shutdown, run) = start_service(&roots);
    let restarted_reader =
        Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    let restarted =
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), restarted_reader).unwrap();
    assert_eq!(restarted.revision(), Ok(1));
    assert!(restarted.lookup("alice").unwrap().is_some());
    assert!(restarted.lookup("bob").unwrap().is_some());
    shutdown.shutdown();
    run.join().unwrap();
}

#[test]
fn authority_rejects_a_rolled_back_account_snapshot() {
    let roots = TestRoots::new();
    let (endpoint, shutdown, run) = start_service(&roots);
    let mut client = UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    let mut provisioner =
        OfflineAccountProvisioner::initialize(&roots.accounts, generation(0x11), &mut client)
            .unwrap();
    let snapshot_path = roots.accounts.join(ACCOUNT_SNAPSHOT_NAME);
    let old_snapshot = fs::read(&snapshot_path).unwrap();
    provisioner.replace(generation(0x22), &mut client).unwrap();

    let mut snapshot = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&snapshot_path)
        .unwrap();
    snapshot.write_all(&old_snapshot).unwrap();
    snapshot.sync_all().unwrap();

    let reader = Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    assert!(matches!(
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), reader),
        Err(RuntimeAccountStoreError::Store(
            PersistentAccountStoreError::CheckpointMismatch
        ))
    ));
    shutdown.shutdown();
    run.join().unwrap();
}

#[test]
fn real_service_initial_conflict_aborts_uncheckpointed_snapshot_for_retry() {
    let roots = TestRoots::new();
    let (endpoint, shutdown, run) = start_service(&roots);
    let mut winner_client = UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    OfflineAccountProvisioner::initialize(
        &roots.winner_accounts,
        generation(0x11),
        &mut winner_client,
    )
    .unwrap();

    let mut contender_client =
        UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    let snapshot_path = roots.accounts.join(ACCOUNT_SNAPSHOT_NAME);
    assert!(matches!(
        OfflineAccountProvisioner::initialize(
            &roots.accounts,
            generation(0x22),
            &mut contender_client,
        ),
        Err(turso_mysql_server::OfflineProvisioningError::CheckpointConflict(pending))
            if pending.durable_revision() == 0
    ));
    assert!(!snapshot_path.exists());

    assert!(matches!(
        OfflineAccountProvisioner::initialize(
            &roots.accounts,
            generation(0x33),
            &mut contender_client,
        ),
        Err(turso_mysql_server::OfflineProvisioningError::CheckpointConflict(_))
    ));
    assert!(!snapshot_path.exists());

    shutdown.shutdown();
    run.join().unwrap();
}
