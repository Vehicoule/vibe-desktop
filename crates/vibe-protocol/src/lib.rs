//! Typed client for the Mistral Vibe app-server protocol (NDJSON JSON-RPC 2.0
//! over stdio). See `vibe/app_server/` in mistral-vibe for the authoritative
//! models this mirrors.

pub mod client;
pub mod json_patch;
pub mod models;
pub mod projection;

pub use client::{ClientError, ClientResult, Connection};
pub use models::*;
pub use projection::{Projection, Reduce};
