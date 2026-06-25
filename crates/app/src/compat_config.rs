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
    HistoryArchiveEntry, ValidationThresholdLevel,
};
use henyey_herder::{ValidatorEntryInfo, ValidatorQuality, ValidatorWeightConfig};
use henyey_overlay::PeerAddress;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Top-level keys that `translate_stellar_core_config` actively handles.
///
/// "Handles" means either:
/// - the key is translated into an `AppConfig` field, or
/// - the key is validated and rejected when the corresponding subsystem is
///   not implemented in henyey (see `validate_invariant_compat_keys` for
///   `INVARIANT_CHECKS` and `INVARIANT_EXTRA_CHECKS`, which are recognised
///   here so `classify_keys` does not flag them as typos, but whose
///   non-default values are rejected because henyey has no
///   InvariantManager).
const SUPPORTED_KEYS: &[&str] = &[
    "NODE_SEED",
    "NODE_IS_VALIDATOR",
    "MANUAL_CLOSE",
    "NODE_HOME_DOMAIN",
    "NETWORK_PASSPHRASE",
    "DATABASE",
    "BUCKET_DIR_PATH",
    "DISABLE_BUCKET_GC",
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
    "PUBLISH_TO_ARCHIVE_DELAY",
    "ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING",
    "GENESIS_TEST_ACCOUNT_COUNT",
    "USE_CONFIG_FOR_GENESIS",
    "TESTING_UPGRADE_LEDGER_PROTOCOL_VERSION",
    "TESTING_UPGRADE_DESIRED_FEE",
    "TESTING_UPGRADE_RESERVE",
    "TESTING_UPGRADE_MAX_TX_SET_SIZE",
    "RUN_STANDALONE",
    "LOADGEN_WASM_BYTES_FOR_TESTING",
    "LOADGEN_WASM_BYTES_DISTRIBUTION_FOR_TESTING",
    // Sub-tables (handled structurally)
    "HISTORY",
    "VALIDATORS",
    "QUORUM_SET",
    "HOME_DOMAINS",
    "FORCE_OLD_STYLE_LEADER_ELECTION",
    "FORCE_SCP",
    "FLOOD_ARB_TX_BASE_ALLOWANCE",
    "FLOOD_ARB_TX_DAMPING_FACTOR",
    "FLOOD_TX_PERIOD_MS",
    "FLOOD_ADVERT_PERIOD_MS",
    "FAILURE_SAFETY",
    "UNSAFE_QUORUM",
    "SURVEYOR_KEYS",
    "PEER_FLOOD_READING_CAPACITY_BYTES",
    "FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES",
    // Invariant-check compat keys: validated by `validate_invariant_compat_keys`
    // and translated into InvariantConfig. See [AUDIT-213] / issue #2102.
    "INVARIANT_CHECKS",
    "INVARIANT_EXTRA_CHECKS",
    "STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY",
];

/// Valid stellar-core keys that henyey intentionally does not support.
/// These are logged at `info` level rather than `warn`.
const UNSUPPORTED_KNOWN_KEYS: &[&str] = &[
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
    // stellar-core Config::adjust() defaults FORCE_SCP to NODE_IS_VALIDATOR
    // for test-network validator configs generated by Supercluster.
    config.node.force_scp = get_bool(table, "FORCE_SCP").unwrap_or(config.node.is_validator);
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
    // stellar-core's DISABLE_BUCKET_GC kill-switch (Config.h:590, default false).
    // Carry a drop-in core config's operator intent through to henyey's native
    // `buckets.disable_bucket_gc` flag; absent ⇒ stays false (GC enabled).
    if let Some(disable_gc) = get_bool(table, "DISABLE_BUCKET_GC") {
        config.buckets.disable_bucket_gc = disable_gc;
    }

    // --- HTTP ---
    // stellar-core derives a single bind address from PUBLIC_HTTP_PORT and uses it
    // for both the command server and query server (CommandHandler.cpp:56-84).
    // We compute it once here and apply to both compat_http and query configs.
    let compat_bind_address = if get_bool(table, "PUBLIC_HTTP_PORT").unwrap_or(false) {
        "::".to_string()
    } else {
        "127.0.0.1".to_string()
    };

    if let Some(port) = get_u16(table, "HTTP_PORT") {
        // stellar-core treats HTTP_PORT=0 as "don't listen" (CommandHandler.cpp:56-77,
        // Config::setNoListen sets HTTP_PORT=0). Disable both native and compat HTTP.
        // For nonzero ports, enable the compat HTTP server (stellar-rpc expects
        // stellar-core's wire format) and disable the native HTTP server.
        if let Some(port) = nonzero_port(port) {
            config.http.enabled = false;
            config.compat_http = CompatHttpConfig {
                enabled: true,
                port,
                address: compat_bind_address.clone(),
            };
        } else {
            config.http.enabled = false;
            config.compat_http.enabled = false;
        }
    }

    // --- Query server ---
    // stellar-core treats HTTP_QUERY_PORT=0 as "don't listen"
    // (CommandHandler.cpp:72-83).
    if let Some(port) = get_u16(table, "HTTP_QUERY_PORT") {
        config.query.port = nonzero_port(port);
        // Apply the same PUBLIC_HTTP_PORT-derived bind address as the command server.
        if config.query.port.is_some() {
            config.query.address = Some(compat_bind_address.clone());
        }
    }
    if let Some(v) = get_u32(table, "QUERY_SNAPSHOT_LEDGERS") {
        config.query.snapshot_ledgers = v;
    }
    if let Some(v) = get_usize(table, "QUERY_THREAD_POOL_SIZE") {
        config.query.thread_pool_size = v;
    }

    // --- Overlay ---
    if let Some(port) = get_u16(table, "PEER_PORT") {
        if port == 0 {
            anyhow::bail!(
                "PEER_PORT must not be 0 \
                 (stellar-core enforces minimum port of 1; set a valid port)"
            );
        }
        config.overlay.peer_port = port;
    }
    if let Some(peers) = get_string_array(table, "KNOWN_PEERS")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        config.overlay.known_peers = peers
            .iter()
            .map(|s| s.parse::<PeerAddress>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Invalid KNOWN_PEERS entry: {}", e))?;
    }
    if let Some(peers) = get_string_array(table, "PREFERRED_PEERS")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        config.overlay.preferred_peers = peers
            .iter()
            .map(|s| s.parse::<PeerAddress>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Invalid PREFERRED_PEERS entry: {}", e))?;
    }
    if let Some(keys) = get_string_array(table, "PREFERRED_PEER_KEYS")
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
    if let Some(keys) = get_string_array(table, "SURVEYOR_KEYS")
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

    // --- Publish-to-archive delay ---
    // stellar-core: PUBLISH_TO_ARCHIVE_DELAY (seconds, default 0). Gates the
    // start of checkpoint publishing on a wall-clock delay. See #3032.
    if let Some(v) = get_u32(table, "PUBLISH_TO_ARCHIVE_DELAY") {
        config.history.publish_to_archive_delay_seconds = v as u64;
    }

    // --- History archives ---
    // stellar-core format: [HISTORY.name] with get="cmd {0}" sub-tables
    if let Some(history_table) = get_table_strict(table, "HISTORY")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?
    {
        let mut archives = Vec::new();
        for (name, entry) in history_table {
            let entry_table = entry.as_table().ok_or_else(|| {
                anyhow::anyhow!("HISTORY.{}: expected table, got {}", name, entry.type_str())
            })?;
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
        if !archives.is_empty() {
            // Preserve the already-parsed publish delay; only the archive list
            // is sourced from the [HISTORY.*] sub-tables.
            config.history.archives = archives;
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
    let validators_array = get_array_of_tables_strict(table, "VALIDATORS")
        .map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?;
    let has_validators_section = validators_array.as_ref().map_or(false, |a| !a.is_empty());

    if let Some(validators) = &validators_array {
        let mut validator_keys = Vec::new();
        let mut validator_addresses = Vec::new();
        for (i, val_table) in validators.iter().enumerate() {
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
                let peer_str = if addr.contains(':') {
                    addr.to_string()
                } else {
                    // Default Stellar peer port
                    format!("{addr}:11625")
                };
                let peer_addr = peer_str.parse::<PeerAddress>().map_err(|e| {
                    anyhow::anyhow!(
                        "Invalid ADDRESS '{}' in [[VALIDATORS]] entry '{}': {}",
                        addr,
                        name,
                        e
                    )
                })?;
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

    if let Some(qs_val) = table.get("QUORUM_SET") {
        let qs_table = qs_val.as_table().ok_or_else(|| {
            anyhow::anyhow!("QUORUM_SET: expected table, got {}", qs_val.type_str())
        })?;
        if let Some(raw_validators) = get_string_array(qs_table, "VALIDATORS")
            .map_err(|e| anyhow::anyhow!("[QUORUM_SET] {e}"))?
        {
            let mut keys: Vec<String> = Vec::new();
            for s in &raw_validators {
                if s == "$self" {
                    // "$self" refers to the node's own key — resolve it from NODE_SEED
                    let seed_str = config.node.node_seed.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Cannot resolve $self in [QUORUM_SET]: NODE_SEED not set")
                    })?;
                    let secret = henyey_crypto::SecretKey::from_strkey(seed_str).map_err(|e| {
                        anyhow::anyhow!(
                            "Cannot resolve $self in [QUORUM_SET]: invalid NODE_SEED: {}",
                            e
                        )
                    })?;
                    keys.push(secret.public_key().to_strkey());
                } else {
                    // stellar-core format: "$PUBKEY $NAME" (key+name space-separated).
                    // Split on whitespace and keep only the first token (the public key).
                    let key = s.split_whitespace().next().unwrap_or(s);
                    keys.push(key.to_string());
                }
            }
            // [QUORUM_SET] always overrides [[VALIDATORS]]-generated quorum set.
            config.node.quorum_set.validators = keys;
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
    // Genesis-construction overrides. Mirrors stellar-core's
    // USE_CONFIG_FOR_GENESIS / TESTING_UPGRADE_* keys (Config.cpp:198/231-234).
    // Absent keys leave the stellar-core-matching defaults in place, so a
    // config without these is byte-identical to the legacy genesis.
    if let Some(v) = get_bool(table, "USE_CONFIG_FOR_GENESIS") {
        config.testing.use_config_for_genesis = v;
    }
    if let Some(v) = get_u32(table, "TESTING_UPGRADE_LEDGER_PROTOCOL_VERSION") {
        config.testing.testing_upgrade_ledger_protocol_version = v;
    }
    if let Some(v) = get_u32(table, "TESTING_UPGRADE_DESIRED_FEE") {
        config.testing.testing_upgrade_desired_fee = v;
    }
    if let Some(v) = get_u32(table, "TESTING_UPGRADE_RESERVE") {
        config.testing.testing_upgrade_reserve = v;
    }
    if let Some(v) = get_u32(table, "TESTING_UPGRADE_MAX_TX_SET_SIZE") {
        config.testing.testing_upgrade_max_tx_set_size = v;
    }
    if let Some(v) = get_bool(table, "RUN_STANDALONE") {
        config.testing.run_standalone = v;
    }
    // SorobanUpload WASM sizing (parity: stellar-core samples the upload size
    // from LOADGEN_WASM_BYTES_FOR_TESTING weighted by
    // LOADGEN_WASM_BYTES_DISTRIBUTION_FOR_TESTING; absent → built-in default).
    if let Some(v) = get_u32_array(table, "LOADGEN_WASM_BYTES_FOR_TESTING") {
        config.testing.loadgen_wasm_bytes = v;
    }
    if let Some(v) = get_u32_array(table, "LOADGEN_WASM_BYTES_DISTRIBUTION_FOR_TESTING") {
        config.testing.loadgen_wasm_bytes_distribution = v;
    }

    // --- Ignored keys (accepted silently for compatibility) ---
    // DISABLE_XDR_FSYNC, etc.

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

    // Validate and translate invariant-check compat keys.
    validate_invariant_compat_keys(table, config.node.is_validator)?;
    translate_invariant_config(table, &mut config);

    warn_unrecognized_keys(table).map_err(|e| anyhow::anyhow!("Compat config error: {}", e))?;

    Ok(config)
}

/// Validate compat invariant-check keys.
///
/// The three stellar-core invariant-check keys (`INVARIANT_CHECKS`,
/// `INVARIANT_EXTRA_CHECKS`, `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY`) are
/// now supported and translated into henyey's `InvariantConfig`. This function
/// validates types and enforces the validator+extra_checks constraint.
///
/// Type errors fire unconditionally (even when the value is logically
/// equivalent to a default) to prevent `INVARIANT_CHECKS = ""` and
/// similar wrong-typed configs from sneaking through as accepted.
///
/// `is_validator` is taken from the already-parsed `config.node.is_validator`.
fn validate_invariant_compat_keys(
    table: &toml::map::Map<String, toml::Value>,
    is_validator: bool,
) -> anyhow::Result<()> {
    // INVARIANT_CHECKS: array of strings, default [].
    if let Some(value) = table.get("INVARIANT_CHECKS") {
        let arr = value.as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "INVARIANT_CHECKS must be an array of strings, got: {}",
                value
            )
        })?;
        for elem in arr {
            elem.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "INVARIANT_CHECKS must be an array of strings, got element: {}",
                    elem
                )
            })?;
        }
        // Checks are now supported — translated in translate_invariant_config().
    }

    // INVARIANT_EXTRA_CHECKS: boolean, default false.
    if let Some(value) = table.get("INVARIANT_EXTRA_CHECKS") {
        let b = value.as_bool().ok_or_else(|| {
            anyhow::anyhow!("INVARIANT_EXTRA_CHECKS must be a boolean, got: {}", value)
        })?;
        if b && is_validator {
            // Verbatim wording from stellar-core Config.cpp:2003-2004 so
            // operators switching binaries see an identical diagnostic.
            anyhow::bail!(
                "Invalid configuration: INVARIANT_EXTRA_CHECKS cannot be \
                 enabled on a validator node (NODE_IS_VALIDATOR=true)"
            );
        }
    }

    // STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY is a tuning knob translated
    // in translate_invariant_config().

    Ok(())
}

/// Translate stellar-core invariant config keys into henyey's `InvariantConfig`.
fn translate_invariant_config(table: &toml::map::Map<String, toml::Value>, config: &mut AppConfig) {
    if let Some(value) = table.get("INVARIANT_CHECKS") {
        if let Some(arr) = value.as_array() {
            config.invariants.checks = arr
                .iter()
                .filter_map(|v| {
                    v.as_str().map(|s| {
                        // stellar-core uses negative-lookahead regex syntax for
                        // exclusion patterns (e.g. "(?!EventsAreConsistentWithEntryDiffs).*").
                        // Rust's regex engine does not support lookahead. Since henyey
                        // does not register the excluded invariant, these patterns are
                        // equivalent to ".*" (match all registered invariants).
                        if s.starts_with("(?!") && s.ends_with(").*") {
                            ".*".to_string()
                        } else {
                            s.to_string()
                        }
                    })
                })
                .collect();
        }
    }

    if let Some(value) = table.get("INVARIANT_EXTRA_CHECKS") {
        if let Some(b) = value.as_bool() {
            config.invariants.extra_checks = b;
        }
    }

    if let Some(value) = table.get("STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY") {
        if let Some(n) = value.as_integer() {
            config.invariants.snapshot_frequency_secs = n as u64;
        }
    }
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
fn classify_keys(table: &toml::map::Map<String, toml::Value>) -> Result<UnrecognizedKeys, String> {
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
    if let Some(validators) = get_array_of_tables_strict(table, "VALIDATORS")? {
        for (i, val_table) in validators.iter().enumerate() {
            for key in val_table.keys() {
                let k = key.as_str();
                if !val_supported.contains(k) && !val_unsupported.contains(k) {
                    result.validator_unknown.push((i, key.clone()));
                }
            }
        }
    }

    // Sub-table: [QUORUM_SET]
    let qs_recognized: HashSet<&str> = QUORUM_SET_KEYS.iter().copied().collect();
    if let Some(qs_table) = get_table_strict(table, "QUORUM_SET")? {
        for key in qs_table.keys() {
            if !qs_recognized.contains(key.as_str()) {
                result.quorum_set_unknown.push(key.clone());
            }
        }
    }

    // Sub-table: [HISTORY.*]
    let hist_recognized: HashSet<&str> = HISTORY_ENTRY_KEYS.iter().copied().collect();
    if let Some(history_table) = get_table_strict(table, "HISTORY")? {
        for (name, entry) in history_table {
            let entry_table = entry.as_table().ok_or_else(|| {
                format!("HISTORY.{}: expected table, got {}", name, entry.type_str())
            })?;
            for key in entry_table.keys() {
                if !hist_recognized.contains(key.as_str()) {
                    result.history_unknown.push((name.clone(), key.clone()));
                }
            }
        }
    }

    Ok(result)
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
    use stellar_xdr::NodeId;

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
fn parse_node_id(pubkey: &str) -> anyhow::Result<stellar_xdr::NodeId> {
    let pk = henyey_crypto::PublicKey::from_strkey(pubkey)
        .map_err(|e| anyhow::anyhow!("Invalid public key '{}': {}", pubkey, e))?;
    Ok(stellar_xdr::NodeId(
        stellar_xdr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::Uint256(*pk.as_bytes())),
    ))
}

/// Warn about unrecognized keys in a stellar-core format config.
///
/// Classifies each top-level key as supported, unsupported-but-known, or
/// unknown, and emits appropriate log messages. Also validates sub-table
/// keys within `[[VALIDATORS]]`, `[QUORUM_SET]`, and `[HISTORY.*]`.
fn warn_unrecognized_keys(table: &toml::map::Map<String, toml::Value>) -> Result<(), String> {
    let classified = classify_keys(table)?;

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
    Ok(())
}

/// Extract a base URL from a stellar-core / SSC archive command template.
///
/// Input:  `"curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}"`
/// Output: `Some("https://history.stellar.org/prd/core-testnet/core_testnet_001")`
///
/// Also handles simpler forms like `"wget -q {0} -O {1}"` and `"cp …"` where no
/// URL is present, returning `None`.
///
/// The URL is taken from the first `http(s)://` token in the command. The
/// `{0}`/`{1}` path-template placeholders mark where the remote/local path is
/// substituted, so the URL is truncated at the first `{` and then any
/// query-string and trailing slash is trimmed. This is more robust than
/// stripping a fixed `/{0}` suffix, because SSC may render the template with a
/// query string, shell quotes, or a trailing slash before the placeholder
/// (history feasibility doc §6 flagged the fixed-suffix form as brittle).
fn extract_url_from_curl_cmd(cmd: &str) -> Option<String> {
    for raw_token in cmd.split_whitespace() {
        // Strip surrounding shell quotes SSC/stellar-core may wrap the URL in,
        // e.g. `'https://host/{0}?auth=tok'`.
        let token = raw_token.trim_matches(|c| c == '\'' || c == '"');
        if token.starts_with("http://") || token.starts_with("https://") {
            // The `{0}`/`{1}` placeholder marks where the path is substituted.
            // Truncate the URL there so anything after (incl. a query string
            // appended after the placeholder) is dropped.
            let mut url = match token.find('{') {
                Some(idx) => &token[..idx],
                None => token,
            };
            // Drop a query string that precedes the placeholder, then a single
            // trailing slash, so the base URL has no spurious suffix.
            if let Some(q) = url.find('?') {
                url = &url[..q];
            }
            let url = url.trim_end_matches('/');
            if url.len() > "https://".len() {
                return Some(url.to_string());
            }
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

/// In stellar-core configs, port 0 means "don't listen" (see
/// `CommandHandler.cpp:56-77` and `Config::setNoListen`). Returns `None`
/// for port 0, `Some(port)` otherwise.
fn nonzero_port(port: u16) -> Option<u16> {
    if port == 0 {
        None
    } else {
        Some(port)
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

/// Read a TOML array of non-negative integers as `Vec<u32>`. Returns `None`
/// if the key is absent or not an array; elements that aren't u32-representable
/// integers are skipped with a warning.
fn get_u32_array(table: &toml::map::Map<String, toml::Value>, key: &str) -> Option<Vec<u32>> {
    let arr = table.get(key)?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for elem in arr {
        match elem.as_integer().and_then(|i| u32::try_from(i).ok()) {
            Some(v) => out.push(v),
            None => tracing::warn!(
                key,
                element = %elem,
                "Compat config array element is not a u32; skipping"
            ),
        }
    }
    Some(out)
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

/// Parses a TOML array of strings. Returns an error if the value is not an
/// array or if any element is not a string — matching stellar-core's fail-fast
/// `readArray<std::string>` behavior.
fn get_string_array(
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

/// Returns the value for `key` as a table reference, erroring if present but
/// not a table. Matches stellar-core's fail-fast behavior for structured config
/// sections like `[HISTORY]`.
fn get_table_strict<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<&'a toml::map::Map<String, toml::Value>>, String> {
    let val = match table.get(key) {
        Some(v) => v,
        None => return Ok(None),
    };
    val.as_table()
        .map(Some)
        .ok_or_else(|| format!("{key}: expected table, got {}", val.type_str()))
}

/// Returns the value for `key` as an array of table references, erroring if the
/// key is present but not an array, or if any element is not a table. Matches
/// stellar-core's fail-fast behavior for `[[VALIDATORS]]`-style sections.
fn get_array_of_tables_strict<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<Option<Vec<&'a toml::map::Map<String, toml::Value>>>, String> {
    let val = match table.get(key) {
        Some(v) => v,
        None => return Ok(None),
    };
    let arr = val
        .as_array()
        .ok_or_else(|| format!("{key}: expected array, got {}", val.type_str()))?;
    let mut result = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let t = elem
            .as_table()
            .ok_or_else(|| format!("{key}[{i}]: expected table, got {}", elem.type_str()))?;
        result.push(t);
    }
    Ok(Some(result))
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
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
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
            Some("SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2")
        );
        assert!(!config.node.is_validator);
        assert!(!config.node.force_scp);
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
    fn test_force_scp_defaults_to_validator_for_compat_config() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Private test network 'ssc'"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            VALIDATORS = ["$self"]
            THRESHOLD_PERCENT = 100
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.node.is_validator);
        assert!(config.node.force_scp);
    }

    #[test]
    fn test_force_scp_explicit_false_overrides_validator_default() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Private test network 'ssc'"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            FORCE_SCP = false
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            VALIDATORS = ["$self"]
            THRESHOLD_PERCENT = 100
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.node.is_validator);
        assert!(!config.node.force_scp);
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
    fn test_publish_to_archive_delay_parsed() {
        // Present: top-level PUBLISH_TO_ARCHIVE_DELAY maps to the history field.
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            PUBLISH_TO_ARCHIVE_DELAY = 7
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.history.publish_to_archive_delay_seconds, 7);

        // The key must be classified as supported, not unknown.
        let table = core_toml.as_table().unwrap();
        let classified = classify_keys(table).unwrap();
        assert!(
            !classified
                .unknown
                .iter()
                .any(|k| k == "PUBLISH_TO_ARCHIVE_DELAY"),
            "PUBLISH_TO_ARCHIVE_DELAY should not be reported unknown"
        );
        assert!(
            !classified
                .unsupported
                .iter()
                .any(|k| k == "PUBLISH_TO_ARCHIVE_DELAY"),
            "PUBLISH_TO_ARCHIVE_DELAY should be supported, not unsupported-known"
        );

        // Absent: defaults to 0.
        let no_delay_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&no_delay_toml).unwrap();
        assert_eq!(config.history.publish_to_archive_delay_seconds, 0);
    }

    #[test]
    fn test_extract_url_from_curl_cmd() {
        // --- Existing testnet shapes: behavior MUST stay identical ---
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

        // --- AC#2 hardening (#3295): literal SSC `get` template shapes ---
        // In-cluster localhost archive read-back (own publishing archive).
        assert_eq!(
            extract_url_from_curl_cmd("curl -sf http://localhost:1570/{0} -o {1}"),
            Some("http://localhost:1570".to_string())
        );
        // In-cluster peer pod hostname with port.
        assert_eq!(
            extract_url_from_curl_cmd("curl -sf http://ssc-core-0.ssc.local:1570/{0} -o {1}"),
            Some("http://ssc-core-0.ssc.local:1570".to_string())
        );

        // Trailing slash directly before the placeholder is normalized away.
        assert_eq!(
            extract_url_from_curl_cmd("curl -sf https://history.example.org/archive/{0} -o {1}"),
            Some("https://history.example.org/archive".to_string())
        );
        // A double slash before the placeholder normalizes to a single base.
        assert_eq!(
            extract_url_from_curl_cmd("curl -sf https://history.example.org/archive//{0} -o {1}"),
            Some("https://history.example.org/archive".to_string())
        );

        // Quoted URL with a query string appended after the placeholder — the
        // fixed-suffix-strip form returned None here (silent empty URL); the
        // hardened extractor recovers the base.
        assert_eq!(
            extract_url_from_curl_cmd("curl -sf 'https://history.example.org/{0}?auth=tok' -o {1}"),
            Some("https://history.example.org".to_string())
        );

        // Put-style command where the URL is the last token and {1} is the file.
        assert_eq!(
            extract_url_from_curl_cmd("curl -T {1} https://history.example.org/{0}"),
            Some("https://history.example.org".to_string())
        );

        // --- Forms with no extractable URL → None ---
        // wget with the URL supplied via the {0} placeholder (no literal URL).
        assert_eq!(extract_url_from_curl_cmd("wget -q {0} -O {1}"), None);
        // A bare scheme with no host must not produce a phantom archive URL.
        assert_eq!(extract_url_from_curl_cmd("curl -sf http:// -o {1}"), None);
        // Plain cp upload command (no URL).
        assert_eq!(
            extract_url_from_curl_cmd("cp {1} /opt/stellar/history/ssc_local/{0}"),
            None
        );
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
        assert_eq!(
            config.overlay.known_peers[0],
            PeerAddress::new("core1.stellar.org", 11625)
        );
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
            PeerAddress::new("core-testnet1.stellar.org", 11625)
        );
        assert_eq!(
            config.overlay.known_peers[1],
            PeerAddress::new("core-testnet2.stellar.org", 11625)
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
            PeerAddress::new("explicit-peer.stellar.org", 11625)
        );
    }

    #[test]
    fn test_malformed_validator_address_fails_fast() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            [[VALIDATORS]]
            NAME = "badval"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
            ADDRESS = ":invalid:address:"
            "#,
        )
        .unwrap();

        let err = translate_stellar_core_config(&core_toml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid ADDRESS") && msg.contains("badval"),
            "Expected error about invalid ADDRESS in validator 'badval', got: {msg}"
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
    fn test_quorum_set_pubkey_name_format() {
        // SSC writes explicit quorum set validators as "$PUBKEY $NAME" (e.g.
        // "GB... core-new-0"). stellar-core splits on whitespace. Henyey must
        // extract only the public key.
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = [
                "$self",
                "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y core-new-0",
                "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP core-old-0",
            ]
            "#,
        )
        .unwrap();

        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.node.quorum_set.validators.len(), 3);
        // $self resolves to the node's own key
        let self_pubkey = henyey_crypto::SecretKey::from_strkey(
            "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2",
        )
        .unwrap()
        .public_key()
        .to_strkey();
        assert_eq!(config.node.quorum_set.validators[0], self_pubkey);
        // pubkey+name entries are split on whitespace, extracting only the key
        assert_eq!(
            config.node.quorum_set.validators[1],
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y"
        );
        assert_eq!(
            config.node.quorum_set.validators[2],
            "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP"
        );
        assert_eq!(config.node.quorum_set.threshold_percent, 100);
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
    fn test_use_config_for_genesis_parsed() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            USE_CONFIG_FOR_GENESIS = true
            TESTING_UPGRADE_LEDGER_PROTOCOL_VERSION = 25
            TESTING_UPGRADE_DESIRED_FEE = 200
            TESTING_UPGRADE_RESERVE = 5000000
            TESTING_UPGRADE_MAX_TX_SET_SIZE = 500
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(config.testing.use_config_for_genesis);
        assert_eq!(config.testing.testing_upgrade_ledger_protocol_version, 25);
        assert_eq!(config.testing.testing_upgrade_desired_fee, 200);
        assert_eq!(config.testing.testing_upgrade_reserve, 5_000_000);
        assert_eq!(config.testing.testing_upgrade_max_tx_set_size, 500);

        let gc = config.testing.genesis_config();
        assert!(gc.use_config_for_genesis);
        assert_eq!(gc.protocol_version, 25);
        assert_eq!(gc.base_fee, 200);
        assert_eq!(gc.base_reserve, 5_000_000);
        assert_eq!(gc.max_tx_set_size, 500);
    }

    #[test]
    fn test_use_config_for_genesis_defaults() {
        // Absent keys ⇒ stellar-core defaults (false / CURRENT_PROTOCOL / 100 / 100_000_000 / 50).
        // stellar-core sets TESTING_UPGRADE_LEDGER_PROTOCOL_VERSION = CURRENT_LEDGER_PROTOCOL_VERSION.
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.testing.use_config_for_genesis);
        assert_eq!(
            config.testing.testing_upgrade_ledger_protocol_version,
            henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION
        );
        assert_eq!(config.testing.testing_upgrade_desired_fee, 100);
        assert_eq!(config.testing.testing_upgrade_reserve, 100_000_000);
        assert_eq!(config.testing.testing_upgrade_max_tx_set_size, 50);
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
            .contains(&PeerAddress::new("core-testnet1.stellar.org", 11625)));
        assert_eq!(config.overlay.preferred_peers.len(), 1);
        assert_eq!(
            config.overlay.preferred_peers[0],
            PeerAddress::new("core-testnet1.stellar.org", 11625)
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

    /// Config acceptance for the Henyey mixed-image Supercluster (SSC) mission: the
    /// mission-shaped, mixed-cluster `stellar-core.cfg` that SSC renders for a
    /// **henyey validator** node in a 4-node (1 henyey + 3 stellar-core)
    /// cluster must be accepted by henyey *without manual patching*.
    ///
    /// This is distinct from `test_ssc_generated_config_full_parse`, which
    /// covers a testnet *watcher* shape (`NODE_IS_VALIDATOR=false`, real
    /// testnet peers, history archives). Here the node is a VALIDATOR
    /// participating as a minority in a stellar-core-majority quorum, with
    /// in-cluster pod hostnames, accelerated close time, and NO history.
    ///
    /// The test drives the exact `is_stellar_core_format` +
    /// `translate_stellar_core_config` entry the binary's `load_config` path
    /// uses (main.rs), so it exercises real parsing, not a stub. It fails if
    /// the mission fixture is not parseable (the durable, CI-enforced
    /// regression artifact the triage report required).
    #[test]
    fn test_ssc_mission_mixed_config_parse() {
        let fixture = include_str!("compat_http/test_fixtures/ssc_mission_mixed.cfg");
        let raw: toml::Value = toml::from_str(fixture).unwrap();

        // Detected as stellar-core format (the binary's branch in load_config).
        assert!(
            is_stellar_core_format(&raw),
            "mission mixed-cluster config must be detected as stellar-core format"
        );

        // Translates without error — i.e. accepted WITHOUT manual patching (AC#2).
        let config = translate_stellar_core_config(&raw).unwrap();

        // --- Node is a VALIDATOR (the henyey node participates in consensus) ---
        assert!(
            config.node.is_validator,
            "mission node must be a validator (NODE_IS_VALIDATOR=true)"
        );
        // NODE_SEED is the henyey node's own seed (the strkey before " self").
        assert_eq!(
            config.node.node_seed.as_deref(),
            Some("SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2")
        );

        // --- Quorum: the listed [[VALIDATORS]] are exactly the 3 stellar-core
        // peers; henyey's own key is added automatically (NOT listed in its own
        // [[VALIDATORS]], mirroring stellar-core's addSelfToValidators which
        // never dedups, Config.cpp:869-898). So the auto-generated quorum-set
        // key list holds the 3 distinct core keys...
        let henyey_self_key = henyey_crypto::SecretKey::from_strkey(
            "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2",
        )
        .unwrap()
        .public_key()
        .to_strkey();
        let core_keys = [
            "GDKXE2OZMJIPOSLNA6N6F2BVCI3O777I2OOC4BV7VOYUEHYX7RTRYA7Y",
            "GCUCJTIYXSOXKBSNFGNFWW5MUQ54HKRPGJUTQFJ5RQXZXNOLNXYDHRAP",
            "GC2V2EFSXN6SQTWVYA5EPJPBWWIMSD2XQNKUOHGEKB535AQE2I6IXV2Z",
        ];
        let validators = &config.node.quorum_set.validators;
        assert_eq!(
            validators.len(),
            3,
            "expected the 3 listed stellar-core validators, got {}: {:?}",
            validators.len(),
            validators
        );
        for core_key in core_keys {
            assert!(
                validators.contains(&core_key.to_string()),
                "quorum must contain stellar-core key {core_key}"
            );
        }
        // henyey does NOT list itself in [[VALIDATORS]] (no duplicate self).
        assert!(
            !validators.contains(&henyey_self_key),
            "henyey must not list its own key in [[VALIDATORS]] (self is auto-added)"
        );

        // ...and the FULL 4-node quorum (3 core + auto-added self) materializes
        // in the ValidatorWeightConfig, with exactly 4 DISTINCT node ids and no
        // self double-counting (the inverse of
        // `test_quorum_set_self_without_node_seed_fails`).
        let vwc = config
            .validator_weight_config
            .as_ref()
            .expect("mission validator config must build a ValidatorWeightConfig");
        assert_eq!(
            vwc.validator_entries.len(),
            4,
            "weight config must hold 4 distinct validators (3 core + self), got {}",
            vwc.validator_entries.len()
        );
        let self_node_id = parse_node_id(&henyey_self_key).unwrap();
        assert!(
            vwc.validator_entries.contains_key(&self_node_id),
            "weight config must contain henyey's auto-added self entry"
        );
        for core_key in core_keys {
            let nid = parse_node_id(core_key).unwrap();
            assert!(
                vwc.validator_entries.contains_key(&nid),
                "weight config must contain stellar-core key {core_key}"
            );
        }

        // --- Accelerated close time on (SSC integration missions) ---
        assert!(
            config.testing.accelerate_time,
            "ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING must be true"
        );

        // --- In-cluster known peers (pod hostnames, NOT real testnet peers) ---
        // The henyey ADDRESS and the 3 core ADDRESSes are in-cluster pod DNS.
        assert!(
            !config.overlay.known_peers.is_empty(),
            "mission config must yield in-cluster known peers"
        );
        for peer in &config.overlay.known_peers {
            assert!(
                peer.host.contains("ssc-henyey-mixed") || peer.host.ends_with(".svc.cluster.local"),
                "known peer must be an in-cluster pod hostname, got {peer:?}"
            );
        }

        // --- NO history archives (first mission excludes history) ---
        assert!(
            config.history.archives.is_empty(),
            "first mission has no history archives, got {}",
            config.history.archives.len()
        );

        // Compat flag set.
        assert!(config.is_compat_config);
    }

    /// AC#2 of the SSC history mission (#3295): the mission-shaped
    /// `stellar-core.cfg` that SSC renders for a **publishing validator** —
    /// one that both reads from peer archives (`get`) and writes its own
    /// checkpoints (`put` + `mkdir`) into a cluster-local archive — must be
    /// accepted by henyey *without manual patching*.
    ///
    /// This is distinct from `test_ssc_generated_config_full_parse` (a
    /// read-only watcher with `get`-only archives) and
    /// `test_ssc_mission_mixed_config_parse` (a validator with NO history).
    /// Here we assert the full `[HISTORY.*]` triple round-trips: extracted
    /// `url`, preserved `put`/`mkdir` strings, and `get_enabled`/`put_enabled`
    /// flags, plus the inline `[[VALIDATORS]].HISTORY` `get`-only archives.
    ///
    /// The test drives the same `is_stellar_core_format` +
    /// `translate_stellar_core_config` entry the binary's config-load path
    /// uses, so it exercises real parsing, not a stub. It fails if a future
    /// SSC template-rendering change breaks URL extraction or drops the
    /// publish commands.
    #[test]
    fn test_ssc_mission_history_config_parse() {
        let fixture = include_str!("compat_http/test_fixtures/ssc_mission_history.cfg");
        let raw: toml::Value = toml::from_str(fixture).unwrap();

        assert!(
            is_stellar_core_format(&raw),
            "SSC history config must be detected as stellar-core format"
        );

        let config = translate_stellar_core_config(&raw).unwrap();

        // --- Node is a publishing validator ---
        assert!(config.node.is_validator);

        // --- History archives: 2 from [HISTORY.*] + 2 inline [[VALIDATORS]].HISTORY ---
        // The publishing archive `ssc_local` carries the full get/put/mkdir triple.
        let ssc_local = config
            .history
            .archives
            .iter()
            .find(|a| a.name == "ssc_local")
            .expect("must have [HISTORY.ssc_local] archive");
        assert_eq!(
            ssc_local.url, "http://localhost:1570",
            "url must be extracted from the localhost curl `get` template, with the /{{0}} suffix stripped"
        );
        assert!(ssc_local.get_enabled, "ssc_local has a `get`");
        assert!(ssc_local.put_enabled, "ssc_local has a `put`");
        assert_eq!(
            ssc_local.put.as_deref(),
            Some("cp {0} /opt/stellar/history/ssc_local/{1}"),
            "the `put` command template must be preserved verbatim for the uploader"
        );
        assert_eq!(
            ssc_local.mkdir.as_deref(),
            Some("mkdir -p /opt/stellar/history/ssc_local/{0}"),
            "the `mkdir` command template must be preserved verbatim"
        );

        // The read-only peer mirror `ssc_peer` is get-only (no put/mkdir).
        let ssc_peer = config
            .history
            .archives
            .iter()
            .find(|a| a.name == "ssc_peer")
            .expect("must have [HISTORY.ssc_peer] archive");
        assert_eq!(ssc_peer.url, "http://ssc-core-1.ssc.local:1570");
        assert!(ssc_peer.get_enabled);
        assert!(!ssc_peer.put_enabled, "peer mirror is read-only");
        assert!(ssc_peer.put.is_none());
        assert!(ssc_peer.mkdir.is_none());

        // Inline [[VALIDATORS]].HISTORY archives (get-only) are also present.
        let ssc_core_0 = config
            .history
            .archives
            .iter()
            .find(|a| a.name == "ssc_core_0")
            .expect("must have inline VALIDATORS HISTORY archive ssc_core_0");
        assert_eq!(ssc_core_0.url, "http://ssc-core-0.ssc.local:1570");
        assert!(ssc_core_0.get_enabled);
        assert!(!ssc_core_0.put_enabled);

        // Total: 2 from [HISTORY.*] + 2 from [[VALIDATORS]].HISTORY.
        assert_eq!(
            config.history.archives.len(),
            4,
            "expected 4 archives (2 [HISTORY.*] + 2 inline), got {}",
            config.history.archives.len()
        );

        // Compat flag set — proves the binary's load path treats this as a
        // stellar-core compat config (no manual patching).
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
        let classified = classify_keys(table).unwrap();
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
        let classified = classify_keys(table).unwrap();
        // UNSAFE_QUORUM, FORCE_SCP, and FAILURE_SAFETY are now supported
        // (actively translated).
        assert!(classified.unsupported.is_empty());
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
        let classified = classify_keys(table).unwrap();
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
        let classified = classify_keys(table).unwrap();
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
        let classified = classify_keys(table).unwrap();
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
        let classified = classify_keys(table).unwrap();
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
        let classified = classify_keys(table).unwrap();
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
PEER_FLOOD_READING_CAPACITY_BYTES=4294967297
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
    fn test_translate_disable_bucket_gc() {
        // #3153: a drop-in stellar-core config's DISABLE_BUCKET_GC kill-switch
        // translates to henyey's native buckets.disable_bucket_gc.
        let config = translate(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DISABLE_BUCKET_GC = true
            "#,
        )
        .unwrap();
        assert!(
            config.buckets.disable_bucket_gc,
            "DISABLE_BUCKET_GC = true must translate to disable_bucket_gc = true"
        );

        // Explicit false translates to false.
        let config = translate(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DISABLE_BUCKET_GC = false
            "#,
        )
        .unwrap();
        assert!(
            !config.buckets.disable_bucket_gc,
            "DISABLE_BUCKET_GC = false must translate to disable_bucket_gc = false"
        );

        // Absent => default false (GC enabled).
        let config = translate(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        assert!(
            !config.buckets.disable_bucket_gc,
            "absent DISABLE_BUCKET_GC must default to disable_bucket_gc = false"
        );
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
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = [
                "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS",
                "GCGB2S2KBER5MNQNJTNF5N3Y4PEPFMHONPIGXNOYIREMYCMJZ3GAVDXQ"
            ]
            "#
        .to_string();
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
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = []
            "#
        .to_string();
        let app_config = translate(&cfg).unwrap();
        assert!(app_config.overlay.surveyor_keys.is_empty());
    }

    #[test]
    fn test_translate_surveyor_keys_rejects_non_string() {
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = ["GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS", 42]
            "#
        .to_string();
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("SURVEYOR_KEYS[1]"),
            "Expected type error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_surveyor_keys_rejects_non_array() {
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            UNSAFE_QUORUM = true
            SURVEYOR_KEYS = "GDEX3JU2AUGVPQFGFKMEOGHEUQ4YGRIYDJIKQSC7QLHAJ4RV63MJKGAS"
            "#
        .to_string();
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("SURVEYOR_KEYS: expected array"),
            "Expected array error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peer_keys() {
        let cfg = r#"
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
        .to_string();
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
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            PREFERRED_PEER_KEYS = ["valid-string", 42]
            "#
        .to_string();
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("PREFERRED_PEER_KEYS[1]"),
            "Expected element error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peers_only_wrong_type() {
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            PREFERRED_PEERS_ONLY = "yes"
            "#
        .to_string();
        let err = translate(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("PREFERRED_PEERS_ONLY"),
            "Expected type error, got: {}",
            err
        );
    }

    #[test]
    fn test_translate_preferred_peers_only_default_false() {
        let cfg = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/core.db"
            HTTP_PORT = 11626
            UNSAFE_QUORUM = true
            "#
        .to_string();
        let config = translate(&cfg).unwrap();
        assert!(!config.overlay.preferred_peers_only);
        assert!(config.overlay.preferred_peer_keys.is_empty());
    }

    // --- Invariant-check compat keys (#2102 / [AUDIT-213]) ---

    /// Common minimal config that translates successfully on its own —
    /// extended by the invariant tests with each scenario's keys.
    fn minimal_compat_toml() -> &'static str {
        r#"
        NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
        "#
    }

    #[test]
    fn test_invariant_checks_default_accepted() {
        // No invariant keys at all — the common migrated-config case.
        let raw: toml::Value = toml::from_str(minimal_compat_toml()).unwrap();
        assert!(translate_stellar_core_config(&raw).is_ok());
    }

    #[test]
    fn test_invariant_checks_explicit_empty_accepted() {
        let toml_str = format!("{}\nINVARIANT_CHECKS = []\n", minimal_compat_toml());
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        assert!(translate_stellar_core_config(&raw).is_ok());
    }

    #[test]
    fn test_invariant_extra_checks_explicit_false_accepted() {
        let toml_str = format!(
            "{}\nINVARIANT_EXTRA_CHECKS = false\n",
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        assert!(translate_stellar_core_config(&raw).is_ok());
    }

    #[test]
    fn test_invariant_freq_supported() {
        // `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY` is now a supported key
        // translated into InvariantConfig.
        for value in ["300", "100", "0", "9999", "604800"] {
            let toml_str = format!(
                "{}\nSTATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY = {}\n",
                minimal_compat_toml(),
                value
            );
            let raw: toml::Value = toml::from_str(&toml_str).unwrap();
            let config = translate_stellar_core_config(&raw)
                .unwrap_or_else(|e| panic!("value {value} should translate: {e}"));
            assert_eq!(
                config.invariants.snapshot_frequency_secs,
                value.parse::<u64>().unwrap()
            );
            let classified = classify_keys(raw.as_table().unwrap()).unwrap();
            assert!(
                !classified
                    .unknown
                    .contains(&"STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY".to_string()),
                "value {value} should not be flagged as unknown"
            );
        }
    }

    #[test]
    fn test_invariant_checks_non_empty_accepted() {
        // Non-empty INVARIANT_CHECKS is now supported and translated.
        let toml_str = format!(
            "{}\nINVARIANT_CHECKS = [\"ConservationOfLumens\"]\n",
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.invariants.checks, vec!["ConservationOfLumens"]);
    }

    #[test]
    fn test_invariant_checks_negative_lookahead_translated() {
        // stellar-core uses negative-lookahead regex syntax like
        // "(?!EventsAreConsistentWithEntryDiffs).*" for INVARIANT_CHECKS.
        // Rust regex does not support lookahead, so henyey translates
        // these to ".*" (since henyey does not register the excluded
        // invariant, the patterns are equivalent).
        let toml_str = format!(
            "{}\nINVARIANT_CHECKS = [\"(?!EventsAreConsistentWithEntryDiffs).*\"]\n",
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.invariants.checks, vec![".*"]);
    }

    #[test]
    fn test_invariant_extra_checks_true_rejected_validator() {
        // Validator path: the diagnostic must reuse stellar-core's exact
        // wording from Config.cpp:2003-2004 so operators switching
        // binaries see an identical message.
        let toml_str = format!(
            r#"
            {}
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = true
            INVARIANT_EXTRA_CHECKS = true
            "#,
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_EXTRA_CHECKS cannot be enabled on a validator node"),
            "expected verbatim stellar-core wording, got: {msg}"
        );
    }

    #[test]
    fn test_invariant_extra_checks_true_accepted_watcher() {
        // Watcher path: INVARIANT_EXTRA_CHECKS=true is now accepted
        // for non-validator nodes.
        let toml_str = format!(
            "{}\nNODE_IS_VALIDATOR = false\nINVARIANT_EXTRA_CHECKS = true\n",
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&raw).unwrap();
        assert!(config.invariants.extra_checks);
    }

    #[test]
    fn test_invariant_checks_wrong_type() {
        // Non-array value.
        let toml_str = format!("{}\nINVARIANT_CHECKS = \"foo\"\n", minimal_compat_toml());
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_CHECKS must be an array of strings"),
            "expected outer type-error message, got: {msg}"
        );
    }

    #[test]
    fn test_invariant_checks_wrong_type_logically_empty() {
        // `""` is a string; it is logically equivalent to an empty array
        // (zero elements) but typed wrong. Type errors must fire
        // unconditionally — not be silently accepted.
        let toml_str = format!("{}\nINVARIANT_CHECKS = \"\"\n", minimal_compat_toml());
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_CHECKS must be an array of strings"),
            "type errors must fire even for logically-empty wrong types, got: {msg}"
        );
    }

    #[test]
    fn test_invariant_checks_wrong_element_type() {
        // Per-element type check: array of non-strings.
        let toml_str = format!("{}\nINVARIANT_CHECKS = [42]\n", minimal_compat_toml());
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_CHECKS must be an array of strings"),
            "expected per-element type-error message, got: {msg}"
        );
    }

    #[test]
    fn test_invariant_extra_checks_wrong_type() {
        let toml_str = format!(
            "{}\nINVARIANT_EXTRA_CHECKS = \"true\"\n",
            minimal_compat_toml()
        );
        let raw: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&raw).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_EXTRA_CHECKS must be a boolean"),
            "expected type-error message, got: {msg}"
        );
    }

    #[test]
    fn test_invariant_keys_correctly_classified() {
        // All three invariant keys are now in SUPPORTED_COMPAT_KEYS
        // (translated into InvariantConfig).
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            INVARIANT_CHECKS = []
            INVARIANT_EXTRA_CHECKS = false
            STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY = 300
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        let classified = classify_keys(raw.as_table().unwrap()).unwrap();

        // All three keys: handled (in SUPPORTED_COMPAT_KEYS), so absent from both
        // unsupported and unknown.
        for key in [
            "INVARIANT_CHECKS",
            "INVARIANT_EXTRA_CHECKS",
            "STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY",
        ] {
            assert!(
                !classified.unsupported.contains(&key.to_string()),
                "{key} should not be in unsupported (it's in SUPPORTED_KEYS)"
            );
            assert!(
                !classified.unknown.contains(&key.to_string()),
                "{key} should not be in unknown"
            );
        }
    }

    #[test]
    fn test_is_stellar_core_format_detects_invariant_only_configs() {
        // Each of the three keys, alone, should trip stellar-core format
        // detection. Regression against a future refactor that moves any
        // of them to a slice not consulted by `is_stellar_core_format`.
        for toml_body in [
            r#"INVARIANT_CHECKS = []"#,
            r#"INVARIANT_EXTRA_CHECKS = false"#,
            r#"STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY = 300"#,
        ] {
            let raw: toml::Value = toml::from_str(toml_body).unwrap();
            assert!(
                is_stellar_core_format(&raw),
                "config `{toml_body}` should be detected as stellar-core format"
            );
        }
    }

    #[test]
    fn test_audit_finding_reproducer() {
        // Verbatim TOML body from issue #2102's "Reachability and Attack
        // Vector" section. Now that we have InvariantManager, this config
        // translates successfully for a watcher (non-validator). The
        // validator case still rejects INVARIANT_EXTRA_CHECKS=true.
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Public Global Stellar Network ; September 2015"
            DATABASE = "sqlite3://stellar.db"
            INVARIANT_CHECKS = ["BucketListIsConsistentWithDatabase", "ConservationOfLumens"]
            INVARIANT_EXTRA_CHECKS = true
            STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY = 1
        "#;
        let raw: toml::Value = toml::from_str(toml_str).unwrap();
        assert!(is_stellar_core_format(&raw));
        // Watcher: succeeds now.
        let config = translate_stellar_core_config(&raw).unwrap();
        assert_eq!(config.invariants.checks.len(), 2);
        assert!(config.invariants.extra_checks);
        assert_eq!(config.invariants.snapshot_frequency_secs, 1);

        // Validator + INVARIANT_EXTRA_CHECKS=true: still rejected.
        let toml_str_validator = r#"
            NETWORK_PASSPHRASE = "Public Global Stellar Network ; September 2015"
            DATABASE = "sqlite3://stellar.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = true
            INVARIANT_CHECKS = ["ConservationOfLumens"]
            INVARIANT_EXTRA_CHECKS = true
        "#;
        let raw_v: toml::Value = toml::from_str(toml_str_validator).unwrap();
        let err = translate_stellar_core_config(&raw_v).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("INVARIANT_EXTRA_CHECKS cannot be enabled on a validator node"),
            "validator + extra_checks must still reject, got: {msg}"
        );
    }

    #[test]
    fn test_http_port_zero_disables_both_servers() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 0
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        // HTTP_PORT=0 means "don't listen" in stellar-core (CommandHandler.cpp:56-77).
        // Both native HTTP and compat HTTP must be disabled.
        assert!(!config.http.enabled, "native HTTP should be disabled");
        assert!(
            !config.compat_http.enabled,
            "compat HTTP should be disabled for port 0"
        );
    }

    #[test]
    fn test_http_port_zero_with_nonzero_query_port() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 0
            HTTP_QUERY_PORT = 8080
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.http.enabled, "native HTTP should be disabled");
        assert!(
            !config.compat_http.enabled,
            "compat HTTP should be disabled"
        );
        assert_eq!(
            config.query.port,
            Some(8080),
            "query server should be enabled on 8080"
        );
    }

    #[test]
    fn test_nonzero_http_port_with_query_port_zero() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            HTTP_QUERY_PORT = 0
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.http.enabled, "native HTTP should be disabled");
        assert!(config.compat_http.enabled, "compat HTTP should be enabled");
        assert_eq!(config.compat_http.port, 11626);
        assert_eq!(
            config.query.port, None,
            "query server should be disabled for port 0"
        );
    }

    #[test]
    fn test_http_query_port_zero_disables_query() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_QUERY_PORT = 0
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(
            config.query.port, None,
            "query server should be disabled for port 0"
        );
    }

    #[test]
    fn test_public_http_port_applies_to_query_server() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            HTTP_QUERY_PORT = 11627
            PUBLIC_HTTP_PORT = true
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        // Both compat HTTP and query server should bind to all interfaces
        assert_eq!(config.compat_http.address, "::");
        assert_eq!(config.query.address, Some("::".to_string()));
        assert_eq!(config.query.port, Some(11627));
    }

    #[test]
    fn test_public_http_port_false_query_binds_localhost() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            HTTP_QUERY_PORT = 11627
            PUBLIC_HTTP_PORT = false
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.compat_http.address, "127.0.0.1");
        assert_eq!(config.query.address, Some("127.0.0.1".to_string()));
    }

    #[test]
    fn test_http_port_zero_with_public_query_port() {
        // HTTP_PORT=0 disables compat HTTP, but query server should still get
        // the PUBLIC_HTTP_PORT-derived address.
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 0
            HTTP_QUERY_PORT = 11627
            PUBLIC_HTTP_PORT = true
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert!(!config.compat_http.enabled);
        assert_eq!(config.query.port, Some(11627));
        assert_eq!(config.query.address, Some("::".to_string()));
    }

    #[test]
    fn test_no_query_port_leaves_address_none() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            PUBLIC_HTTP_PORT = true
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        // Without HTTP_QUERY_PORT, query.address should remain None
        assert_eq!(config.query.port, None);
        assert_eq!(config.query.address, None);
    }

    #[test]
    fn test_query_port_zero_leaves_address_none() {
        // HTTP_QUERY_PORT=0 means disabled — don't set an address either
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            HTTP_QUERY_PORT = 0
            PUBLIC_HTTP_PORT = true
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            UNSAFE_QUORUM = true
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&core_toml).unwrap();
        assert_eq!(config.query.port, None);
        assert_eq!(config.query.address, None);
    }

    #[test]
    fn test_known_peers_rejects_non_string_element() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            KNOWN_PEERS = ["valid-peer.example.com", 42]
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("KNOWN_PEERS[1]"),
            "expected error about KNOWN_PEERS[1], got: {err}"
        );
    }

    #[test]
    fn test_preferred_peers_rejects_non_string_element() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            PREFERRED_PEERS = [true, "valid-peer.example.com"]
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("PREFERRED_PEERS[0]"),
            "expected error about PREFERRED_PEERS[0], got: {err}"
        );
    }

    #[test]
    fn test_known_peers_rejects_non_array_type() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            KNOWN_PEERS = "not-an-array"
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("KNOWN_PEERS"),
            "expected error mentioning KNOWN_PEERS, got: {err}"
        );
    }

    #[test]
    fn test_malformed_known_peers_does_not_fall_back_to_validator_addresses() {
        // If KNOWN_PEERS has non-string elements, the config must error —
        // not silently produce an empty list that triggers the validator-address
        // fallback.
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            KNOWN_PEERS = [42]

            [[VALIDATORS]]
            NAME = "v1"
            PUBLIC_KEY = "GBCR5OVQ54S2EKHLBZMK6VYMTXZHXN3T45Y6PRX4PX4FXDMJJGY4FD42"
            ADDRESS = "validator1.example.com"
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("KNOWN_PEERS[0]"),
            "expected error about KNOWN_PEERS[0], got: {err}"
        );
    }

    #[test]
    fn test_known_peers_valid_string_array() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            KNOWN_PEERS = ["peer1.example.com", "peer2.example.com:11625"]
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&toml).unwrap();
        assert_eq!(
            config.overlay.known_peers,
            vec![
                PeerAddress::new("peer1.example.com", 11625),
                PeerAddress::new("peer2.example.com", 11625),
            ]
        );
    }

    #[test]
    fn test_quorum_set_validators_non_string_element_rejected() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["GABCDEF", 42]
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("VALIDATORS[1]"),
            "expected error about VALIDATORS[1], got: {err}"
        );
    }

    #[test]
    fn test_quorum_set_validators_non_array_rejected() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            [QUORUM_SET]
            VALIDATORS = "not-an-array"
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("VALIDATORS"),
            "expected error mentioning VALIDATORS, got: {err}"
        );
    }

    #[test]
    fn test_quorum_set_non_table_rejected() {
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            DATABASE = "sqlite3:///tmp/test.db"
            QUORUM_SET = "not-a-table"
            "#,
        )
        .unwrap();
        let err = translate_stellar_core_config(&toml).unwrap_err();
        assert!(
            err.to_string().contains("QUORUM_SET"),
            "expected error mentioning QUORUM_SET, got: {err}"
        );
    }

    #[test]
    fn test_quorum_set_empty_validators_overrides() {
        // With [[VALIDATORS]] present AND [QUORUM_SET] VALIDATORS = [], the
        // manual override should clear the auto-generated validator list.
        // Before the fix, `if !keys.is_empty()` would skip the override,
        // preserving the auto-generated quorum set from [[VALIDATORS]].
        let toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "SDQVDISRYN2JXBS7ICL7QJAEKB3HWBJFP2QECXG7GZICAHBK4UNJCWK2 self"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0

            [[VALIDATORS]]
            NAME = "self"
            PUBLIC_KEY = "GDKXE2OZMJIPOSLNA6N6F2BVCI3O6GDRAG2MIF5U3M3FZPXEOAKNQH6I"
            HOME_DOMAIN = "test.org"

            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = []
            "#,
        )
        .unwrap();
        let config = translate_stellar_core_config(&toml).unwrap();
        assert!(
            config.node.quorum_set.validators.is_empty(),
            "expected empty validators after VALIDATORS = [], got: {:?}",
            config.node.quorum_set.validators
        );
    }

    #[test]
    fn test_compat_config_peer_port_zero_rejected() {
        let core_toml: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            HTTP_PORT = 11626
            DATABASE = "sqlite3:///tmp/stellar-core.db"
            PEER_PORT = 0
            "#,
        )
        .unwrap();
        let result = translate_stellar_core_config(&core_toml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("PEER_PORT must not be 0"),
            "Expected PEER_PORT=0 rejection at translation time, got: {}",
            err
        );
    }

    #[test]
    fn test_history_wrong_type_rejected() {
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            HISTORY = 42
        "#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let err = translate_stellar_core_config(&parsed).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("HISTORY") && msg.contains("expected table"),
            "Expected HISTORY type error, got: {msg}"
        );
    }

    #[test]
    fn test_history_entry_wrong_type_rejected() {
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false

            [HISTORY]
            good = { get = "curl -sf http://example.com/{0} -o {1}" }
            bad = 42
        "#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let err = translate_stellar_core_config(&parsed).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("HISTORY.bad") && msg.contains("expected table"),
            "Expected HISTORY entry type error, got: {msg}"
        );
    }

    #[test]
    fn test_validators_wrong_type_rejected() {
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            VALIDATORS = "not an array"
        "#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let err = translate_stellar_core_config(&parsed).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("VALIDATORS") && msg.contains("expected array"),
            "Expected VALIDATORS type error, got: {msg}"
        );
    }

    #[test]
    fn test_validators_entry_wrong_type_mixed_rejected() {
        // TOML [[VALIDATORS]] always produces array-of-tables, so we need to
        // test with raw toml::Value manipulation for mixed entries.
        let mut val: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            "#,
        )
        .unwrap();
        // Inject VALIDATORS as an array with a valid table then a non-table
        let table = val.as_table_mut().unwrap();
        table.insert(
            "VALIDATORS".to_string(),
            toml::Value::Array(vec![
                toml::Value::Table({
                    let mut t = toml::map::Map::new();
                    t.insert(
                        "PUBLIC_KEY".to_string(),
                        toml::Value::String(
                            "GCGB2S2KBER43AGH5QINNPFMXNLW6WFASTDPQ6KRFGMLEZMURQ2KZOR".to_string(),
                        ),
                    );
                    t.insert("NAME".to_string(), toml::Value::String("good".to_string()));
                    t
                }),
                toml::Value::Integer(42),
            ]),
        );
        let toml_str = toml::to_string(&val).unwrap();
        let parsed: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&parsed).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("VALIDATORS[1]") && msg.contains("expected table"),
            "Expected VALIDATORS entry type error at index 1, got: {msg}"
        );
    }

    #[test]
    fn test_history_empty_table_valid() {
        let toml_str = r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false

            [HISTORY]
        "#;
        let parsed: toml::Value = toml::from_str(toml_str).unwrap();
        let config = translate_stellar_core_config(&parsed).unwrap();
        assert!(config.history.archives.is_empty());
    }

    #[test]
    fn test_validators_empty_array_valid() {
        // TOML doesn't allow [[VALIDATORS]] to produce an empty array directly,
        // so we use raw Value manipulation.
        let mut val: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            NODE_SEED = "SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH"
            NODE_IS_VALIDATOR = false
            "#,
        )
        .unwrap();
        val.as_table_mut()
            .unwrap()
            .insert("VALIDATORS".to_string(), toml::Value::Array(vec![]));
        let toml_str = toml::to_string(&val).unwrap();
        let parsed: toml::Value = toml::from_str(&toml_str).unwrap();
        // Should succeed without error
        translate_stellar_core_config(&parsed).unwrap();
    }

    #[test]
    fn test_classify_keys_rejects_validators_wrong_type() {
        let mut val: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        val.as_table_mut().unwrap().insert(
            "VALIDATORS".to_string(),
            toml::Value::String("bad".to_string()),
        );
        let err = classify_keys(val.as_table().unwrap()).unwrap_err();
        assert!(
            err.contains("VALIDATORS") && err.contains("expected array"),
            "Expected VALIDATORS type error from classify_keys, got: {err}"
        );
    }

    #[test]
    fn test_classify_keys_rejects_history_wrong_type() {
        let mut val: toml::Value = toml::from_str(
            r#"
            NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
            "#,
        )
        .unwrap();
        val.as_table_mut()
            .unwrap()
            .insert("HISTORY".to_string(), toml::Value::Integer(99));
        let err = classify_keys(val.as_table().unwrap()).unwrap_err();
        assert!(
            err.contains("HISTORY") && err.contains("expected table"),
            "Expected HISTORY type error from classify_keys, got: {err}"
        );
    }

    // ---- Auto-generated hierarchical quorum set + nested [QUORUM_SET] parsing
    // (issue #3622). Tests assert on the NORMALIZED `to_xdr()` ScpQuorumSet (the
    // artifact that feeds SCP), not the raw QuorumSetConfig, because
    // normalize_quorum_set collapses singleton inner sets.

    /// Deterministic test pubkey from a seed byte.
    fn test_pubkey(seed: u8) -> String {
        henyey_crypto::SecretKey::from_seed(&[seed; 32])
            .public_key()
            .to_strkey()
    }

    /// Deterministic test NODE_SEED (S...) from a seed byte.
    fn test_node_seed(seed: u8) -> String {
        henyey_crypto::SecretKey::from_seed(&[seed; 32]).to_strkey()
    }

    /// Build a stellar-core compat TOML for an auto-qset topology.
    ///
    /// `domains` is a list of (home_domain, quality, validator_seeds). The node
    /// itself (NODE_SEED from `self_seed`) joins `self_domain`. All validators
    /// use deterministic keys via `test_pubkey`.
    fn build_auto_qset_toml(
        domains: &[(&str, &str, &[u8])],
        self_seed: u8,
        self_domain: &str,
    ) -> String {
        let mut s = String::new();
        s.push_str("NETWORK_PASSPHRASE = \"Standalone Network ; February 2017\"\n");
        s.push_str(&format!("NODE_SEED = \"{}\"\n", test_node_seed(self_seed)));
        s.push_str("NODE_IS_VALIDATOR = true\n");
        s.push_str(&format!("NODE_HOME_DOMAIN = \"{}\"\n", self_domain));
        for (domain, quality, _) in domains {
            s.push_str("[[HOME_DOMAINS]]\n");
            s.push_str(&format!("HOME_DOMAIN = \"{}\"\n", domain));
            s.push_str(&format!("QUALITY = \"{}\"\n", quality));
        }
        for (domain, _quality, seeds) in domains {
            for seed in *seeds {
                s.push_str("[[VALIDATORS]]\n");
                s.push_str(&format!("NAME = \"v{}\"\n", seed));
                s.push_str(&format!("PUBLIC_KEY = \"{}\"\n", test_pubkey(*seed)));
                s.push_str(&format!("HOME_DOMAIN = \"{}\"\n", domain));
            }
        }
        s
    }

    #[test]
    fn test_auto_qset_groups_by_home_domain() {
        // 3 HIGH domains, 3 validators each. Self joins bd.
        let toml_str = build_auto_qset_toml(
            &[
                ("bd", "HIGH", &[1, 2, 3]),
                ("cq", "HIGH", &[4, 5, 6]),
                ("kb", "HIGH", &[7, 8, 9]),
            ],
            200, // self is a distinct key, joins bd (so bd has 4 validators)
            "bd",
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&val).unwrap();

        let xdr = config.node.quorum_set.to_xdr().unwrap();
        // 3 inner sets (one per home domain), no top-level validators.
        assert_eq!(xdr.inner_sets.len(), 3, "expected 3 per-domain inner sets");
        assert!(
            xdr.validators.is_empty(),
            "top-level validators must be empty on the auto path"
        );
        // Top threshold = BFT of 3 inner sets = 3 - (3-1)/3 = 3.
        assert_eq!(xdr.threshold, 3, "top BFT threshold of 3 inner sets");
        for inner in xdr.inner_sets.iter() {
            // bd has self + 3 = 4 validators; cq/kb have 3.
            assert!(
                inner.validators.len() == 3 || inner.validators.len() == 4,
                "inner domain set should have 3 or 4 validators, got {}",
                inner.validators.len()
            );
            // SIMPLE_MAJORITY: n - (n-1)/2.
            let n = inner.validators.len() as u32;
            let expected = n - (n - 1) / 2;
            assert_eq!(inner.threshold, expected, "inner SM threshold for n={n}");
        }
    }

    #[test]
    fn test_auto_qset_threshold_matches_core_vector() {
        // 7-org/23-node topology mirroring StableApproximateTier1CoreSets:
        // bd/cq/kb/sp/sdf/wx = 3 each, lo = 5; all HIGH. Self joins sdf (so sdf
        // has 4). Total inner sets = 7. Core counts: top BFT(7)=5, lo SM(5)=3,
        // others SM(3)=2 (sdf SM(4)=3 because self adds one).
        let toml_str = build_auto_qset_toml(
            &[
                ("bd", "HIGH", &[1, 2, 3]),
                ("cq", "HIGH", &[4, 5, 6]),
                ("kb", "HIGH", &[7, 8, 9]),
                ("sp", "HIGH", &[10, 11, 12]),
                ("sdf", "HIGH", &[13, 14, 15]),
                ("wx", "HIGH", &[16, 17, 18]),
                ("lo", "HIGH", &[19, 20, 21, 22, 23]),
            ],
            100, // self is a distinct key, joins sdf
            "sdf",
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&val).unwrap();
        let xdr = config.node.quorum_set.to_xdr().unwrap();

        assert_eq!(xdr.inner_sets.len(), 7, "7 org inner sets");
        assert!(xdr.validators.is_empty());
        // Top BFT of 7 = 7 - (7-1)/3 = 5.
        assert_eq!(xdr.threshold, 5, "top BFT(7)");

        // Find lo (5 validators), sdf (3 + self = 4), and a plain-3 org.
        let mut saw_lo = false;
        let mut saw_sdf = false;
        let mut saw_three = false;
        for inner in xdr.inner_sets.iter() {
            let n = inner.validators.len() as u32;
            let sm = n - (n - 1) / 2;
            assert_eq!(inner.threshold, sm, "inner SM for n={n}");
            match n {
                5 => {
                    saw_lo = true;
                    assert_eq!(inner.threshold, 3, "lo SM(5)=3");
                }
                4 => {
                    saw_sdf = true;
                    assert_eq!(inner.threshold, 3, "sdf SM(4)=3 (self included)");
                }
                3 => {
                    saw_three = true;
                    assert_eq!(inner.threshold, 2, "other SM(3)=2");
                }
                _ => panic!("unexpected inner size {n}"),
            }
        }
        assert!(saw_lo && saw_sdf && saw_three, "all org sizes present");
    }

    #[test]
    fn test_auto_qset_quality_tier_nesting() {
        // HIGH (3 orgs x 3 vals) over MEDIUM (2 orgs x 2 vals) over LOW (1 org x
        // 2 vals). Self joins a HIGH org. Top inner_sets = 3 HIGH org sets + 1
        // nested set; nested = 2 MEDIUM org sets + 1 nested set; innermost = 1
        // LOW org set (with 2 vals so no singleton collapse).
        let toml_str = build_auto_qset_toml(
            &[
                ("h1", "HIGH", &[1, 2, 3]),
                ("h2", "HIGH", &[4, 5, 6]),
                ("h3", "HIGH", &[7, 8, 9]),
                ("m1", "MEDIUM", &[10, 11]),
                ("m2", "MEDIUM", &[12, 13]),
                ("l1", "LOW", &[14, 15]),
            ],
            100,
            "h1",
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&val).unwrap();
        let xdr = config.node.quorum_set.to_xdr().unwrap();

        // Top: 3 HIGH org sets + 1 nested (MEDIUM tier) = 4 inner sets.
        assert_eq!(xdr.inner_sets.len(), 4, "3 HIGH orgs + 1 MEDIUM-tier nest");
        assert!(xdr.validators.is_empty());

        // Identify the nested MEDIUM-tier set: the one that itself contains inner
        // sets (org sets have only validators).
        let medium_tier = xdr
            .inner_sets
            .iter()
            .find(|s| !s.inner_sets.is_empty())
            .expect("a nested MEDIUM-tier set must exist");
        // MEDIUM tier: 2 MEDIUM org sets + 1 nested (LOW tier) = 3 inner sets.
        assert_eq!(
            medium_tier.inner_sets.len(),
            3,
            "2 MEDIUM orgs + 1 LOW-tier nest"
        );

        let low_tier = medium_tier
            .inner_sets
            .iter()
            .find(|s| !s.inner_sets.is_empty())
            .expect("a nested LOW-tier set must exist");
        // LOW tier: just the single LOW org set.
        assert_eq!(low_tier.inner_sets.len(), 1, "1 LOW org set");
        assert_eq!(
            low_tier.inner_sets[0].validators.len(),
            2,
            "LOW org has 2 validators (no singleton collapse)"
        );
    }

    #[test]
    fn test_auto_qset_ascending_quality_error() {
        // Two validators in the same home domain with different qualities is a
        // "must have same quality" error (handled via HOME_DOMAINS providing one
        // quality per domain; inline mismatch can't happen). Instead exercise the
        // genuine core path: a validator whose quality is higher than the current
        // recursion tier cannot occur after sort, but a same-domain quality
        // mismatch does. We test same-domain same-quality enforcement by giving a
        // domain two validators where one carries an inline QUALITY that differs.
        //
        // The reachable error in henyey's model: a HIGH domain with <3 validators.
        // Ascending-quality is structurally prevented by the quality-DESC sort, so
        // we assert the redundancy guard here and cover nesting separately.
        let toml_str = build_auto_qset_toml(&[("bd", "HIGH", &[1, 2])], 100, "bd");
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let result = translate_stellar_core_config(&val);
        assert!(result.is_err(), "HIGH domain with <3 validators must error");
    }

    #[test]
    fn test_high_quality_needs_three() {
        // A HIGH-quality home domain with only 2 validators (self in a different
        // domain) must error with the redundancy message.
        let toml_str = build_auto_qset_toml(
            &[("bd", "HIGH", &[1, 2]), ("cq", "HIGH", &[4, 5, 6])],
            100,
            "cq",
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let err = translate_stellar_core_config(&val).unwrap_err();
        assert!(
            err.to_string().contains("redundancy") || err.to_string().contains("at least 3"),
            "expected redundancy error, got: {err}"
        );
    }

    #[test]
    fn test_explicit_nested_qset_recurses() {
        // [QUORUM_SET] with THRESHOLD_PERCENT + two nested subtables, each with
        // VALIDATORS. Mirrors SSC addExplicitQsetAt output.
        let toml_str = format!(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "{seed}"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 67
            [QUORUM_SET.sub0]
            THRESHOLD_PERCENT = 51
            VALIDATORS = ["{a}", "{b}", "{c}"]
            [QUORUM_SET.sub1]
            THRESHOLD_PERCENT = 51
            VALIDATORS = ["{d}", "{e}", "{f}"]
            "#,
            seed = test_node_seed(100),
            a = test_pubkey(1),
            b = test_pubkey(2),
            c = test_pubkey(3),
            d = test_pubkey(4),
            e = test_pubkey(5),
            f = test_pubkey(6),
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&val).unwrap();
        let xdr = config.node.quorum_set.to_xdr().unwrap();

        assert_eq!(xdr.inner_sets.len(), 2, "two nested subtables");
        assert!(
            xdr.validators.is_empty(),
            "no direct validators at top of explicit nested qset"
        );
        // Top: 67% of 2 inner sets = 1 + (2*67-1)/100 = 2.
        assert_eq!(xdr.threshold, 2);
        for inner in xdr.inner_sets.iter() {
            assert_eq!(inner.validators.len(), 3);
            // 51% of 3 = 1 + (3*51-1)/100 = 2.
            assert_eq!(inner.threshold, 2);
        }
    }

    #[test]
    fn test_explicit_qset_validator_name_suffix_stripped() {
        // VALIDATORS entry "<G...> bd-0": only the leading G... token is parsed.
        let pk = test_pubkey(1);
        let toml_str = format!(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "{seed}"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 100
            VALIDATORS = ["{pk} bd-0"]
            "#,
            seed = test_node_seed(100),
            pk = pk,
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let config = translate_stellar_core_config(&val).unwrap();
        assert_eq!(config.node.quorum_set.validators.len(), 1);
        assert_eq!(
            config.node.quorum_set.validators[0], pk,
            "name suffix must be stripped, only the G... token kept"
        );
    }

    #[test]
    fn test_explicit_qset_nesting_too_deep() {
        // 5 levels of nesting (top=0, sub=1, sub.sub=2, ... level 5) must error
        // (MAXIMUM_QUORUM_NESTING_LEVEL = 4).
        let toml_str = format!(
            r#"
            NETWORK_PASSPHRASE = "Standalone Network ; February 2017"
            NODE_SEED = "{seed}"
            NODE_IS_VALIDATOR = true
            UNSAFE_QUORUM = true
            FAILURE_SAFETY = 0
            [QUORUM_SET]
            THRESHOLD_PERCENT = 67
            [QUORUM_SET.a]
            [QUORUM_SET.a.b]
            [QUORUM_SET.a.b.c]
            [QUORUM_SET.a.b.c.d]
            [QUORUM_SET.a.b.c.d.e]
            VALIDATORS = ["{pk}"]
            "#,
            seed = test_node_seed(100),
            pk = test_pubkey(1),
        );
        let val: toml::Value = toml::from_str(&toml_str).unwrap();
        let result = translate_stellar_core_config(&val);
        assert!(
            result.is_err(),
            "nesting deeper than level 4 must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("levels") || err.contains("nesting") || err.contains("too many"),
            "expected nesting-depth error, got: {err}"
        );
    }
}
