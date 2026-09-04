// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Tests run by the privileged Linux fixture, not ordinary Rust test runs.

#![cfg(unix)]

use std::{env, path::PathBuf, time::Duration};

use turso_mysql_checkpoint_authority::{
    AuthorityId, UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
    UnixCheckpointAuthorityGet, UnixCheckpointAuthorityGetError,
};
use turso_mysql_server::{
    AuthenticatedPrincipal, AuthorizationError, CredentialProvider, DatabaseAction,
    DatabaseAuthorizer, PersistentAccountStore, TableAction,
};

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
    let gateadmin = principal_for(&store, "gateadmin");
    let reportreader = principal_for(&store, "reportreader");
    assert_eq!(
        store.authorize(
            &gateadmin,
            DatabaseAction::Query {
                database: "reports"
            },
        ),
        Ok(())
    );
    assert_eq!(
        store.authorize(
            &reportreader,
            DatabaseAction::Connect {
                database: Some("reports"),
            },
        ),
        Ok(())
    );
    assert_eq!(
        store.authorize(
            &reportreader,
            DatabaseAction::Query {
                database: "reports"
            },
        ),
        Err(AuthorizationError::Denied)
    );
    assert_eq!(
        store.authorize_table(
            &reportreader,
            TableAction::Select {
                database: "reports",
                table: "records",
            },
        ),
        Ok(())
    );
    assert_eq!(
        store.authorize_table(
            &reportreader,
            TableAction::Select {
                database: "reports",
                table: "other",
            },
        ),
        Err(AuthorizationError::Denied)
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

fn principal_for(store: &PersistentAccountStore, username: &str) -> AuthenticatedPrincipal {
    let snapshot = store
        .lookup(username)
        .expect("fixture account lookup succeeds")
        .expect("fixture account is present");
    AuthenticatedPrincipal::from_account_id_for_testing(snapshot.account_id().clone())
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
