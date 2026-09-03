// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Tests run by the privileged Linux fixture, not ordinary Rust test runs.

#![cfg(unix)]

use std::{env, path::PathBuf, time::Duration};

use turso_mysql_checkpoint_authority::{
    AuthorityId, UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
    UnixCheckpointAuthorityGet, UnixCheckpointAuthorityGetError,
};

const SOCKET_ENV: &str = "TURSO_MYSQL_CROSS_UID_SOCKET";
const AUTHORITY_ENV: &str = "TURSO_MYSQL_CROSS_UID_AUTHORITY";
const SERVICE_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_SERVICE_UID";
const CLIENT_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_CLIENT_UID";
const FOREIGN_UID_ENV: &str = "TURSO_MYSQL_CROSS_UID_FOREIGN_UID";

#[test]
#[ignore = "requires the privileged Linux cross-UID fixture"]
fn configured_client_reads_the_durable_checkpoint() {
    assert_running_as(CLIENT_UID_ENV);
    let client = client();
    assert!(matches!(
        client.get_checkpoint(),
        Ok(UnixCheckpointAuthorityGet::Checkpoint(_))
    ));
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
