//! Library surface of the Use Pod provider agent.
//!
//! The binary (`src/main.rs`) is a thin wrapper around these modules so that
//! integration tests can exercise config parsing, identity, and protocol
//! plumbing directly.

pub mod backend;
pub mod config;
pub mod discovery;
pub mod heartbeat;
pub mod identity;
pub mod job_executor;
pub mod setup;
pub mod ws_client;
