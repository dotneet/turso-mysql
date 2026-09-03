// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Tests run by the privileged Linux fixture, not ordinary Rust test runs.

#![cfg(unix)]

use std::{env, fs, path::PathBuf, time::Duration};

use turso_mysql_checkpoint_authority::{
    AuthorityId, UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
    UnixCheckpointAuthorityGet, UnixCheckpointAuthorityGetError,
};
use turso_mysql_server::{CredentialProvider, PersistentAccountStore};

const SOCKET_ENV: &str = "TURSO_MYSQL_CROSS_UID_SOCKET";
const AUTHORITY_ENV: &str = "TURSO_MYSQL_CROSS_UID_AUTHORITY";
const SERVICE_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_SERVICE_UID";
const CLIENT_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_CLIENT_UID";
const FOREIGN_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_FOREIGN_UID";
const ACCOUNT_STORE_ROOT_ENV: &str = "TURSO_MYSQL_CROSS_UID_ACCOUNT_STORE_ROOT";

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture"]
fn configured_client_observes_revised_accounts_and_grants() {
    assert_running_as(CLIENT_UID_ENV);
    let client = client();
    let checkpoint = match client.get_checkpoint() {
        Ok(UnixCheckpointAuthorityGet::Checkpoint(checkpoint)) => checkpoint,
        result => panic!("fixture authority did not return a checkpoint: {result:?}"),
    };
    assert_eq!(checkpoint.revision(), 1);

    let account_store_root = PathBuf::from(required(ACCOUNT_STORE_ROOT_ENV));
    let store = PersistentAccountStore::open(&account_store_root, &checkpoint)
        .expect("fixture account store opens with the authority checkpoint");
    assert_eq!(store.revision(), Ok(1));
    assert_eq!(store.checkpoint(), Ok(checkpoint));
    assert!(store
        .lookup("gateadmin")
        .expect("fixture account lookup succeeds")
        .is_some());
    assert!(store
        .lookup("reportreader")
        .expect("fixture account lookup succeeds")
        .is_some());
    let expected_grant = [0, 7, 3, 0, b'r', b'e', b'p', b'o', b'r', b't', b's'];
    let snapshot = fs::read(account_store_root.join(".turso-mysql-authz-v1"))
        .expect("fixture account snapshot is readable");
    assert_eq!(
        snapshot
            .windows(expected_grant.len())
            .filter(|entry| *entry == expected_grant)
            .count(),
        2
    );
}

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture"]
fn foreign_client_is_rejected_despite_socket_group_access() {
    assert_running_as(FOREIGN_UID_ENV);
    let client = client();
    assert_eq!(
        client.get_checkpoint(),
        Err(UnixCheckpointAuthorityGetError::Unavailable)
    );
}

fn client() -> UnixCheckpointAuthorityClient {
    let socket = PathBuf::from(required(SOCKET_ENV));
    let authority = AuthorityId::new(required(AUTHORITY_ENV)).expect("fixture authority is valid");
    let service_uid = required(SERVICE_UID_ENV)
        .parse()
        .expect("fixture service UID is valid");
    let configuration = UnixCheckpointAuthorityClientConfig::new(
        socket,
        authority,
        service_uid,
        Duration::from_secs(1),
    )
    .expect("fixture client configuration is valid");
    UnixCheckpointAuthorityClient::new(configuration).expect("peer verification is available")
}

fn assert_running_as(uid_environment: &str) {
    let expected = required(uid_environment)
        .parse::<u32>()
        .expect("fixture UID is valid");
    // SAFETY: geteuid has no arguments and only reads process credentials.
    assert_eq!(unsafe { libc::geteuid() }, expected);
}

fn required(name: &str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("fixture environment {name} is missing"))
}
