// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Durable local authority primitives for account-store rollback checkpoints.
//!
//! This crate deliberately contains neither a listener nor a client.  The
//! future service owns Unix peer verification and lifecycle; these components
//! define the bounded wire format and the descriptor-backed authority state it
//! will use.

#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(not(unix))]
compile_error!("turso_mysql_checkpoint_authority requires a reviewed Unix target");

#[cfg(unix)]
pub mod protocol;
#[cfg(unix)]
pub mod store;

#[cfg(unix)]
pub use protocol::{
    decode_request, decode_response, encode_request, encode_response, AuthorityId, CasResponse,
    GetResponse, ProtocolError, Request, Response, CHECKPOINT_BYTES, MAX_FRAME_BYTES,
    MAX_FRAME_PAYLOAD_BYTES,
};
#[cfg(unix)]
pub use store::{CheckpointStore, CheckpointStoreCas, CheckpointStoreError};
