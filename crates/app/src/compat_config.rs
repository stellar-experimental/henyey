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
        "PREFERRED_UPGRADE_PROTOCOL_VERSION",
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

    let mut config = AppConfig {
        is_compat_config: true,
        ..AppConfig::default()
    };

    // Clear defaults that should come from the stellar-core config, not from
    // henyey's testnet preset. These will be repopulated from the config below.
    config.overlay.known_peers.clear();
    config.history.archives.clear();
    config.node.quorum_set.validators.clear();

    // --- Node ---
    if let Some(seed) = get_str(table, "NODE_SEED") {
        // stellar-core allows "SEED name" format (e.g., "S... self") — strip the name suffix
        let seed = seed.split_whitespace().next().unwrap_or(&seed).to_string();
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
        // When PUBLIC_HTTP_PORT=true, bind to all interfaces (dual-stack) so that
        // clients connecting via IPv4 or IPv6 localhost can reach the server.
        // This matches stellar-core's behavior where PUBLIC_HTTP_PORT controls
        // whether the HTTP port is accessible beyond localhost.
        let address = if get_bool(table, "PUBLIC_HTTP_PORT").unwrap_or(false) {
            "::".to_string()
        } else {
            "127.0.0.1".to_string()
        };
        config.compat_http = CompatHttpConfig {
            enabled: true,
            port,
            address,
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

    // --- Upgrades ---
    if let Some(v) = get_u32(table, "PREFERRED_UPGRADE_PROTOCOL_VERSION") {
        config.upgrades.protocol_version = Some(v);
    }

    // --- Testing ---
    if let Some(v) = get_bool(table, "ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING") {
        config.testing.accelerate_time = v;
    }

    // --- Catchup ---
    if let Some(v) = get_bool(table, "CATCHUP_COMPLETE") {
        config.catchup.complete = v;
    }
    if let Some(v) = get_u32(table, "CATCHUP_RECENT") {
        config.catchup.recent = v;
    }

    // --- Maintenance ---
    if let Some(v) = get_u32(table, "AUTOMATIC_MAINTENANCE_PERIOD") {
        config.maintenance.period_secs = v as u64;
        if v == 0 {
            config.maintenance.enabled = false;
        }
    }
    if let Some(v) = get_u32(table, "AUTOMATIC_MAINTENANCE_COUNT") {
        config.maintenance.count = v;
        if v == 0 {
            config.maintenance.enabled = false;
        }
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
        let mut validator_addresses = Vec::new();
        for (i, val) in validators.iter().enumerate() {
            if let Some(val_table) = val.as_table() {
                let key = val_table
                    .get("PUBLIC_KEY")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("[[VALIDATORS]] entry {} missing or invalid PUBLIC_KEY", i)
                    })?;
                validator_keys.push(key.to_string());
                // Extract ADDRESS for peer discovery (e.g., "core-testnet1.stellar.org")
                if let Some(addr) = val_table.get("ADDRESS").and_then(|v| v.as_str()) {
                    let peer_addr = if addr.contains(':') {
                        addr.to_string()
                    } else {
                        // Default Stellar peer port
                        format!("{addr}:11625")
                    };
                    validator_addresses.push(peer_addr);
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
        // Use validator addresses as known peers if no explicit KNOWN_PEERS was set
        if config.overlay.known_peers.is_empty() && !validator_addresses.is_empty() {
            config.overlay.known_peers = validator_addresses;
        }
    }

    // --- Old-style [QUORUM_SET] (used by quickstart local mode) ---
    // stellar-core format:
    //   [QUORUM_SET]
    //   THRESHOLD_PERCENT=100
    //   VALIDATORS=["$self"]
    if let Some(qs_table) = table.get("QUORUM_SET").and_then(|v| v.as_table()) {
        if let Some(validators) = qs_table.get("VALIDATORS").and_then(|v| v.as_array()) {
            let mut keys: Vec<String> = Vec::new();
            for v in validators {
                if let Some(s) = v.as_str() {
                    if s == "$self" {
                        // "$self" refers to the node's own key — resolve it from NODE_SEED
                        let seed_str = config.node.node_seed.as_ref().ok_or_else(|| {
                            anyhow::anyhow!(
                                "Cannot resolve $self in [QUORUM_SET]: NODE_SEED not set"
                            )
                        })?;
                        let secret =
                            henyey_crypto::SecretKey::from_strkey(seed_str).map_err(|e| {
                                anyhow::anyhow!(
                                    "Cannot resolve $self in [QUORUM_SET]: invalid NODE_SEED: {}",
                                    e
                                )
                            })?;
                        keys.push(secret.public_key().to_strkey());
                    } else {
                        keys.push(s.to_string());
                    }
                }
            }
            // Only override if the [[VALIDATORS]] section didn't already set the quorum set
            if !keys.is_empty() && config.node.quorum_set.validators.is_empty() {
                config.node.quorum_set.validators = keys;
            }
        }
        // Parse THRESHOLD_PERCENT and apply it to the quorum set config.
        // stellar-core default is 67 if not specified.
        if let Some(tp) = qs_table
            .get("THRESHOLD_PERCENT")
            .and_then(|v| v.as_integer())
        {
            if (1..=100).contains(&tp) {
                config.node.quorum_set.threshold_percent = tp as u32;
            } else {
                tracing::warn!(
                    threshold_percent = tp,
                    "THRESHOLD_PERCENT must be between 1 and 100, using default"
                );
            }
        }
    }

    // --- Testing keys ---
    if let Some(val) = table.get("ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING") {
        if let Some(b) = val.as_bool() {
            config.testing.generate_load_for_testing = b;
        } else if let Some(s) = val.as_str() {
            config.testing.generate_load_for_testing = s.eq_ignore_ascii_case("true");
        }
    }
    if let Some(v) = get_u32(table, "GENESIS_TEST_ACCOUNT_COUNT") {
        config.testing.genesis_test_account_count = v;
    }
    if let Some(v) = get_bool(table, "RUN_STANDALONE") {
        config.testing.run_standalone = v;
    }

    // --- Ignored keys (accepted silently for compatibility) ---
    // UNSAFE_QUORUM, FORCE_SCP, DISABLE_XDR_FSYNC, etc.

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
        // Without PUBLIC_HTTP_PORT, should bind to localhost only
        assert_eq!(config.compat_http.address, "127.0.0.1");

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

    #[test]
    fn test_validator_address_as_known_peers() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [[VALIDATORS]]
            NAME = "sdftest1"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
            ADDRESS = "core-testnet1.stellar.org"
            HISTORY = "curl -sf http://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"

            [[VALIDATORS]]
            NAME = "sdftest2"
            PUBLIC_KEY = "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP"
            ADDRESS = "core-testnet2.stellar.org:11625"
            HISTORY = "curl -sf http://history.stellar.org/prd/core-testnet/core_testnet_002/{0} -o {1}"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        // ADDRESS fields should be extracted as known_peers (with default port appended if missing)
        assert_eq!(config.overlay.known_peers.len(), 2);
        assert_eq!(
            config.overlay.known_peers[0],
            "core-testnet1.stellar.org:11625"
        );
        assert_eq!(
            config.overlay.known_peers[1],
            "core-testnet2.stellar.org:11625"
        );
    }

    #[test]
    fn test_known_peers_not_overridden_by_validator_address() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            KNOWN_PEERS = ["explicit-peer.stellar.org:11625"]
            [[VALIDATORS]]
            NAME = "sdftest1"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
            ADDRESS = "core-testnet1.stellar.org"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        // Explicit KNOWN_PEERS should take precedence over validator ADDRESS
        assert_eq!(config.overlay.known_peers.len(), 1);
        assert_eq!(
            config.overlay.known_peers[0],
            "explicit-peer.stellar.org:11625"
        );
    }

    #[test]
    fn test_old_style_quorum_set() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["$self"]
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.node.is_validator);
        // "$self" should be resolved to the node's own public key from NODE_SEED
        assert_eq!(config.node.quorum_set.validators.len(), 1);
        let expected_pubkey = henyey_crypto::SecretKey::from_strkey(
            "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2",
        )
        .unwrap()
        .public_key()
        .to_strkey();
        assert_eq!(config.node.quorum_set.validators[0], expected_pubkey);
        // THRESHOLD_PERCENT=100 should be applied (not silently dropped to default 67)
        assert_eq!(config.node.quorum_set.threshold_percent, 100);
    }

    #[test]
    fn test_quorum_set_self_without_node_seed_fails() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["$self"]
            "#,
        )
        .unwrap();

        let result = translate_stellar_core_config(&core_toml);
        assert!(result.is_err(), "Should fail when $self cannot be resolved");
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("NODE_SEED not set"));
    }

    #[test]
    fn test_validators_missing_public_key_fails() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            [[VALIDATORS]]
            NAME = "test"
            ADDRESS = "core-testnet1.stellar.org"
            "#,
        )
        .unwrap();

        let result = translate_stellar_core_config(&core_toml);
        assert!(result.is_err(), "Should fail when PUBLIC_KEY is missing");
        assert!(result.unwrap_err().to_string().contains("PUBLIC_KEY"));
    }

    #[test]
    fn test_old_style_quorum_set_threshold_percent() {
        // With THRESHOLD_PERCENT=100 and 1 validator, both 100% and 67% produce threshold=1.
        // But the config value itself must be 100, not the default 67.
        // Use a 2-validator setup with THRESHOLD_PERCENT=100 to make the threshold observable:
        // - 100%: threshold = (2*100)/100 = 2
        // - 67% (default): threshold = (2*67)/100 = 1 — WRONG
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let core_toml: toml::Value = toml::from_str(&format!(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["$self", "{}"]
            "#,
            key2
        ))
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.node.quorum_set.threshold_percent, 100);
        assert_eq!(config.node.quorum_set.validators.len(), 2);

        // Verify to_xdr produces correct threshold
        let xdr_qs = config.node.quorum_set.to_xdr().unwrap();
        // 100% of 2 validators = threshold 2
        assert_eq!(xdr_qs.threshold, 2);
    }

    #[test]
    fn test_quorum_threshold_ceiling_division() {
        // Verify threshold uses ceiling division matching stellar-core:
        //   1 + ((total * percent - 1) / 100)
        // With 3 validators and 51%: ceil(3*0.51) = 2
        // Floor division would give: (3*51)/100 = 1 — WRONG
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();

        let qs = crate::config::QuorumSetConfig {
            threshold_percent: 51,
            validators: vec![key1, key2, key3],
            inner_sets: vec![],
        };
        let xdr = qs.to_xdr().unwrap();
        assert_eq!(xdr.threshold, 2, "ceil(3 * 51 / 100) should be 2, not 1");

        // Also verify 67% with 3 validators: ceil(2.01) = 3
        // stellar-core: 1 + (3*67-1)/100 = 1 + 200/100 = 1 + 2 = 3
        let qs67 = crate::config::QuorumSetConfig {
            threshold_percent: 67,
            validators: qs.validators.clone(),
            inner_sets: vec![],
        };
        let xdr67 = qs67.to_xdr().unwrap();
        assert_eq!(xdr67.threshold, 3, "ceil(3 * 67 / 100) should be 3, not 2");
    }

    #[test]
    fn test_preferred_upgrade_protocol_version() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            PREFERRED_UPGRADE_PROTOCOL_VERSION = 25
            "#,
        )
        .unwrap();

        assert!(is_stellar_core_format(&core_toml));
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.upgrades.protocol_version, Some(25));
    }

    #[test]
    fn test_preferred_upgrade_protocol_version_absent() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.upgrades.protocol_version, None);
    }

    #[test]
    fn test_unknown_config_keys_silently_ignored() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING = true
            EXPERIMENTAL_BUCKETLIST_DB = true
            PUBLIC_HTTP_PORT = true
            COMMANDS = ["ll?level=debug"]
            BACKFILL_STELLAR_ASSET_EVENTS = true
            BUCKETLIST_DB_MEMORY_FOR_CACHING = 0
            "#,
        )
        .unwrap();

        // Should not error on unknown keys
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.compat_http.port, 11626);
        // PUBLIC_HTTP_PORT=true should bind to all interfaces (dual-stack)
        assert_eq!(config.compat_http.address, "::");
        // ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING should now be parsed
        assert!(config.testing.accelerate_time);
    }

    #[test]
    fn test_generate_load_for_testing_bool() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.testing.generate_load_for_testing);
    }

    #[test]
    fn test_generate_load_for_testing_string() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING = "true"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.testing.generate_load_for_testing);
    }

    #[test]
    fn test_generate_load_for_testing_false() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING = false
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.testing.generate_load_for_testing);
    }

    #[test]
    fn test_generate_load_for_testing_absent() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.testing.generate_load_for_testing);
    }

    #[test]
    fn test_genesis_test_account_count() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            GENESIS_TEST_ACCOUNT_COUNT = 100
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.testing.genesis_test_account_count, 100);
    }

    #[test]
    fn test_genesis_test_account_count_default() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.testing.genesis_test_account_count, 0);
    }

    #[test]
    fn test_maintenance_config_translation() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            AUTOMATIC_MAINTENANCE_PERIOD = 3600
            AUTOMATIC_MAINTENANCE_COUNT = 25000
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 3600);
        assert_eq!(config.maintenance.count, 25000);
    }

    #[test]
    fn test_maintenance_config_disabled_by_zero_period() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            AUTOMATIC_MAINTENANCE_PERIOD = 0
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.maintenance.enabled);
    }

    #[test]
    fn test_maintenance_config_disabled_by_zero_count() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            AUTOMATIC_MAINTENANCE_COUNT = 0
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.maintenance.enabled);
    }

    #[test]
    fn test_maintenance_config_defaults_when_absent() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        // Should get defaults when not specified in compat config
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 4 * 60 * 60);
        assert_eq!(config.maintenance.count, 50_000);
    }

    #[test]
    fn test_local_history_archive_with_cp_commands() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            [HISTORY.vs]
            get = "cp /opt/stellar/history-archive/data/{0} {1}"
            put = "cp {0} /opt/stellar/history-archive/data/{1}"
            mkdir = "mkdir -p /opt/stellar/history-archive/data/{0}"
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.history.archives.len(), 1);
        let archive = &config.history.archives[0];
        assert_eq!(archive.name, "vs");
        assert!(archive.get_enabled);
        assert!(archive.put_enabled);
        assert!(archive.put.is_some());
        assert!(archive.mkdir.is_some());
        assert_eq!(
            archive.put.as_deref().unwrap(),
            "cp {0} /opt/stellar/history-archive/data/{1}"
        );
    }

    /// End-to-end test: parse a realistic Supercluster (SSC) generated config.
    ///
    /// This fixture represents the full config that SSC's Kubernetes mission
    /// controller generates for a watcher node in a 3-validator testnet cluster
    /// with load generation enabled and metadata streaming to stellar-rpc.
    ///
    /// The config includes keys that henyey parses AND keys that are silently
    /// ignored (EXPERIMENTAL_BUCKETLIST_DB, COMMANDS, etc.). The test verifies
    /// that the translator produces a correct `AppConfig` without errors.
    #[test]
    fn test_ssc_generated_config_full_parse() {
        let fixture = include_str!("compat_http/test_fixtures/ssc_generated_config.cfg");
        let raw: toml::Value = toml::from_str(fixture).unwrap();

        // Must be detected as stellar-core format
        assert!(
            is_stellar_core_format(&raw),
            "SSC config must be detected as stellar-core format"
        );

        // Must translate without error
        let config = translate_stellar_core_config(&raw).unwrap();

        // --- Network ---
        assert_eq!(
            config.network.passphrase,
            "Test SDF Network ; September 2015"
        );

        // --- HTTP / Compat ---
        assert!(config.compat_http.enabled);
        assert_eq!(config.compat_http.port, 11626);
        // PUBLIC_HTTP_PORT=true → bind to dual-stack wildcard
        assert_eq!(config.compat_http.address, "::");
        assert!(!config.http.enabled); // native HTTP disabled when compat is on

        // --- Overlay ---
        assert_eq!(config.overlay.peer_port, 11625);
        assert_eq!(config.overlay.known_peers.len(), 3);
        assert!(config
            .overlay
            .known_peers
            .contains(&"core-testnet1.stellar.org:11625".to_string()));
        assert_eq!(config.overlay.preferred_peers.len(), 1);
        assert_eq!(
            config.overlay.preferred_peers[0],
            "core-testnet1.stellar.org:11625"
        );

        // --- Database ---
        assert_eq!(
            config.database.path,
            PathBuf::from("/opt/stellar/stellar-core.db")
        );

        // --- Buckets ---
        assert_eq!(
            config.buckets.directory,
            PathBuf::from("/opt/stellar/buckets")
        );

        // --- Node ---
        assert_eq!(
            config.node.node_seed.as_deref(),
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH")
        );
        assert!(!config.node.is_validator);
        assert_eq!(
            config.node.home_domain.as_deref(),
            Some("testnet.stellar.org")
        );

        // --- Metadata ---
        assert_eq!(config.metadata.output_stream.as_deref(), Some("fd:3"));
        assert!(config.metadata.emit_soroban_tx_meta_ext_v1);
        assert!(config.metadata.emit_ledger_close_meta_ext_v1);

        // --- Events ---
        assert!(config.events.emit_classic_events);

        // --- Diagnostics ---
        assert!(config.diagnostics.soroban_diagnostic_events);
        assert!(config.diagnostics.tx_submission_diagnostics);

        // --- Catchup ---
        assert!(!config.catchup.complete);
        assert_eq!(config.catchup.recent, 1024);

        // --- Testing ---
        assert!(config.testing.generate_load_for_testing);
        assert!(!config.testing.accelerate_time);

        // --- Maintenance ---
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 3600);
        assert_eq!(config.maintenance.count, 50000);

        // --- Validators → quorum set ---
        assert_eq!(config.node.quorum_set.validators.len(), 3);
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
        assert!(config
            .node
            .quorum_set
            .validators
            .contains(&"GC2V2EFSXN6SQTWVYA5EPJPBWWIMSD2XQNKUOHGEKB535AQE2I6IXV2Z".to_string()));

        // --- History archives ---
        // Should have archives from both [[VALIDATORS]].HISTORY and [HISTORY.name] sections.
        // [[VALIDATORS]] inline HISTORY produces 3 archives, [HISTORY.*] produces 2.
        // Total unique: 5 (3 from validators + 2 from top-level HISTORY).
        assert!(
            config.history.archives.len() >= 5,
            "expected at least 5 history archives, got {}",
            config.history.archives.len()
        );

        // Verify at least one from [HISTORY.sdf1]
        let sdf1 = config.history.archives.iter().find(|a| a.name == "sdf1");
        assert!(sdf1.is_some(), "should have archive from [HISTORY.sdf1]");
        assert_eq!(
            sdf1.unwrap().url,
            "https://history.stellar.org/prd/core-testnet/core_testnet_001"
        );

        // Verify the compat flag is set
        assert!(config.is_compat_config);
    }

    /// Verify that the existing captive-core-testnet.cfg also parses correctly.
    #[test]
    fn test_captive_core_testnet_cfg_parse() {
        let fixture = include_str!("../../../configs/captive-core-testnet.cfg");
        let raw: toml::Value = toml::from_str(fixture).unwrap();

        // This config only has [[HOME_DOMAINS]] and [[VALIDATORS]].
        // HOME_DOMAINS has QUALITY which is not a recognized stellar-core key,
        // but the VALIDATORS section triggers detection.
        // Actually, this config has no flat stellar-core keys like NETWORK_PASSPHRASE.
        // It is a supplementary config used by stellar-rpc alongside injected keys.
        // Let's verify it parses as TOML at minimum.
        let has_validators = raw
            .as_table()
            .map(|t| t.contains_key("VALIDATORS"))
            .unwrap_or(false);
        assert!(has_validators, "fixture should have VALIDATORS section");
    }

    #[test]
    fn test_compat_run_standalone_parsed() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
RUN_STANDALONE=true
NODE_IS_VALIDATOR=true
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
HTTP_QUERY_PORT=11627

[HISTORY.local]
get="curl -sf http://localhost:1570/{0} -o {1}"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert!(config.testing.run_standalone);
        assert!(config.node.is_validator);
        assert_eq!(config.query.port, Some(11627));
        // Verify is_networked_validator returns false for standalone validators.
        assert!(
            !config.is_networked_validator(),
            "Standalone validator should not be treated as networked"
        );
    }
}
