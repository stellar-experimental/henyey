//! Stellar JSON-RPC 2.0 server for henyey.
//!
//! Implements the Stellar RPC API, serving JSON-RPC 2.0 requests
//! over a single `POST /` HTTP endpoint.

mod context;
mod dispatch;
mod error;
mod fee_window;
mod methods;
mod server;
mod simulate;
mod types;
mod util;

pub use server::RpcServer;
