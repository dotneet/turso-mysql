// Copyright 2026 the Turso authors. All rights reserved. MIT license.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    os::unix::process::ExitStatusExt,
    path::Path,
    process::{Child, Command},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use turso_mysql_server::{
    provision_account, AccountDefinition, AccountGenerationBuilder, AccountId,
    AccountStoreCheckpoint, AccountStoreCheckpointAuthority, AccountStoreCheckpointReader,
    AccountStoreCheckpointRequest, CheckpointAuthorityId, CheckpointPersistence,
    CheckpointReadError, CrashSafeReconcileOutcome, CredentialProvider, DatabasePrivileges,
    GlobalPrivileges, OfflineAccountProvisioner, OfflineProvisioningError,
    PersistentAccountStoreError, ProtectedPassword, ReloadOutcome, RuntimeAccountReload,
    RuntimeAccountStore, RuntimeAccountStoreError, RuntimeConfig, RuntimeLimits, RuntimeTimeouts,
    UnixSocketConfig, MIN_WRITE_LIMIT,
};

use crate::{
    AuthorityId, CheckpointAuthority, CheckpointAuthorityConfig, CheckpointAuthorityShutdown,
    UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
};

const AUTHORITY_NAME: &str = "runtime-control-plane";
const ACCOUNT_SNAPSHOT_NAME: &str = ".turso-mysql-authz-v1";
const PROVISIONING_JOURNAL_NAME: &str = ".turso-mysql-provision-pending-v1";
const ADD_ACCOUNT_CRASH_POINT_ENV: &str = "TURSO_MYSQL_ADD_ACCOUNT_CRASH_POINT";
const ADD_ACCOUNT_CRASH_ROOT_ENV: &str = "TURSO_MYSQL_ADD_ACCOUNT_CRASH_ROOT";
const AUTHORITY_SOCKET_ENV: &str = "TURSO_MYSQL_CHECKPOINT_AUTHORITY_SOCKET";
const CHECKPOINT_BYTES: usize = 32 * 2 + 8;

struct DurableButAmbiguousClient {
    inner: UnixCheckpointAuthorityClient,
}

impl AccountStoreCheckpointReader for DurableButAmbiguousClient {
    fn request_checkpoint(
        &self,
        authority: &CheckpointAuthorityId,
    ) -> Result<AccountStoreCheckpointRequest, CheckpointReadError> {
        self.inner.request_checkpoint(authority)
    }
}

impl AccountStoreCheckpointAuthority for DurableButAmbiguousClient {
    fn serves_authority(&self, authority: &CheckpointAuthorityId) -> bool {
        self.inner.serves_authority(authority)
    }

    fn compare_and_persist(
        &mut self,
        expected: Option<&AccountStoreCheckpoint>,
        replacement: &AccountStoreCheckpoint,
    ) -> CheckpointPersistence {
        assert_eq!(
            self.inner.compare_and_persist(expected, replacement),
            CheckpointPersistence::Durable
        );
        CheckpointPersistence::Ambiguous
    }
}

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

fn read_authority_checkpoint(endpoint: &Path) -> Option<AccountStoreCheckpoint> {
    let client = UnixCheckpointAuthorityClient::new(client_config(endpoint)).unwrap();
    match client.get_checkpoint().unwrap() {
        crate::UnixCheckpointAuthorityGet::Checkpoint(checkpoint) => Some(checkpoint),
        crate::UnixCheckpointAuthorityGet::Missing => None,
    }
}

fn read_account_file(root: &Path, name: &str) -> Option<Vec<u8>> {
    match fs::read(root.join(name)) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("could not read account-store file {name}: {error}"),
    }
}

fn decode_pending_journal(
    journal: &[u8],
    operation: u8,
) -> (Option<AccountStoreCheckpoint>, AccountStoreCheckpoint) {
    assert!(journal.len() >= 8 + 1 + CHECKPOINT_BYTES);
    assert_eq!(&journal[..4], b"TMCP");
    assert_eq!(journal[4], 1);
    assert_eq!(journal[5], operation);
    let authority_len = usize::from(u16::from_be_bytes([journal[6], journal[7]]));
    let authority_end = 8 + authority_len;
    assert_eq!(&journal[8..authority_end], AUTHORITY_NAME.as_bytes());
    let expected_tag = journal[authority_end];
    let expected_start = authority_end + 1;
    let (expected, replacement_start) = match expected_tag {
        0 => (None, expected_start),
        1 => {
            let expected_end = expected_start + CHECKPOINT_BYTES;
            (
                Some(
                    AccountStoreCheckpoint::from_bytes(&journal[expected_start..expected_end])
                        .unwrap(),
                ),
                expected_end,
            )
        }
        _ => panic!("pending journal has an unknown expected-checkpoint tag"),
    };
    let replacement_end = replacement_start + CHECKPOINT_BYTES;
    assert_eq!(journal.len(), replacement_end + 32);
    let replacement =
        AccountStoreCheckpoint::from_bytes(&journal[replacement_start..replacement_end]).unwrap();
    (expected, replacement)
}

fn kill_child_after_stop(child: &mut Child, point: &str) {
    let pid = child.id().try_into().expect("child PID fits pid_t");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut status = 0;
        // SAFETY: pid identifies the child owned by `child`, and status is writable storage.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG | libc::WUNTRACED) };
        if waited == pid {
            assert!(
                libc::WIFSTOPPED(status),
                "operation child exited before stopping at {point}"
            );
            break;
        }
        assert_ne!(waited, -1, "could not wait for operation child at {point}");
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("operation child did not stop at {point}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
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
    OfflineAccountProvisioner::initialize_crash_safe(
        &roots.accounts,
        authority_id(),
        generation(0x11),
        &mut provision_client,
        Instant::now() + Duration::from_secs(1),
    )
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
fn real_service_reconciles_a_durable_but_ambiguous_account_replacement() {
    let roots = TestRoots::new();
    let (endpoint, shutdown, run) = start_service(&roots);
    let mut initialize_client =
        UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    OfflineAccountProvisioner::initialize_crash_safe(
        &roots.accounts,
        authority_id(),
        generation(0x11),
        &mut initialize_client,
        Instant::now() + Duration::from_secs(1),
    )
    .unwrap();

    let mut password = *b"correct horse battery staple";
    let account = provision_account(
        "bob",
        ProtectedPassword::new(&mut password),
        true,
        GlobalPrivileges::new(true, false),
    )
    .unwrap();
    let grants = [account.grant("reports", DatabasePrivileges::new(true, true, false, false))];
    let mut ambiguous_client = DurableButAmbiguousClient {
        inner: UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap(),
    };
    assert!(matches!(
        OfflineAccountProvisioner::add_account_crash_safe(
            &roots.accounts,
            authority_id(),
            account,
            grants,
            &mut ambiguous_client,
            Instant::now() + Duration::from_secs(1),
        ),
        Err(OfflineProvisioningError::CheckpointAmbiguous(pending))
            if pending.durable_revision() == 1
    ));
    assert!(roots.accounts.join(PROVISIONING_JOURNAL_NAME).exists());

    let reader = Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
    let authority_durable =
        RuntimeAccountStore::open(&runtime_config(&roots.accounts), reader).unwrap();
    assert_eq!(authority_durable.revision(), Ok(1));
    assert!(authority_durable.lookup("alice").unwrap().is_some());
    assert!(authority_durable.lookup("bob").unwrap().is_some());

    let mut reconcile_client =
        UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
    assert_eq!(
        OfflineAccountProvisioner::reconcile_crash_safe(
            &roots.accounts,
            &authority_id(),
            &mut reconcile_client,
            Instant::now() + Duration::from_secs(1),
        ),
        Ok(CrashSafeReconcileOutcome::Reconciled { revision: 1 })
    );
    assert!(!roots.accounts.join(PROVISIONING_JOURNAL_NAME).exists());

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

#[test]
fn real_service_process_kill_recovers_each_add_account_journal_boundary() {
    if let Some(point) = std::env::var_os(ADD_ACCOUNT_CRASH_POINT_ENV) {
        let root = std::env::var_os(ADD_ACCOUNT_CRASH_ROOT_ENV)
            .expect("add-account crash child requires an account root");
        let endpoint = std::env::var_os(AUTHORITY_SOCKET_ENV)
            .expect("add-account crash child requires an authority socket");
        let mut client =
            UnixCheckpointAuthorityClient::new(client_config(Path::new(&endpoint))).unwrap();
        let mut password = *b"correct horse battery staple";
        let account = provision_account(
            "bob",
            ProtectedPassword::new(&mut password),
            true,
            GlobalPrivileges::new(true, false),
        )
        .unwrap();
        let grants = [account.grant("reports", DatabasePrivileges::new(true, true, false, false))];
        let _ = OfflineAccountProvisioner::add_account_crash_safe(
            root,
            authority_id(),
            account,
            grants,
            &mut client,
            Instant::now() + Duration::from_secs(5),
        );
        panic!(
            "add-account crash-point child continued past {}",
            point.to_string_lossy()
        );
    }

    for point in [
        "after-journal-publish",
        "after-snapshot-publish",
        "after-durable-cas",
        "after-journal-clear",
    ] {
        let roots = TestRoots::new();
        let (endpoint, setup_shutdown, setup_run) = start_service(&roots);
        let mut setup_client =
            UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
        OfflineAccountProvisioner::initialize_crash_safe(
            &roots.accounts,
            authority_id(),
            generation(0x11),
            &mut setup_client,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        let expected = read_authority_checkpoint(&endpoint).expect("initial CAS must be durable");
        let expected_snapshot =
            read_account_file(&roots.accounts, ACCOUNT_SNAPSHOT_NAME).expect("initial snapshot");

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("integration_tests::real_service_process_kill_recovers_each_add_account_journal_boundary")
            .arg("--nocapture")
            .env(ADD_ACCOUNT_CRASH_POINT_ENV, point)
            .env(ADD_ACCOUNT_CRASH_ROOT_ENV, &roots.accounts)
            .env(AUTHORITY_SOCKET_ENV, &endpoint)
            .spawn()
            .unwrap();
        kill_child_after_stop(&mut child, point);

        let authority_before = read_authority_checkpoint(&endpoint);
        let snapshot_before = read_account_file(&roots.accounts, ACCOUNT_SNAPSHOT_NAME);
        let journal_before = read_account_file(&roots.accounts, PROVISIONING_JOURNAL_NAME);
        match point {
            "after-journal-publish" | "after-snapshot-publish" => {
                assert_eq!(authority_before, Some(expected));
                assert!(journal_before.is_some());
            }
            "after-durable-cas" => {
                assert!(journal_before.is_some());
            }
            "after-journal-clear" => {
                assert_eq!(
                    authority_before.map(|checkpoint| checkpoint.revision()),
                    Some(1)
                );
                assert_eq!(journal_before, None);
            }
            _ => unreachable!("unknown add-account crash point"),
        }
        if let Some(journal) = journal_before.as_ref() {
            let (journal_expected, journal_replacement) = decode_pending_journal(journal, 1);
            assert_eq!(journal_expected, Some(expected));
            assert_eq!(journal_replacement.revision(), 1);
            assert!(expected.belongs_to_same_store(journal_replacement));
            if point == "after-durable-cas" {
                assert_eq!(authority_before, Some(journal_replacement));
            }
        }
        match point {
            "after-journal-publish" => assert_eq!(snapshot_before, Some(expected_snapshot.clone())),
            "after-snapshot-publish" | "after-durable-cas" | "after-journal-clear" => {
                assert!(snapshot_before.is_some());
                assert_ne!(snapshot_before, Some(expected_snapshot.clone()));
            }
            _ => unreachable!("unknown add-account crash point"),
        }

        assert_eq!(
            read_authority_checkpoint(&endpoint),
            authority_before,
            "authority changed while the operation child was stopped at {point}"
        );
        assert_eq!(
            read_account_file(&roots.accounts, ACCOUNT_SNAPSHOT_NAME),
            snapshot_before,
            "snapshot changed while the operation child was stopped at {point}"
        );
        assert_eq!(
            read_account_file(&roots.accounts, PROVISIONING_JOURNAL_NAME),
            journal_before,
            "journal changed while the operation child was stopped at {point}"
        );

        setup_shutdown.shutdown();
        setup_run.join().unwrap();
        let (endpoint, shutdown, run) = start_service(&roots);
        let mut reconcile_client =
            UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap();
        let expected_outcome = match point {
            "after-journal-publish" => CrashSafeReconcileOutcome::AbortedBeforeSnapshot,
            "after-snapshot-publish" | "after-durable-cas" => {
                CrashSafeReconcileOutcome::Reconciled { revision: 1 }
            }
            "after-journal-clear" => CrashSafeReconcileOutcome::NoPendingUpdate,
            _ => unreachable!("unknown add-account crash point"),
        };
        assert_eq!(
            OfflineAccountProvisioner::reconcile_crash_safe(
                &roots.accounts,
                &authority_id(),
                &mut reconcile_client,
                Instant::now() + Duration::from_secs(5),
            ),
            Ok(expected_outcome),
            "unexpected add-account reconciliation result at {point}"
        );
        assert_eq!(
            read_account_file(&roots.accounts, PROVISIONING_JOURNAL_NAME),
            None
        );
        match point {
            "after-journal-publish" => {
                assert_eq!(read_authority_checkpoint(&endpoint), Some(expected));
                assert_eq!(
                    read_account_file(&roots.accounts, ACCOUNT_SNAPSHOT_NAME),
                    Some(expected_snapshot.clone())
                );
            }
            "after-snapshot-publish" | "after-durable-cas" | "after-journal-clear" => {
                let expected_checkpoint = if let Some(journal) = journal_before.as_ref() {
                    decode_pending_journal(journal, 1).1
                } else {
                    authority_before.expect("journal-clear authority state")
                };
                assert_eq!(
                    read_authority_checkpoint(&endpoint),
                    Some(expected_checkpoint)
                );
                assert_eq!(
                    read_account_file(&roots.accounts, ACCOUNT_SNAPSHOT_NAME),
                    snapshot_before
                );
            }
            _ => unreachable!("unknown add-account crash point"),
        }

        shutdown.shutdown();
        run.join().unwrap();
        let (endpoint, shutdown, run) = start_service(&roots);
        let reader =
            Arc::new(UnixCheckpointAuthorityClient::new(client_config(&endpoint)).unwrap());
        let restarted =
            RuntimeAccountStore::open(&runtime_config(&roots.accounts), reader).unwrap();
        assert_eq!(
            restarted.revision(),
            Ok(if point == "after-journal-publish" {
                0
            } else {
                1
            })
        );
        assert!(restarted.lookup("alice").unwrap().is_some());
        assert_eq!(
            restarted.lookup("bob").unwrap().is_some(),
            point != "after-journal-publish"
        );
        shutdown.shutdown();
        run.join().unwrap();
    }
}
