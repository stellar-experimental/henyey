//! stellar-core configuration compatibility layer.
//!
//! stellar-rpc generates a flat TOML configuration file with `SCREAMING_CASE`
//! keys for stellar-core:
//!
//! ```toml
//! NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
//! HTTP_PORT = 11626
//! DATABASE = "sqlite3:///tmp/stellar-core.db"
//! METADATA_OUTPUT_STREAM = "fd:3"
//! UNSAFE_QUORUM = true
//! NODE_SEED = "S..."
//! ```
//!
//! Henyey uses nested TOML with `snake_case`:
//!
//! ```toml
//! [network]
//! passphrase = "Test SDF Network ; September 2015"
//! [http]
//! port = 11626
//! ```
//!
//! This module auto-detects the format and translates stellar-core configs
//! into henyey's [`AppConfig`](crate::config::AppConfig).

use crate::config::{
    AppConfig, CompatHttpConfig, DatabaseConfig, HistoryArchiveEntry, HistoryConfig, HttpConfig,
};
use std::path::PathBuf;

/// Detect whether a TOML string is in stellar-core format.
///
/// Returns `true` if the top-level table contains at least one key that
/// matches a known stellar-core uppercase config key.
pub fn is_stellar_core_format(raw: &toml::Value) -> bool {
    let table = match raw.as_table() {
        Some(t) => t,
        None => return false,
    };

    // Check for well-known stellar-core top-level keys
    const STELLAR_CORE_KEYS: &[&str] = &[
        "NETWORK_PASSPHRASE",
        "HTTP_PORT",
        "DATABASE",
        "NODE_SEED",
        "NODE_IS_VALIDATOR",
        "METADATA_OUTPUT_STREAM",
        "UNSAFE_QUORUM",
        "PEER_PORT",
        "HTTP_QUERY_PORT",
        "BUCKET_DIR_PATH",
        "RUN_STANDALONE",
        "MANUAL_CLOSE",
        "ENABLE_SOROBAN_DIAGNOSTIC_EVENTS",
        "ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION",
        "EMIT_SOROBAN_TRANSACTION_META_EXT_V1",
        "EMIT_LEDGER_CLOSE_META_EXT_V1",
        "EMIT_CLASSIC_EVENTS",
        "CATCHUP_COMPLETE",
        "CATCHUP_RECENT",
        "FORCE_SCP",
        "KNOWN_PEERS",
        "PREFERRED_PEERS",
    ];

    table
        .keys()
        .any(|k| STELLAR_CORE_KEYS.contains(&k.as_str()))
}

/// Translate a stellar-core format TOML config into a henyey `AppConfig`.
///
/// The input must be a valid `toml::Value` that has been detected as
/// stellar-core format by [`is_stellar_core_format`].
pub fn translate_stellar_core_config(raw: &toml::Value) -> anyhow::Result<AppConfig> {
    let table = raw
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("Config must be a TOML table"))?;

    let mut config = AppConfig::default();

    // --- Node ---
    if let Some(seed) = get_str(table, "NODE_SEED") {
        config.node.node_seed = Some(seed);
    }
    if let Some(v) = get_bool(table, "NODE_IS_VALIDATOR") {
        config.node.is_validator = v;
    }
    if let Some(v) = get_bool(table, "MANUAL_CLOSE") {
        config.node.manual_close = v;
    }
    // NODE_HOME_DOMAIN
    if let Some(v) = get_str(table, "NODE_HOME_DOMAIN") {
        config.node.home_domain = Some(v);
    }

    // --- Network ---
    if let Some(passphrase) = get_str(table, "NETWORK_PASSPHRASE") {
        config.network.passphrase = passphrase;
    }

    // --- Database ---
    if let Some(db_str) = get_str(table, "DATABASE") {
        // stellar-core format: "sqlite3:///path/to/db"
        // Strip the sqlite3:// prefix to get the raw path.
        let path = if let Some(stripped) = db_str.strip_prefix("sqlite3://") {
            stripped.to_string()
        } else {
            db_str
        };
        config.database = DatabaseConfig {
            path: PathBuf::from(path),
            ..DatabaseConfig::default()
        };
    }

    // --- Buckets ---
    if let Some(dir) = get_str(table, "BUCKET_DIR_PATH") {
        config.buckets.directory = PathBuf::from(dir);
    }

    // --- HTTP ---
    if let Some(port) = get_u16(table, "HTTP_PORT") {
        // When stellar-core format is used, enable the compat HTTP server
        // on this port, since stellar-rpc expects stellar-core's wire format.
        // Disable the native HTTP server to avoid port conflict (both default
        // to the same port).
        config.http = HttpConfig {
            enabled: false,
            port,
            ..HttpConfig::default()
        };
        config.compat_http = CompatHttpConfig {
            enabled: true,
            port,
        };
    }

    // --- Query server ---
    if let Some(port) = get_u16(table, "HTTP_QUERY_PORT") {
        config.query.port = Some(port);
    }
    if let Some(v) = get_u32(table, "QUERY_SNAPSHOT_LEDGERS") {
        config.query.snapshot_ledgers = v;
    }
    if let Some(v) = get_usize(table, "QUERY_THREAD_POOL_SIZE") {
        config.query.thread_pool_size = v;
    }

    // --- Overlay ---
    if let Some(port) = get_u16(table, "PEER_PORT") {
        config.overlay.peer_port = port;
    }
    if let Some(peers) = get_string_array(table, "KNOWN_PEERS") {
        config.overlay.known_peers = peers;
    }
    if let Some(peers) = get_string_array(table, "PREFERRED_PEERS") {
        config.overlay.preferred_peers = peers;
    }

    // --- Metadata ---
    if let Some(stream) = get_str(table, "METADATA_OUTPUT_STREAM") {
        config.metadata.output_stream = Some(stream);
    }
    if let Some(v) = get_bool(table, "EMIT_SOROBAN_TRANSACTION_META_EXT_V1") {
        config.metadata.emit_soroban_tx_meta_ext_v1 = v;
    }
    if let Some(v) = get_bool(table, "EMIT_LEDGER_CLOSE_META_EXT_V1") {
        config.metadata.emit_ledger_close_meta_ext_v1 = v;
    }

    // --- Events ---
    if let Some(v) = get_bool(table, "EMIT_CLASSIC_EVENTS") {
        config.events.emit_classic_events = v;
    }

    // --- Diagnostics ---
    if let Some(v) = get_bool(table, "ENABLE_SOROBAN_DIAGNOSTIC_EVENTS") {
        config.diagnostics.soroban_diagnostic_events = v;
    }
    if let Some(v) = get_bool(table, "ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION") {
        config.diagnostics.tx_submission_diagnostics = v;
    }

    // --- Catchup ---
    if let Some(v) = get_bool(table, "CATCHUP_COMPLETE") {
        config.catchup.complete = v;
    }
    if let Some(v) = get_u32(table, "CATCHUP_RECENT") {
        config.catchup.recent = v;
    }

    // --- History archives ---
    // stellar-core format: [HISTORY.name] with get="cmd {0}" sub-tables
    if let Some(history_table) = table.get("HISTORY").and_then(|v| v.as_table()) {
        let mut archives = Vec::new();
        for (name, entry) in history_table {
            if let Some(entry_table) = entry.as_table() {
                let get_cmd = entry_table
                    .get("get")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                // Extract URL from curl command template:
                // "curl -sf https://example.com/{0} -o {1}" → "https://example.com"
                let url = get_cmd
                    .as_ref()
                    .and_then(|cmd| extract_url_from_curl_cmd(cmd))
                    .unwrap_or_default();

                let put_cmd = entry_table
                    .get("put")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let mkdir_cmd = entry_table
                    .get("mkdir")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                archives.push(HistoryArchiveEntry {
                    name: name.clone(),
                    url,
                    get_enabled: get_cmd.is_some(),
                    put_enabled: put_cmd.is_some(),
                    put: put_cmd,
                    mkdir: mkdir_cmd,
                });
            }
        }
        if !archives.is_empty() {
            config.history = HistoryConfig { archives };
        }
    }

    // --- Validators / quorum set ---
    // stellar-core uses [[VALIDATORS]] array-of-tables with NAME, PUBLIC_KEY, etc.
    // stellar-rpc typically generates these for captive-core configs.
    if let Some(validators) = table.get("VALIDATORS").and_then(|v| v.as_array()) {
        let mut validator_keys = Vec::new();
        for val in validators {
            if let Some(val_table) = val.as_table() {
                if let Some(key) = val_table.get("PUBLIC_KEY").and_then(|v| v.as_str()) {
                    validator_keys.push(key.to_string());
                }
                // Also extract inline HISTORY from validators
                if let Some(hist_cmd) = val_table.get("HISTORY").and_then(|v| v.as_str()) {
                    let name = val_table
                        .get("NAME")
                        .and_then(|v| v.as_str())
                        .unwrap_or("validator")
                        .to_string();
                    if let Some(url) = extract_url_from_curl_cmd(hist_cmd) {
                        config.history.archives.push(HistoryArchiveEntry {
                            name,
                            url,
                            get_enabled: true,
                            put_enabled: false,
                            put: None,
                            mkdir: None,
                        });
                    }
                }
            }
        }
        if !validator_keys.is_empty() {
            config.node.quorum_set.validators = validator_keys;
        }
    }

    // --- Ignored keys (accepted silently for compatibility) ---
    // UNSAFE_QUORUM, RUN_STANDALONE, FORCE_SCP, DISABLE_XDR_FSYNC,
    // ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING, etc.

    Ok(config)
}

/// Extract a base URL from a stellar-core curl command template.
///
/// Input:  `"curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"`
/// Output: `Some("https://history.stellar.org/prd/core-testnet/core_testnet_001")`
///
/// Also handles simpler forms like `"wget -q {0} -O {1}"` where no URL is present.
fn extract_url_from_curl_cmd(cmd: &str) -> Option<String> {
    // Look for an http/https URL in the command string
    for token in cmd.split_whitespace() {
        if token.starts_with("http://") || token.starts_with("https://") {
            // Strip the /{0} suffix that stellar-core appends for the remote path template
            let url = token
                .trim_end_matches("/{0}")
                .trim_end_matches("/{1}")
                .trim_end_matches("{0}")
                .trim_end_matches("{1}");
            return Some(url.to_string());
        }
    }
    None
}

// --- Helper functions for typed value extraction ---

fn get_str(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn get_bool(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<bool> {
    table.get(key).and_then(|v| v.as_bool())
}

fn get_u16(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<u16> {
    table
        .get(key)
        .and_then(|v| v.as_integer())
        .and_then(|i| u16::try_from(i).ok())
}

fn get_u32(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<u32> {
    table
        .get(key)
        .and_then(|v| v.as_integer())
        .and_then(|i| u32::try_from(i).ok())
}

fn get_usize(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<usize> {
    table
        .get(key)
        .and_then(|v| v.as_integer())
        .and_then(|i| usize::try_from(i).ok())
}

fn get_string_array(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<Vec<String>> {
    table.get(key).and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_stellar_core_format() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            "#,
        )
        .unwrap();
        assert!(is_stellar_core_format(&core_toml));
    }

    #[test]
    fn test_detect_henyey_format() {
        let henyey_toml: toml::Value = toml::from_str(
            r#"
            [network]
            passphrase = "Test SDF Network ; September 2015"
            [http]
            port = 11626
            "#,
        )
        .unwrap();
        assert!(!is_stellar_core_format(&henyey_toml));
    }

    #[test]
    fn test_translate_basic_config() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            HTTP_QUERY_PORT = 11627
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            BUCKET_DIR_PATH = "/tmp/buckets"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            METADATA_OUTPUT_STREAM = "fd:3"
            UNSAFE_QUORUM = true
            ENABLE_SOROBAN_DIAGNOSTIC_EVENTS = true
            ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION = true
            EMIT_SOROBAN_TRANSACTION_META_EXT_V1 = true
            EMIT_LEDGER_CLOSE_META_EXT_V1 = true
            EMIT_CLASSIC_EVENTS = true
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();

        assert_eq!(
            config.network.passphrase,
            "Test SDF Network ; September 2015"
        );
        assert_eq!(config.http.port, 11626);
        assert_eq!(config.query.port, Some(11627));
        assert_eq!(config.database.path, PathBuf::from("/tmp/stellar-core.db"));
        assert_eq!(config.buckets.directory, PathBuf::from("/tmp/buckets"));
        assert_eq!(
            config.node.node_seed.as_deref(),
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH")
        );
        assert!(!config.node.is_validator);
        assert_eq!(config.metadata.output_stream.as_deref(), Some("fd:3"));
        assert!(config.diagnostics.soroban_diagnostic_events);
        assert!(config.diagnostics.tx_submission_diagnostics);
        assert!(config.metadata.emit_soroban_tx_meta_ext_v1);
        assert!(config.metadata.emit_ledger_close_meta_ext_v1);
        assert!(config.events.emit_classic_events);

        // Compat HTTP should be auto-enabled on HTTP_PORT
        assert!(config.compat_http.enabled);
        assert_eq!(config.compat_http.port, 11626);

        // Native HTTP should be disabled to avoid port conflict
        assert!(!config.http.enabled);
    }

    #[test]
    fn test_database_prefix_stripping() {
        let core_toml: toml::Value =
            toml::from_str(r#"DATABASE = "sqlite3:///var/lib/stellar/stellar.db""#).unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(
            config.database.path,
            PathBuf::from("/var/lib/stellar/stellar.db")
        );
    }

    #[test]
    fn test_history_archive_translation() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [HISTORY.sdf1]
            get = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"
            [HISTORY.sdf2]
            get = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_002/{0} -o {1}"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();

        assert_eq!(config.history.archives.len(), 2);
        assert_eq!(config.history.archives[0].name, "sdf1");
        assert_eq!(
            config.history.archives[0].url,
            "https://history.stellar.org/prd/core-testnet/core_testnet_001"
        );
        assert!(config.history.archives[0].get_enabled);
        assert!(!config.history.archives[0].put_enabled);
    }

    #[test]
    fn test_extract_url_from_curl_cmd() {
        assert_eq!(
            extract_url_from_curl_cmd(
                "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"
            ),
            Some("https://history.stellar.org/prd/core-testnet/core_testnet_001".to_string())
        );

        assert_eq!(
            extract_url_from_curl_cmd("curl http://example.com/{0} -o {1}"),
            Some("http://example.com".to_string())
        );

        // No URL in command
        assert_eq!(extract_url_from_curl_cmd("cp /local/{0} /dest/{1}"), None);
    }

    #[test]
    fn test_validators_with_inline_history() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [[VALIDATORS]]
            NAME = "sdftest1"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
            HISTORY = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"

            [[VALIDATORS]]
            NAME = "sdftest2"
            PUBLIC_KEY = "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP"
            HISTORY = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_002/{0} -o {1}"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();

        // Validators from [[VALIDATORS]] should be present in quorum set
        assert!(config
            .node
            .quorum_set
            .validators
            .contains(&"GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y".to_string()));
        assert!(config
            .node
            .quorum_set
            .validators
            .contains(&"GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP".to_string()));

        // Inline HISTORY should be extracted as archives
        let archive_names: Vec<&str> = config
            .history
            .archives
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert!(archive_names.contains(&"sdftest1"));
        assert!(archive_names.contains(&"sdftest2"));

        let sdftest1 = config
            .history
            .archives
            .iter()
            .find(|a| a.name == "sdftest1")
            .unwrap();
        assert_eq!(
            sdftest1.url,
            "https://history.stellar.org/prd/core-testnet/core_testnet_001"
        );
    }

    #[test]
    fn test_known_peers() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            KNOWN_PEERS = ["core1.stellar.org:11625", "core2.stellar.org:11625"]
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.overlay.known_peers.len(), 2);
        assert_eq!(config.overlay.known_peers[0], "core1.stellar.org:11625");
    }
}
