//! Application orchestration for rs-stellar-core.
//!
//! This crate provides the top-level application layer that wires together all
//! subsystems of a Stellar Core node. It is responsible for:
//!
//! - **Configuration management**: Loading and validating TOML configuration files
//!   with environment variable overrides ([`config`] module)
//! - **Application lifecycle**: Initializing, running, and gracefully shutting down
//!   all node components ([`app`] module)
//! - **Command execution**: Implementing CLI commands like `run` and `catchup`
//!   ([`run_cmd`] and [`catchup_cmd`] modules)
//! - **Logging and progress tracking**: Setting up structured logging and providing
//!   progress reporting for long-running operations ([`logging`] module)
//! - **Database maintenance**: Background cleanup of old ledger headers and SCP
//!   history to prevent unbounded database growth ([`maintainer`] module)
//! - **Metadata streaming**: Emitting `LedgerCloseMeta` frames to external consumers
//!   ([`meta_stream`] module)
//! - **Network surveys**: Collecting and reporting overlay network topology data
//!   ([`survey`] module)
//!
//! # Architecture
//!
//! The [`App`] struct is the central coordinator that owns handles to:
//! - Database for persistent storage
//! - [`BucketManager`](henyey_bucket::BucketManager) for ledger state
//! - [`LedgerManager`](henyey_ledger::LedgerManager) for ledger operations
//! - [`OverlayManager`](henyey_overlay::OverlayManager) for P2P networking
//! - [`Herder`](henyey_herder::Herder) for consensus coordination
//!
//! # Usage
//!
//! ```no_run
//! use henyey_app::{App, AppConfig, run_node, RunOptions};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Load configuration
//! let config = AppConfig::from_file("config.toml")?;
//!
//! // Run the node
//! run_node(config, RunOptions::default()).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Modules
//!
//! - [`app`]: Core application struct and component initialization
//! - [`catchup_cmd`]: History catchup command implementation
//! - [`config`]: Configuration loading and validation
//! - [`logging`]: Logging setup and progress tracking utilities
//! - [`maintainer`]: Background database maintenance scheduler
//! - [`meta_stream`]: Metadata output stream for `LedgerCloseMeta` emission
//! - [`run_cmd`]: Node run command and HTTP status server
//! - [`survey`]: Time-sliced overlay network survey management

pub mod app;
pub mod catchup_cmd;
pub mod config;
pub mod http;
pub mod logging;
pub mod maintainer;
pub mod meta_stream;
pub mod run_cmd;
pub mod survey;

pub use app::{App, AppState, CatchupResult, CatchupTarget, LedgerSummary, SurveyReport};
pub use catchup_cmd::{run_catchup, CatchupMode, CatchupOptions};
pub use config::AppConfig;
pub use logging::{init_with_handle, LogConfig, LogFormat, LogLevelHandle, LOG_PARTITIONS};
pub use maintainer::{
    Maintainer, MaintenanceConfig, DEFAULT_MAINTENANCE_COUNT, DEFAULT_MAINTENANCE_PERIOD,
};
pub use run_cmd::{run_node, RunMode, RunOptions};
