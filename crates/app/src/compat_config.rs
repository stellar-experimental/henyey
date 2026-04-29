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
    AppConfig, CompatHttpConfig, CompatQuorumSafety, DatabaseConfig, FailureSafety,
    HistoryArchiveEntry, HistoryConfig, HttpConfig, ValidationThresholdLevel,
};
use henyey_herder::{ValidatorEntryInfo, ValidatorQuality, ValidatorWeightConfig};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Top-level keys that `translate_stellar_core_config` actively translates.
const SUPPORTED_KEYS: &[&str] = &[
    "NODE_SEED",
    "NODE_IS_VALIDATOR",
    "MANUAL_CLOSE",
    "NODE_HOME_DOMAIN",
    "NETWORK_PASSPHRASE",
    "DATABASE",
    "BUCKET_DIR_PATH",
    "HTTP_PORT",
    "PUBLIC_HTTP_PORT",
    "HTTP_QUERY_PORT",
    "QUERY_SNAPSHOT_LEDGERS",
    "QUERY_THREAD_POOL_SIZE",
    "PEER_PORT",
    "KNOWN_PEERS",
    "PREFERRED_PEERS",
    "PREFERRED_PEER_KEYS",
    "PREFERRED_PEERS_ONLY",
    "METADATA_OUTPUT_STREAM",
    "EMIT_SOROBAN_TRANSACTION_META_EXT_V1",
    "EMIT_LEDGER_CLOSE_META_EXT_V1",
    "EMIT_CLASSIC_EVENTS",
    "ENABLE_SOROBAN_DIAGNOSTIC_EVENTS",
    "ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION",
    "PREFERRED_UPGRADE_PROTOCOL_VERSION",
    "ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING",
    "CATCHUP_COMPLETE",
    "CATCHUP_RECENT",
    "AUTOMATIC_MAINTENANCE_PERIOD",
    "AUTOMATIC_MAINTENANCE_COUNT",
    "ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING",
    "GENESIS_TEST_ACCOUNT_COUNT",
    "RUN_STANDALONE",
    // Sub-tables (handled structurally)
    "HISTORY",
    "VALIDATORS",
    "QUORUM_SET",
    "HOME_DOMAINS",
    "FORCE_OLD_STYLE_LEADER_ELECTION",
    "FLOOD_ARB_TX_BASE_ALLOWANCE",
    "FLOOD_ARB_TX_DAMPING_FACTOR",
    "FLOOD_TX_PERIOD_MS",
    "FLOOD_ADVERT_PERIOD_MS",
    "FAILURE_SAFETY",
    "UNSAFE_QUORUM",
    "SURVEYOR_KEYS",
    "PEER_FLOOD_READING_CAPACITY_BYTES",
    "FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES",
];

/// Valid stellar-core keys that henyey intentionally does not support.
/// These are logged at `info` level rather than `warn`.
const UNSUPPORTED_KNOWN_KEYS: &[&str] = &[
    "FORCE_SCP",
    "DISABLE_XDR_FSYNC",
    "COMMANDS",
    "EXPERIMENTAL_BUCKETLIST_DB",
    "EXPERIMENTAL_BUCKETLIST_DB_INDEX_PAGE_SIZE_EXPONENT",
    "EXPERIMENTAL_BUCKETLIST_DB_INDEX_CUTOFF",
    "TARGET_PEER_CONNECTIONS",
    "MAX_ADDITIONAL_PEER_CONNECTIONS",
    "MAX_PENDING_CONNECTIONS",
    "PEER_AUTHENTICATION_TIMEOUT",
    "PEER_TIMEOUT",
    "MINIMUM_IDLE_PERCENT",
    "WORKER_THREADS",
    "MAX_CONCURRENT_SUBPROCESSES",
    "LOG_FILE_PATH",
    "BUCKETLIST_DB_MEMORY_FOR_CACHING",
    "BACKFILL_STELLAR_ASSET_EVENTS",
];

/// Recognized keys within `[[VALIDATORS]]` entries.
const VALIDATOR_SUPPORTED_KEYS: &[&str] = &[
    "NAME",
    "PUBLIC_KEY",
    "ADDRESS",
    "HISTORY",
    "HOME_DOMAIN",
    "QUALITY",
];
const VALIDATOR_UNSUPPORTED_KEYS: &[&str] = &[];

/// Recognized keys within `[QUORUM_SET]`.
const QUORUM_SET_KEYS: &[&str] = &["THRESHOLD_PERCENT", "VALIDATORS"];

/// Recognized keys within `[HISTORY.*]` entries.
const HISTORY_ENTRY_KEYS: &[&str] = &["get", "put", "mkdir"];

/// Detect whether a TOML string is in stellar-core format.
///
/// Returns `true` if the top-level table contains at least one key that
/// matches a known stellar-core uppercase config key (supported or
/// unsupported-but-known).
pub fn is_stellar_core_format(raw: &toml::Value) -> bool {
    let table = match raw.as_table() {
        Some(t) => t,
        None => return false,
    };

    table.keys().any(|k| {
        let key = k.as_str();
        SUPPORTED_KEYS.contains(&key) || UNSUPPORTED_KNOWN_KEYS.contains(&key)
    })
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
    if let Some(v) = get_bool(table, "FORCE_OLD_STYLE_LEADER_ELECTION") {
        config.node.force_old_style_leader_election = v;
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
    // PREFERRED_PEER_KEYS: security-sensitive — strict parsing to avoid
    // silently dropping entries under PREFERRED_PEERS_ONLY mode.
    if let Some(keys) = get_string_array_strict(table, "PREFERRED_PEER_KEYS")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        config.overlay.preferred_peer_keys = keys;
    }
    if let Some(v) = get_bool_strict(table, "PREFERRED_PEERS_ONLY")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        config.overlay.preferred_peers_only = v;
    }
    if let Some(v) = get_i64(table, "FLOOD_ARB_TX_BASE_ALLOWANCE") {
        match i32::try_from(v) {
            Ok(i) => config.overlay.flood_arb_tx_base_allowance = i,
            Err(_) => {
                tracing::warn!(
                    key = "FLOOD_ARB_TX_BASE_ALLOWANCE",
                    value = v,
                    "Compat config key value overflows i32 range"
                );
            }
        }
    }
    if let Some(v) = get_f64(table, "FLOOD_ARB_TX_DAMPING_FACTOR") {
        config.overlay.flood_arb_tx_damping_factor = v;
    }
    if let Some(v) = get_i64(table, "FLOOD_TX_PERIOD_MS") {
        if v >= 1 {
            config.overlay.flood_tx_period_ms = v as u64;
        } else {
            tracing::warn!(
                key = "FLOOD_TX_PERIOD_MS",
                value = v,
                "Compat config key value must be >= 1"
            );
        }
    }
    if let Some(v) = get_i64(table, "FLOOD_ADVERT_PERIOD_MS") {
        if v >= 1 {
            config.overlay.flood_advert_period_ms = v as u64;
        } else {
            tracing::warn!(
                key = "FLOOD_ADVERT_PERIOD_MS",
                value = v,
                "Compat config key value must be >= 1"
            );
        }
    }
    // SURVEYOR_KEYS: security-sensitive — strict validation instead of
    // get_string_array (which silently drops non-string elements and could
    // leave the list empty, widening survey access to the full quorum).
    if let Some(keys) = get_string_array_strict(table, "SURVEYOR_KEYS")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        config.overlay.surveyor_keys = keys;
    }
    if let Some(v) = get_u32(table, "PEER_FLOOD_READING_CAPACITY_BYTES") {
        config.overlay.peer_flood_reading_capacity_bytes = v;
    }
    if let Some(v) = get_u32(table, "FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES") {
        config.overlay.flow_control_send_more_batch_size_bytes = v;
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

    // Parse [[HOME_DOMAINS]] first — validators may reference these for quality.
    let domain_quality_map = parse_home_domains(table)?;

    // Track validator metadata for building ValidatorWeightConfig later.
    let mut validator_entries: Vec<(String, String, Option<String>, Option<String>)> = Vec::new(); // (pubkey, name, home_domain, quality)
    let has_manual_quorum_set = table.contains_key("QUORUM_SET");
    let has_validators_section = table
        .get("VALIDATORS")
        .and_then(|v| v.as_array())
        .map_or(false, |a| !a.is_empty());

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
                let name = val_table
                    .get("NAME")
                    .and_then(|v| v.as_str())
                    .unwrap_or("validator")
                    .to_string();
                let home_domain = val_table
                    .get("HOME_DOMAIN")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let quality_str = val_table
                    .get("QUALITY")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                validator_keys.push(key.to_string());
                validator_entries.push((key.to_string(), name.clone(), home_domain, quality_str));

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

    // Build ValidatorWeightConfig when on the auto-generated quorum set path
    // (not manual [QUORUM_SET]) and validators have quality/home_domain data.
    // Matches stellar-core: setValidatorWeightConfig is only called on the
    // auto-generated qset path (Config.cpp:2110).
    if config.node.is_validator && !validator_entries.is_empty() && !has_manual_quorum_set {
        config.validator_weight_config =
            build_validator_weight_config(&config, &validator_entries, &domain_quality_map)?;
    }

    // --- Old-style [QUORUM_SET] (used by quickstart local mode) ---
    // stellar-core format:
    //   [QUORUM_SET]
    //   THRESHOLD_PERCENT=100
    //   VALIDATORS=["$self"]

    // Snapshot auto-generated quorum set for UNSAFE_QUORUM override comparison.
    let auto_generated_qset = if has_validators_section {
        Some(config.node.quorum_set.clone())
    } else {
        None
    };

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
            // [QUORUM_SET] always overrides [[VALIDATORS]]-generated quorum set.
            if !keys.is_empty() {
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
    // FORCE_SCP, DISABLE_XDR_FSYNC, etc.

    // --- FAILURE_SAFETY and UNSAFE_QUORUM ---
    // Parse these quorum safety knobs from stellar-core compat configs.
    let unsafe_quorum = match table.get("UNSAFE_QUORUM") {
        Some(v) => v
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("UNSAFE_QUORUM must be a boolean, got: {}", v))?,
        None => false,
    };

    let failure_safety = match table.get("FAILURE_SAFETY") {
        Some(v) => {
            let n = v
                .as_integer()
                .ok_or_else(|| anyhow::anyhow!("FAILURE_SAFETY must be an integer, got: {}", v))?;
            if n < -1 || n > i32::MAX as i64 - 1 {
                anyhow::bail!(
                    "FAILURE_SAFETY must be between -1 and {}, got: {}",
                    i32::MAX - 1,
                    n
                );
            }
            if n == -1 {
                FailureSafety::Auto
            } else {
                FailureSafety::Explicit(n as i32)
            }
        }
        None => FailureSafety::Auto,
    };

    // Gate manual [QUORUM_SET] override on UNSAFE_QUORUM when [[VALIDATORS]]
    // were also present. stellar-core rejects overrides that differ from the
    // auto-generated set unless UNSAFE_QUORUM=true (Config.cpp:2087-2099).
    if has_validators_section && has_manual_quorum_set {
        if let Some(ref auto_qset) = auto_generated_qset {
            // Compare in declaration order (no sorting). stellar-core compares
            // serialized qset strings which are order-sensitive
            // (Config.cpp:2087-2099).
            let sets_differ = auto_qset.validators != config.node.quorum_set.validators
                || auto_qset.threshold_percent != config.node.quorum_set.threshold_percent;

            if sets_differ && !unsafe_quorum {
                anyhow::bail!(
                    "Can't override [[VALIDATORS]] with QUORUM_SET unless you also set \
                     UNSAFE_QUORUM=true. Be sure you know what you are doing!"
                );
            }
        }
    }

    // Determine threshold validation level.
    let threshold_level = if has_manual_quorum_set {
        // Manual [QUORUM_SET] always uses BFT (assumes validators are from
        // different entities). Matches stellar-core Config.cpp:2101-2103.
        ValidationThresholdLevel::ByzantineFaultTolerance
    } else {
        // Auto-generated from [[VALIDATORS]]: use BFT if >1 unique home domain.
        // Validators without HOME_DOMAIN count as empty-string domain, matching
        // stellar-core's mHomeDomain behavior (Config.cpp:674-687).
        let unique_domains: HashSet<&str> = validator_entries
            .iter()
            .map(|(_, _, domain, _)| domain.as_deref().unwrap_or(""))
            .collect();
        if unique_domains.len() > 1 {
            ValidationThresholdLevel::ByzantineFaultTolerance
        } else {
            ValidationThresholdLevel::SimpleMajority
        }
    };

    config.compat_quorum_safety = Some(CompatQuorumSafety {
        failure_safety,
        unsafe_quorum,
        threshold_level,
    });

    warn_unrecognized_keys(table);

    Ok(config)
}

/// Result of classifying config keys as supported, unsupported-known, or unknown.
#[derive(Debug, Default, PartialEq)]
struct UnrecognizedKeys {
    /// Valid stellar-core keys that henyey intentionally skips.
    unsupported: Vec<String>,
    /// Keys not in either supported or unsupported lists — likely typos.
    unknown: Vec<String>,
    /// Unknown keys found in `[[VALIDATORS]]` sub-tables (index, key).
    validator_unknown: Vec<(usize, String)>,
    /// Unknown keys found in `[QUORUM_SET]`.
    quorum_set_unknown: Vec<String>,
    /// Unknown keys found in `[HISTORY.*]` entries (archive name, key).
    history_unknown: Vec<(String, String)>,
}

impl UnrecognizedKeys {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.unsupported.is_empty()
            && self.unknown.is_empty()
            && self.validator_unknown.is_empty()
            && self.quorum_set_unknown.is_empty()
            && self.history_unknown.is_empty()
    }
}

/// Classify all keys in a stellar-core config table.
fn classify_keys(table: &toml::map::Map<String, toml::Value>) -> UnrecognizedKeys {
    let supported: HashSet<&str> = SUPPORTED_KEYS.iter().copied().collect();
    let unsupported_set: HashSet<&str> = UNSUPPORTED_KNOWN_KEYS.iter().copied().collect();

    let mut result = UnrecognizedKeys::default();

    for key in table.keys() {
        let k = key.as_str();
        if supported.contains(k) {
            // Known and handled.
        } else if unsupported_set.contains(k) {
            result.unsupported.push(key.clone());
        } else {
            result.unknown.push(key.clone());
        }
    }

    // Sub-table: [[VALIDATORS]]
    let val_supported: HashSet<&str> = VALIDATOR_SUPPORTED_KEYS.iter().copied().collect();
    let val_unsupported: HashSet<&str> = VALIDATOR_UNSUPPORTED_KEYS.iter().copied().collect();
    if let Some(validators) = table.get("VALIDATORS").and_then(|v| v.as_array()) {
        for (i, val) in validators.iter().enumerate() {
            if let Some(val_table) = val.as_table() {
                for key in val_table.keys() {
                    let k = key.as_str();
                    if !val_supported.contains(k) && !val_unsupported.contains(k) {
                        result.validator_unknown.push((i, key.clone()));
                    }
                }
            }
        }
    }

    // Sub-table: [QUORUM_SET]
    let qs_recognized: HashSet<&str> = QUORUM_SET_KEYS.iter().copied().collect();
    if let Some(qs_table) = table.get("QUORUM_SET").and_then(|v| v.as_table()) {
        for key in qs_table.keys() {
            if !qs_recognized.contains(key.as_str()) {
                result.quorum_set_unknown.push(key.clone());
            }
        }
    }

    // Sub-table: [HISTORY.*]
    let hist_recognized: HashSet<&str> = HISTORY_ENTRY_KEYS.iter().copied().collect();
    if let Some(history_table) = table.get("HISTORY").and_then(|v| v.as_table()) {
        for (name, entry) in history_table {
            if let Some(entry_table) = entry.as_table() {
                for key in entry_table.keys() {
                    if !hist_recognized.contains(key.as_str()) {
                        result.history_unknown.push((name.clone(), key.clone()));
                    }
                }
            }
        }
    }

    result
}

/// Parse `[[HOME_DOMAINS]]` entries into a domain→quality map.
///
/// Matches stellar-core's HOME_DOMAINS parsing (Config.cpp:783-829).
fn parse_home_domains(
    table: &toml::map::Map<String, toml::Value>,
) -> anyhow::Result<HashMap<String, ValidatorQuality>> {
    let mut map = HashMap::new();
    let Some(domains) = table.get("HOME_DOMAINS") else {
        return Ok(map);
    };
    let arr = domains
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("HOME_DOMAINS must be an array of tables"))?;
    for (i, entry) in arr.iter().enumerate() {
        let entry_table = entry
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("[[HOME_DOMAINS]] entry {} must be a table", i))?;
        let domain = entry_table
            .get("HOME_DOMAIN")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("[[HOME_DOMAINS]] entry {} missing HOME_DOMAIN", i))?
            .to_string();
        let quality_str = entry_table
            .get("QUALITY")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("[[HOME_DOMAINS]] entry {} missing QUALITY", i))?;
        let quality = ValidatorQuality::from_str(quality_str).ok_or_else(|| {
            anyhow::anyhow!(
                "[[HOME_DOMAINS]] entry {}: unknown QUALITY '{}'",
                i,
                quality_str
            )
        })?;
        // Check for unknown fields
        for key in entry_table.keys() {
            if key != "HOME_DOMAIN" && key != "QUALITY" {
                anyhow::bail!("Unknown field '{}' in [[HOME_DOMAINS]] entry {}", key, i);
            }
        }
        if map.insert(domain.clone(), quality).is_some() {
            anyhow::bail!("Duplicate HOME_DOMAINS entry for '{}'", domain);
        }
    }
    Ok(map)
}

/// Build a `ValidatorWeightConfig` from parsed validator entries and domain→quality map.
///
/// Resolves each validator's quality from either inline QUALITY or the
/// [[HOME_DOMAINS]] map. Adds a self-entry when the node is a validator.
///
/// Returns `Ok(None)` if validators don't have quality/home_domain data
/// (e.g., captive-core configs without [[HOME_DOMAINS]]).
fn build_validator_weight_config(
    config: &AppConfig,
    validator_entries: &[(String, String, Option<String>, Option<String>)], // (pubkey, name, home_domain, quality)
    domain_quality_map: &HashMap<String, ValidatorQuality>,
) -> anyhow::Result<Option<ValidatorWeightConfig>> {
    use stellar_xdr::curr::NodeId;

    let mut entries: Vec<(NodeId, ValidatorEntryInfo)> = Vec::new();

    for (pubkey, name, home_domain, quality_str) in validator_entries {
        // Resolve home domain — when HOME_DOMAINS is present, all validators
        // must have HOME_DOMAIN (matching stellar-core Config.cpp:719-745).
        let Some(domain) = home_domain.as_deref() else {
            if domain_quality_map.is_empty() {
                // No HOME_DOMAINS at all — feature not in use
                return Ok(None);
            }
            anyhow::bail!(
                "Validator '{}': missing HOME_DOMAIN (required when HOME_DOMAINS is present)",
                name
            );
        };

        // Resolve quality: stellar-core rejects double-definition (inline
        // QUALITY when HOME_DOMAINS already provides it for this domain).
        let quality = match (quality_str, domain_quality_map.get(domain)) {
            (Some(_qs), Some(_)) => {
                anyhow::bail!(
                    "Validator '{}': quality already defined in home domain '{}'",
                    name,
                    domain
                );
            }
            (Some(qs), None) => ValidatorQuality::from_str(qs)
                .ok_or_else(|| anyhow::anyhow!("Validator '{}': unknown QUALITY '{}'", name, qs))?,
            (None, Some(q)) => *q,
            (None, None) => {
                if domain_quality_map.is_empty() {
                    // No HOME_DOMAINS at all — can't build weight config
                    return Ok(None);
                }
                anyhow::bail!(
                    "Validator '{}': missing quality (no inline QUALITY and home domain '{}' not in HOME_DOMAINS)",
                    name,
                    domain
                );
            }
        };

        let node_id = parse_node_id(pubkey)?;
        entries.push((
            node_id,
            ValidatorEntryInfo {
                name: name.clone(),
                home_domain: domain.to_string(),
                quality,
            },
        ));
    }

    // Add self-entry (matches stellar-core's addSelfToValidators, Config.cpp:880-908).
    // Self is added when NODE_IS_VALIDATOR and not the "empty validators + manual QUORUM_SET" case.
    // The caller already checks those conditions.
    if let Some(ref seed_str) = config.node.node_seed {
        let node_home_domain = config.node.home_domain.as_deref().unwrap_or("");
        if node_home_domain.is_empty() {
            if domain_quality_map.is_empty() {
                // No HOME_DOMAINS at all — feature not in use
                return Ok(None);
            }
            anyhow::bail!("NODE_HOME_DOMAIN is required when HOME_DOMAINS is present");
        }

        let quality = if let Some(q) = domain_quality_map.get(node_home_domain) {
            *q
        } else if domain_quality_map.is_empty() {
            // No HOME_DOMAINS — feature not in use
            return Ok(None);
        } else {
            anyhow::bail!(
                "NODE_HOME_DOMAIN '{}' not found in HOME_DOMAINS",
                node_home_domain
            );
        };

        let secret = henyey_crypto::SecretKey::from_strkey(seed_str)
            .map_err(|e| anyhow::anyhow!("Invalid NODE_SEED for self-entry: {}", e))?;
        let self_node_id = parse_node_id(&secret.public_key().to_strkey())?;
        entries.push((
            self_node_id,
            ValidatorEntryInfo {
                name: "self".to_string(),
                home_domain: node_home_domain.to_string(),
                quality,
            },
        ));
    }

    if entries.is_empty() {
        return Ok(None);
    }

    match ValidatorWeightConfig::new(&entries) {
        Ok(vwc) => Ok(Some(vwc)),
        Err(e) => anyhow::bail!("Invalid ValidatorWeightConfig: {}", e),
    }
}

/// Parse a public key string into a NodeId.
fn parse_node_id(pubkey: &str) -> anyhow::Result<stellar_xdr::curr::NodeId> {
    let pk = henyey_crypto::PublicKey::from_strkey(pubkey)
        .map_err(|e| anyhow::anyhow!("Invalid public key '{}': {}", pubkey, e))?;
    Ok(stellar_xdr::curr::NodeId(
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
            *pk.as_bytes(),
        )),
    ))
}

/// Warn about unrecognized keys in a stellar-core format config.
///
/// Classifies each top-level key as supported, unsupported-but-known, or
/// unknown, and emits appropriate log messages. Also validates sub-table
/// keys within `[[VALIDATORS]]`, `[QUORUM_SET]`, and `[HISTORY.*]`.
fn warn_unrecognized_keys(table: &toml::map::Map<String, toml::Value>) {
    let classified = classify_keys(table);

    if !classified.unsupported.is_empty() {
        tracing::info!(
            keys = %classified.unsupported.join(", "),
            "Compat config contains valid stellar-core keys not supported by henyey; ignoring"
        );
    }
    if !classified.unknown.is_empty() {
        tracing::warn!(
            keys = %classified.unknown.join(", "),
            "Unknown compat config keys (not recognized by henyey — check for typos)"
        );
    }
    for (i, key) in &classified.validator_unknown {
        tracing::warn!(
            key = key.as_str(),
            index = i,
            "Unknown key in [[VALIDATORS]] entry (check for typos)"
        );
    }
    for key in &classified.quorum_set_unknown {
        tracing::warn!(
            key = key.as_str(),
            "Unknown key in [QUORUM_SET] (check for typos)"
        );
    }
    for (name, key) in &classified.history_unknown {
        tracing::warn!(
            key = key.as_str(),
            archive = name.as_str(),
            "Unknown key in [HISTORY.{name}] entry (check for typos)"
        );
    }
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
    let val = table.get(key)?;
    match val.as_str() {
        Some(s) => Some(s.to_string()),
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected string)"
            );
            None
        }
    }
}

fn get_bool(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<bool> {
    let val = table.get(key)?;
    match val.as_bool() {
        Some(b) => Some(b),
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected boolean)"
            );
            None
        }
    }
}

fn get_u16(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<u16> {
    let val = table.get(key)?;
    let i = match val.as_integer() {
        Some(i) => i,
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected integer)"
            );
            return None;
        }
    };
    match u16::try_from(i) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                key,
                value = i,
                "Compat config key value overflows u16 range"
            );
            None
        }
    }
}

fn get_u32(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<u32> {
    let val = table.get(key)?;
    let i = match val.as_integer() {
        Some(i) => i,
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected integer)"
            );
            return None;
        }
    };
    match u32::try_from(i) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                key,
                value = i,
                "Compat config key value overflows u32 range"
            );
            None
        }
    }
}

fn get_usize(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<usize> {
    let val = table.get(key)?;
    let i = match val.as_integer() {
        Some(i) => i,
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected integer)"
            );
            return None;
        }
    };
    match usize::try_from(i) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                key,
                value = i,
                "Compat config key value overflows usize range"
            );
            None
        }
    }
}

fn get_i64(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<i64> {
    let val = table.get(key)?;
    match val.as_integer() {
        Some(i) => Some(i),
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected integer)"
            );
            None
        }
    }
}

fn get_f64(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<f64> {
    let val = table.get(key)?;
    match val.as_float() {
        Some(f) => Some(f),
        None => {
            // Try integer as float
            if let Some(i) = val.as_integer() {
                return Some(i as f64);
            }
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected float)"
            );
            None
        }
    }
}

fn get_string_array(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<Vec<String>> {
    let val = table.get(key)?;
    match val.as_array() {
        Some(arr) => Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        ),
        None => {
            tracing::warn!(
                key,
                actual_type = val.type_str(),
                "Compat config key has wrong type (expected array)"
            );
            None
        }
    }
}

/// Like `get_string_array` but returns an error if any element is not a string.
/// Use for security-sensitive fields where silent element drops would widen
/// attack surface (e.g., `PREFERRED_PEER_KEYS` under `PREFERRED_PEERS_ONLY`).
fn get_string_array_strict(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    let val = match table.get(key) {
        Some(v) => v,
        None => return Ok(None),
    };
    let arr = val
        .as_array()
        .ok_or_else(|| format!("{key}: expected array, got {}", val.type_str()))?;
    let mut result = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("{key}[{i}]: expected string, got {}", v.type_str()))?;
        result.push(s.to_string());
    }
    Ok(Some(result))
}

/// Like `get_bool` but returns an error on wrong type instead of warn+None.
/// Use for security-sensitive boolean fields where a silent default would
/// widen attack surface.
fn get_bool_strict(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<bool>, String> {
    let val = match table.get(key) {
        Some(v) => v,
        None => return Ok(None),
    };
    val.as_bool()
        .map(|b| Some(b))
        .ok_or_else(|| format!("{key}: expected boolean, got {}", val.type_str()))
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

    // --- Unknown key detection tests ---

    #[test]
    fn test_classify_all_supported_keys_no_warnings() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        assert!(
            classified.is_empty(),
            "All supported keys should produce no warnings: {classified:?}"
        );
    }

    #[test]
    fn test_classify_unsupported_known_keys() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            UNSAFE_QUORUM = true
            FORCE_SCP = true
            FAILURE_SAFETY = 0
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        // UNSAFE_QUORUM and FAILURE_SAFETY are now supported (actively translated).
        assert_eq!(classified.unsupported.len(), 1);
        assert!(classified.unsupported.contains(&"FORCE_SCP".to_string()));
        assert!(classified.unknown.is_empty());
    }

    #[test]
    fn test_classify_unknown_keys_detected() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTPP_PORT = 11626
            TOTALLY_MADE_UP = true
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        assert_eq!(classified.unknown.len(), 2);
        assert!(classified.unknown.contains(&"HTPP_PORT".to_string()));
        assert!(classified.unknown.contains(&"TOTALLY_MADE_UP".to_string()));
    }

    #[test]
    fn test_classify_validator_unknown_keys() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [[VALIDATORS]]
            NAME = "test"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
            BOGUS_FIELD = "hello"
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        assert!(classified.unknown.is_empty(), "Top-level should be clean");
        assert_eq!(classified.validator_unknown.len(), 1);
        assert_eq!(
            classified.validator_unknown[0],
            (0, "BOGUS_FIELD".to_string())
        );
    }

    #[test]
    fn test_classify_quorum_set_unknown_keys() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["$self"]
            INNER_QUORUM_SETS = []
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        assert_eq!(classified.quorum_set_unknown.len(), 1);
        assert_eq!(classified.quorum_set_unknown[0], "INNER_QUORUM_SETS");
    }

    #[test]
    fn test_classify_history_unknown_keys() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [HISTORY.sdf1]
            get = "curl -sf https://example.com/{0} -o {1}"
            unknown_field = "oops"
            "#,
        )
        .unwrap();
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table);
        assert_eq!(classified.history_unknown.len(), 1);
        assert_eq!(
            classified.history_unknown[0],
            ("sdf1".to_string(), "unknown_field".to_string())
        );
    }

    #[test]
    fn test_ssc_config_has_expected_unsupported_keys() {
        // The SSC fixture contains keys like EXPERIMENTAL_BUCKETLIST_DB,
        // COMMANDS, etc. — these should be classified as unsupported-known,
        // not unknown.
        let fixture = include_str!("compat_http/test_fixtures/ssc_generated_config.cfg");
        let raw: toml::Value = toml::from_str(fixture).unwrap();
        let table = raw.as_table().unwrap();
        let classified = classify_keys(table);
        // The SSC fixture has EXPERIMENTAL_BUCKETLIST_DB, COMMANDS, etc.
        assert!(
            classified.unknown.is_empty(),
            "SSC fixture should have no truly unknown keys, but found: {:?}",
            classified.unknown
        );
    }

    #[test]
    fn test_type_mismatch_does_not_error() {
        // Giving HTTP_PORT a string instead of integer should not crash,
        // just skip it (with a warning).
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = "not_a_number"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        // HTTP_PORT was not parsed, so compat_http should use defaults
        assert!(!config.compat_http.enabled);
    }

    #[test]
    fn test_is_stellar_core_format_detects_unsupported_only_configs() {
        // A config that only has UNSAFE_QUORUM (unsupported-known) should
        // still be detected as stellar-core format.
        let core_toml: toml::Value = toml::from_str(
            r#"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        assert!(is_stellar_core_format(&core_toml));
    }

    #[test]
    fn test_parse_home_domains_valid() {
        let toml_str = r#"
            [[HOME_DOMAINS]]
            HOME_DOMAIN = "example.com"
            QUALITY = "HIGH"

            [[HOME_DOMAINS]]
            HOME_DOMAIN = "other.org"
            QUALITY = "MEDIUM"
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let table = raw.as_table().unwrap();
        let map = parse_home_domains(table).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["example.com"], ValidatorQuality::High);
        assert_eq!(map["other.org"], ValidatorQuality::Medium);
    }

    #[test]
    fn test_parse_home_domains_duplicate_rejected() {
        let toml_str = r#"
            [[HOME_DOMAINS]]
            HOME_DOMAIN = "example.com"
            QUALITY = "HIGH"

            [[HOME_DOMAINS]]
            HOME_DOMAIN = "example.com"
            QUALITY = "MEDIUM"
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let table = raw.as_table().unwrap();
        assert!(parse_home_domains(table).is_err());
    }

    #[test]
    fn test_parse_home_domains_invalid_quality_rejected() {
        let toml_str = r#"
            [[HOME_DOMAINS]]
            HOME_DOMAIN = "example.com"
            QUALITY = "SUPER"
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let table = raw.as_table().unwrap();
        assert!(parse_home_domains(table).is_err());
    }

    #[test]
    fn test_parse_home_domains_case_sensitive() {
        // stellar-core uses exact match — lowercase should be rejected
        let toml_str = r#"
            [[HOME_DOMAINS]]
            HOME_DOMAIN = "example.com"
            QUALITY = "high"
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let table = raw.as_table().unwrap();
        assert!(parse_home_domains(table).is_err());
    }

    #[test]
    fn test_parse_home_domains_empty() {
        let raw: toml::Value = toml::from_str("").unwrap();
        let table = raw.as_table().unwrap();
        let map = parse_home_domains(table).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn test_build_validator_weight_config_basic() {
        let mut config = AppConfig::testnet();
        config.node.node_seed = None; // No self-entry

        let entries = vec![(
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y".to_string(),
            "sdf_testnet_1".to_string(),
            Some("testnet.stellar.org".to_string()),
            None, // quality from HOME_DOMAINS
        )];
        let mut domain_map = HashMap::new();
        domain_map.insert("testnet.stellar.org".to_string(), ValidatorQuality::High);

        let result = build_validator_weight_config(&config, &entries, &domain_map).unwrap();
        assert!(result.is_some());
        let vwc = result.unwrap();
        assert_eq!(vwc.quality_weights[&ValidatorQuality::High], u64::MAX);
    }

    #[test]
    fn test_build_validator_weight_config_double_definition_rejected() {
        let mut config = AppConfig::testnet();
        config.node.node_seed = None;

        let entries = vec![(
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y".to_string(),
            "sdf_testnet_1".to_string(),
            Some("testnet.stellar.org".to_string()),
            Some("MEDIUM".to_string()), // inline QUALITY
        )];
        let mut domain_map = HashMap::new();
        domain_map.insert("testnet.stellar.org".to_string(), ValidatorQuality::High);

        // Double-definition: inline QUALITY + HOME_DOMAINS should error
        assert!(build_validator_weight_config(&config, &entries, &domain_map).is_err());
    }

    #[test]
    fn test_build_validator_weight_config_no_home_domains_returns_none() {
        let mut config = AppConfig::testnet();
        config.node.node_seed = None;

        let entries = vec![(
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y".to_string(),
            "sdf_testnet_1".to_string(),
            Some("testnet.stellar.org".to_string()),
            None, // no inline quality
        )];
        let domain_map = HashMap::new(); // empty HOME_DOMAINS

        // No quality data at all — should return None gracefully
        let result = build_validator_weight_config(&config, &entries, &domain_map).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_build_validator_weight_config_missing_domain_with_home_domains_errors() {
        let mut config = AppConfig::testnet();
        config.node.node_seed = None;

        let entries = vec![(
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y".to_string(),
            "sdf_testnet_1".to_string(),
            Some("unknown.org".to_string()),
            None,
        )];
        let mut domain_map = HashMap::new();
        domain_map.insert("testnet.stellar.org".to_string(), ValidatorQuality::High);

        // HOME_DOMAINS exists but validator's domain isn't in it → error
        assert!(build_validator_weight_config(&config, &entries, &domain_map).is_err());
    }

    #[test]
    fn test_flood_arb_tx_base_allowance_parsed() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_ARB_TX_BASE_ALLOWANCE=10
FLOOD_ARB_TX_DAMPING_FACTOR=0.5
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.overlay.flood_arb_tx_base_allowance, 10);
        assert!((config.overlay.flood_arb_tx_damping_factor - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_flood_arb_tx_base_allowance_disabled() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_ARB_TX_BASE_ALLOWANCE=-1
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.overlay.flood_arb_tx_base_allowance, -1);
    }

    #[test]
    fn test_flood_arb_tx_base_allowance_overflow_ignored() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_ARB_TX_BASE_ALLOWANCE=4294967295
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Value overflows i32 — should be ignored, keeping the default.
        assert_eq!(config.overlay.flood_arb_tx_base_allowance, 5);
    }

    #[test]
    fn test_flood_tx_period_ms_parsed() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_TX_PERIOD_MS=300
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.overlay.flood_tx_period_ms, 300);
    }

    #[test]
    fn test_flood_tx_period_ms_default() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Default should be 200 (matching stellar-core FLOOD_TX_PERIOD_MS)
        assert_eq!(config.overlay.flood_tx_period_ms, 200);
    }

    #[test]
    fn test_flood_tx_period_ms_invalid_preserves_default() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_TX_PERIOD_MS=0
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Invalid value (0) should be ignored, keeping the default 200.
        assert_eq!(config.overlay.flood_tx_period_ms, 200);
    }

    #[test]
    fn test_flood_advert_period_ms_parsed() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_ADVERT_PERIOD_MS=50
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.overlay.flood_advert_period_ms, 50);
    }

    #[test]
    fn test_flood_advert_period_ms_invalid_preserves_default() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOOD_ADVERT_PERIOD_MS=-1
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Invalid value should be ignored, keeping the default 100.
        assert_eq!(config.overlay.flood_advert_period_ms, 100);
    }

    // --- PEER_FLOOD_READING_CAPACITY_BYTES / FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES ---

    #[test]
    fn test_flow_control_bytes_compat_parsed() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
PEER_FLOOD_READING_CAPACITY_BYTES=500000
FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES=100000
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.overlay.peer_flood_reading_capacity_bytes, 500_000);
        assert_eq!(
            config.overlay.flow_control_send_more_batch_size_bytes,
            100_000
        );
    }

    #[test]
    fn test_flow_control_bytes_compat_overflow_ignored() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
PEER_FLOOD_READING_CAPACITY_BYTES=4294967296
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Value exceeds u32::MAX — should be silently ignored, keeping default 0.
        assert_eq!(config.overlay.peer_flood_reading_capacity_bytes, 0);
    }

    #[test]
    fn test_flow_control_bytes_compat_negative_ignored() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES=-1
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // Negative value should be ignored, keeping default 0.
        assert_eq!(config.overlay.flow_control_send_more_batch_size_bytes, 0);
    }

    #[test]
    fn test_flow_control_bytes_compat_defaults() {
        let toml_str = r#"
NETWORK_PASSPHRASE="Test SDF Network ; September 2015"
NODE_SEED="SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH self"
"#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        // When not specified, defaults are 0 (auto-compute).
        assert_eq!(config.overlay.peer_flood_reading_capacity_bytes, 0);
        assert_eq!(config.overlay.flow_control_send_more_batch_size_bytes, 0);
    }

    // --- FAILURE_SAFETY / UNSAFE_QUORUM tests ---

    /// Helper: create a minimal compat config string for a validator with the
    /// given quorum-related fields appended.
    fn compat_validator_config(extra: &str) -> String {
        format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            DATABASE = "sqlite3:///tmp/test.db"
            {extra}
            [HISTORY.testnet]
            get = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{{0}}/{{1}}/{{2}}/{{3}} -o {{4}}"
            "#
        )
    }

    /// Helper: translate a compat config string and return the AppConfig.
    fn translate(config_str: &str) -> anyhow::Result<crate::config::AppConfig> {
        let raw: toml::Value = toml::from_str(config_str).unwrap();
        translate_stellar_core_config(&raw)
    }

    #[test]
    fn test_failure_safety_auto_passes_good_quorum() {
        // 4 validators from different domains, default threshold (67%)
        // BFT: min_threshold = 4 - (4-1)/3 = 3, FAILURE_SAFETY = 4 - 3 = 1
        // closest v-blocking size = 4 - 3 + 1 = 2, so 1 < 2 → passes
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let key4 = henyey_crypto::SecretKey::from_seed(&[4u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "{key1}"
            HOME_DOMAIN = "a.com"

            [[VALIDATORS]]
            NAME = "v2"
            PUBLIC_KEY = "{key2}"
            HOME_DOMAIN = "b.com"

            [[VALIDATORS]]
            NAME = "v3"
            PUBLIC_KEY = "{key3}"
            HOME_DOMAIN = "c.com"

            [[VALIDATORS]]
            NAME = "v4"
            PUBLIC_KEY = "{key4}"
            HOME_DOMAIN = "d.com"
            "#
        ));
        let config = translate(&cfg).unwrap();
        assert!(config.compat_quorum_safety.is_some());
        // Validation should pass
        config.validate().unwrap();
    }

    #[test]
    fn test_failure_safety_explicit_rejects_incompatible() {
        // 2-of-3 quorum set with FAILURE_SAFETY=2
        // closest v-blocking size = 2, so FAILURE_SAFETY(2) >= 2 → rejected
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            FAILURE_SAFETY = 2
            UNSAFE_QUORUM = true
            [QUORUM_SET]
            THRESHOLD_PERCENT = 67
            VALIDATORS = ["{key1}", "{key2}", "{key3}"]
            "#
        ));
        let config = translate(&cfg).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("FAILURE_SAFETY") && err.to_string().contains("quorum"),
            "Expected FAILURE_SAFETY incompatible error, got: {err}"
        );
    }

    #[test]
    fn test_failure_safety_zero_rejected_without_unsafe() {
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{key1}"]
            "#
        ));
        let config = translate(&cfg).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("FAILURE_SAFETY=0"),
            "Expected FAILURE_SAFETY=0 error, got: {err}"
        );
    }

    #[test]
    fn test_failure_safety_zero_accepted_with_unsafe() {
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            FAILURE_SAFETY = 0
            UNSAFE_QUORUM = true
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{key1}"]
            "#
        ));
        let config = translate(&cfg).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn test_low_threshold_rejected_without_unsafe() {
        // 4 validators, threshold 51% → passes is_quorum_set_sane (>50%)
        // but fails BFT check (min threshold = 3 for 4 nodes, 51% of 4 = 3, just barely ok)
        // Try 7 validators with threshold 58%: ceil(7*0.58) = 5, BFT min = 7-(7-1)/3 = 5.
        // That passes. Use 57%: ceil(7*0.57) = 4 < 5 BFT min. And 4/7 > 50% so sanity passes.
        let keys: Vec<String> = (1..=7u8)
            .map(|i| {
                henyey_crypto::SecretKey::from_seed(&[i; 32])
                    .public_key()
                    .to_strkey()
            })
            .collect();
        let validators_str = keys
            .iter()
            .map(|k| format!(r#""{k}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let cfg = compat_validator_config(&format!(
            r#"
            FAILURE_SAFETY = 1
            [QUORUM_SET]
            THRESHOLD_PERCENT = 57
            VALIDATORS = [{validators_str}]
            "#
        ));
        let config = translate(&cfg).unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("THRESHOLD_PERCENTAGE is too low"),
            "Expected threshold too low error, got: {err}"
        );
    }

    #[test]
    fn test_low_threshold_accepted_with_unsafe() {
        // Same 7 validators with low threshold, but UNSAFE_QUORUM=true bypasses checks.
        let keys: Vec<String> = (1..=7u8)
            .map(|i| {
                henyey_crypto::SecretKey::from_seed(&[i; 32])
                    .public_key()
                    .to_strkey()
            })
            .collect();
        let validators_str = keys
            .iter()
            .map(|k| format!(r#""{k}""#))
            .collect::<Vec<_>>()
            .join(", ");
        let cfg = compat_validator_config(&format!(
            r#"
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 57
            VALIDATORS = [{validators_str}]
            "#
        ));
        let config = translate(&cfg).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn test_manual_quorum_set_differs_from_validators_rejected() {
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let key4 = henyey_crypto::SecretKey::from_seed(&[4u8; 32])
            .public_key()
            .to_strkey();
        // [[VALIDATORS]] has 4 keys, but [QUORUM_SET] overrides with only 2
        let cfg = compat_validator_config(&format!(
            r#"
            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "{key1}"
            HOME_DOMAIN = "a.com"

            [[VALIDATORS]]
            NAME = "v2"
            PUBLIC_KEY = "{key2}"
            HOME_DOMAIN = "b.com"

            [[VALIDATORS]]
            NAME = "v3"
            PUBLIC_KEY = "{key3}"
            HOME_DOMAIN = "c.com"

            [[VALIDATORS]]
            NAME = "v4"
            PUBLIC_KEY = "{key4}"
            HOME_DOMAIN = "d.com"

            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{key1}", "{key2}"]
            "#
        ));
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("UNSAFE_QUORUM=true"),
            "Expected UNSAFE_QUORUM gate error, got: {err}"
        );
    }

    #[test]
    fn test_manual_quorum_set_identical_to_validators_accepted() {
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let key4 = henyey_crypto::SecretKey::from_seed(&[4u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "{key1}"
            HOME_DOMAIN = "a.com"

            [[VALIDATORS]]
            NAME = "v2"
            PUBLIC_KEY = "{key2}"
            HOME_DOMAIN = "b.com"

            [[VALIDATORS]]
            NAME = "v3"
            PUBLIC_KEY = "{key3}"
            HOME_DOMAIN = "c.com"

            [[VALIDATORS]]
            NAME = "v4"
            PUBLIC_KEY = "{key4}"
            HOME_DOMAIN = "d.com"

            [QUORUM_SET]
            THRESHOLD_PERCENT = 67
            VALIDATORS = ["{key1}", "{key2}", "{key3}", "{key4}"]
            "#
        ));
        // Same validators and same effective threshold → should be accepted
        let config = translate(&cfg).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn test_manual_quorum_set_differs_accepted_with_unsafe() {
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let key4 = henyey_crypto::SecretKey::from_seed(&[4u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0

            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "{key1}"
            HOME_DOMAIN = "a.com"

            [[VALIDATORS]]
            NAME = "v2"
            PUBLIC_KEY = "{key2}"
            HOME_DOMAIN = "b.com"

            [[VALIDATORS]]
            NAME = "v3"
            PUBLIC_KEY = "{key3}"
            HOME_DOMAIN = "c.com"

            [[VALIDATORS]]
            NAME = "v4"
            PUBLIC_KEY = "{key4}"
            HOME_DOMAIN = "d.com"

            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{key1}", "{key2}"]
            "#
        ));
        let config = translate(&cfg).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn test_manual_quorum_set_lower_threshold_rejected() {
        // Same validators but lower threshold → differs → rejected without UNSAFE_QUORUM
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let key3 = henyey_crypto::SecretKey::from_seed(&[3u8; 32])
            .public_key()
            .to_strkey();
        let key4 = henyey_crypto::SecretKey::from_seed(&[4u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "{key1}"
            HOME_DOMAIN = "a.com"

            [[VALIDATORS]]
            NAME = "v2"
            PUBLIC_KEY = "{key2}"
            HOME_DOMAIN = "b.com"

            [[VALIDATORS]]
            NAME = "v3"
            PUBLIC_KEY = "{key3}"
            HOME_DOMAIN = "c.com"

            [[VALIDATORS]]
            NAME = "v4"
            PUBLIC_KEY = "{key4}"
            HOME_DOMAIN = "d.com"

            [QUORUM_SET]
            THRESHOLD_PERCENT = 34
            VALIDATORS = ["{key1}", "{key2}", "{key3}", "{key4}"]
            "#
        ));
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("UNSAFE_QUORUM=true"),
            "Expected UNSAFE_QUORUM gate error, got: {err}"
        );
    }

    #[test]
    fn test_standalone_quorum_set_without_validators_allowed() {
        // [QUORUM_SET] without [[VALIDATORS]] — always allowed, no UNSAFE_QUORUM needed
        let key1 = henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey();
        let key2 = henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey();
        let cfg = compat_validator_config(&format!(
            r#"
            FAILURE_SAFETY = 0
            UNSAFE_QUORUM = true
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{key1}", "{key2}"]
            "#
        ));
        let config = translate(&cfg).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn test_failure_safety_below_minus_one_rejected() {
        let cfg = compat_validator_config(
            r#"
            FAILURE_SAFETY = -5
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"]
            "#,
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("FAILURE_SAFETY must be between"),
            "Expected range error, got: {err}"
        );
    }

    #[test]
    fn test_failure_safety_non_integer_rejected() {
        let cfg = compat_validator_config(
            r#"
            FAILURE_SAFETY = "foo"
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"]
            "#,
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string()
                .contains("FAILURE_SAFETY must be an integer"),
            "Expected type error, got: {err}"
        );
    }

    #[test]
    fn test_native_config_skips_failure_safety_validation() {
        // Native henyey config has no compat_quorum_safety
        let config = crate::config::AppConfig::testnet();
        assert!(config.compat_quorum_safety.is_none());
    }

    #[test]
    fn test_reordered_manual_quorum_set_rejected_without_unsafe() {
        // Manual [QUORUM_SET] with same validators in different order is
        // rejected without UNSAFE_QUORUM, matching stellar-core's
        // order-sensitive serialized comparison (Config.cpp:2087-2099).
        let cfg = compat_validator_config(
            r#"
            [[VALIDATORS]]
            PUBLIC_KEY = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v1"
            HOME_DOMAIN = "a.org"
            [[VALIDATORS]]
            PUBLIC_KEY = "GBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v2"
            HOME_DOMAIN = "b.org"
            [[VALIDATORS]]
            PUBLIC_KEY = "GCAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v3"
            HOME_DOMAIN = "c.org"
            [[VALIDATORS]]
            PUBLIC_KEY = "GDAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v4"
            HOME_DOMAIN = "d.org"
            [QUORUM_SET]
            THRESHOLD_PERCENT = 67
            VALIDATORS = [
                "GBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
                "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
                "GCAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
                "GDAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            ]
            "#,
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("UNSAFE_QUORUM"),
            "Expected UNSAFE_QUORUM gate for reordered qset, got: {err}"
        );
    }

    #[test]
    fn test_mixed_home_domain_uses_bft_threshold() {
        // Validators with missing HOME_DOMAIN count as empty-string domain,
        // so a mix of present + missing HOME_DOMAIN gives >1 unique domain
        // and triggers BFT threshold validation.
        let cfg = compat_validator_config(
            r#"
            FAILURE_SAFETY = 1
            [[VALIDATORS]]
            PUBLIC_KEY = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v1"
            HOME_DOMAIN = "a.org"
            [[VALIDATORS]]
            PUBLIC_KEY = "GBAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v2"
            [[VALIDATORS]]
            PUBLIC_KEY = "GCAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v3"
            HOME_DOMAIN = "a.org"
            [[VALIDATORS]]
            PUBLIC_KEY = "GDAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
            NAME = "v4"
            HOME_DOMAIN = "a.org"
            [HISTORY.testnet2]
            get = "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"
            "#,
        );
        let app_config = translate(&cfg).unwrap();
        // Should use BFT because we have 2 unique domains: "a.org" and ""
        let safety = app_config.compat_quorum_safety.as_ref().unwrap();
        assert_eq!(
            safety.threshold_level,
            crate::config::ValidationThresholdLevel::ByzantineFaultTolerance,
            "Mixed HOME_DOMAIN validators should use BFT threshold"
        );
    }

    #[test]
    fn test_translate_surveyor_keys() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = [
                "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS",
                "GCGB2S2KBER5MNQNJTNF5N3Y4PEPFMHONPIGXNOYIREMYCMJZ3GAVDXQ"
            ]
            "#
        );
        let app_config = translate(&cfg).unwrap();
        assert_eq!(app_config.overlay.surveyor_keys.len(), 2);
        assert_eq!(
            app_config.overlay.surveyor_keys[0],
            "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS"
        );
        assert_eq!(
            app_config.overlay.surveyor_keys[1],
            "GCGB2S2KBER5MNQNJTNF5N3Y4PEPFMHONPIGXNOYIREMYCMJZ3GAVDXQ"
        );
    }

    #[test]
    fn test_translate_surveyor_keys_empty() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = []
            "#
        );
        let app_config = translate(&cfg).unwrap();
        assert!(app_config.overlay.surveyor_keys.is_empty());
    }

    #[test]
    fn test_translate_surveyor_keys_rejects_non_string() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = ["GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS", 42]
            "#
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("SURVEYOR_KEYS[1]"),
            "Expected type error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_surveyor_keys_rejects_non_array() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS"
            "#
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("SURVEYOR_KEYS: expected array"),
            "Expected array error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peer_keys() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            PREFERRED_PEER_KEYS = [
                "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS",
                "GCKWUQOSX4MMMCEHQ34E7EE7XQHJEMHEBMMXBS2SXMP3KV3HQQISBUU7"
            ]
            PREFERRED_PEERS_ONLY = true
            "#
        );
        let config = translate(&cfg).unwrap();
        assert_eq!(config.overlay.preferred_peer_keys.len(), 2);
        assert!(config
            .overlay
            .preferred_peer_keys
            .contains(&"GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS".to_string()));
        assert!(config
            .overlay
            .preferred_peer_keys
            .contains(&"GCKWUQOSX4MMMCEHQ34E7EE7XQHJEMHEBMMXBS2SXMP3KV3HQQISBUU7".to_string()));
        assert!(config.overlay.preferred_peers_only);
    }

    #[test]
    fn test_translate_preferred_peer_keys_invalid_element() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            PREFERRED_PEER_KEYS = ["valid-string", 42]
            "#
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("PREFERRED_PEER_KEYS[1]"),
            "Expected element error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peers_only_wrong_type() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            PREFERRED_PEERS_ONLY = "yes"
            "#
        );
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("PREFERRED_PEERS_ONLY"),
            "Expected type error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peers_only_default_false() {
        let cfg = format!(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            "#
        );
        let config = translate(&cfg).unwrap();
        assert!(!config.overlay.preferred_peers_only);
        assert!(config.overlay.preferred_peer_keys.is_empty());
    }
}
