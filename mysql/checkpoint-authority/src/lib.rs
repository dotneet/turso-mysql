// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Durable local authority primitives for account-store rollback checkpoints.
//!
//! This crate owns the bounded wire format, durable authority state, and the
//! local Unix service that exposes them to explicitly configured clients.

#[cfg(unix)]
pub mod client;
#[cfg(all(test, unix))]
mod integration_tests;
#[cfg(unix)]
pub mod protocol;
#[cfg(unix)]
pub mod service;
#[cfg(unix)]
pub mod store;
#[cfg(unix)]
mod unix_fs;

#[cfg(unix)]
pub use client::{
    UnixCheckpointAuthorityClient, UnixCheckpointAuthorityClientConfig,
    UnixCheckpointAuthorityClientConfigError, UnixCheckpointAuthorityClientError,
    UnixCheckpointAuthorityGet, UnixCheckpointAuthorityGetError,
};
#[cfg(unix)]
pub use protocol::{
    decode_request, decode_response, encode_request, encode_response, AuthorityId, CasResponse,
    GetResponse, ProtocolError, Request, Response, CHECKPOINT_BYTES, MAX_FRAME_BYTES,
    MAX_FRAME_PAYLOAD_BYTES,
};
#[cfg(unix)]
pub use service::{
    CheckpointAuthority, CheckpointAuthorityBindError, CheckpointAuthorityConfig,
    CheckpointAuthorityConfigError, CheckpointAuthorityRunError, CheckpointAuthorityRunReport,
    CheckpointAuthorityShutdown, CheckpointAuthorityStats,
};
#[cfg(unix)]
pub use store::{CheckpointStore, CheckpointStoreCas, CheckpointStoreError};
