//! Stellar JSON-RPC 2.0 server for henyey.
//!
//! Implements the Stellar RPC API (SEP-35), serving JSON-RPC 2.0 requests
//! over a single `POST /` HTTP endpoint.

mod context;
mod dispatch;
mod error;
pub mod methods;
mod server;
pub mod simulate;
pub mod types;

pub use context::RpcContext;
pub use server::RpcServer;
