//! Genesis initialization and bootstrap logic.
//!
//! Contains functions for building genesis ledger entries, creating the
//! genesis bucket list, and bootstrapping the node from database state.

use henyey_bucket::{BucketList, HotArchiveBucketList};
use henyey_common::{deterministic_seed, Hash256, NetworkId};
use henyey_db::queries::StateQueries;

use super::App;

/// Total coins in the genesis ledger (100 billion XLM in stroops).
const GENESIS_TOTAL_COINS: i64 = 1_000_000_000_000_000_000;

/// Genesis-ledger construction parameters derived from `[testing]` config.
///
/// Mirrors stellar-core's `USE_CONFIG_FOR_GENESIS` + `TESTING_UPGRADE_*`
/// handling in `LedgerManagerImpl::startNewLedger()`
/// (LedgerManagerImpl.cpp:489-503). When `use_config_for_genesis` is false
/// (the default), genesis is built exactly as before: `ledger_version = 0`,
/// `base_fee = 100`, `base_reserve = 100_000_000`, `max_tx_set_size = 100`,
/// and NO Soroban `ConfigSetting` entries.
///
/// When `use_config_for_genesis` is true, the genesis header is built at
/// `protocol_version` with the `base_fee`/`base_reserve`/`max_tx_set_size`
/// overrides, and (for protocol >= 20) the full Soroban config-settings set
/// is injected into the genesis bucket list — matching SSC mixed-image
/// missions running BUILD_TESTS-enabled stellar-core.
#[derive(Debug, Clone, Copy)]
pub struct GenesisConfig {
    /// Whether to honor the `TESTING_UPGRADE_*` overrides and inject the
    /// Soroban config-settings set into genesis.
    pub use_config_for_genesis: bool,
    /// Protocol version for the genesis header when `use_config_for_genesis`.
    pub protocol_version: u32,
    /// `base_fee` override for the genesis header.
    pub base_fee: u32,
    /// `base_reserve` override for the genesis header.
    pub base_reserve: u32,
    /// `max_tx_set_size` override for the genesis header.
    pub max_tx_set_size: u32,
}

impl Default for GenesisConfig {
    fn default() -> Self {
        // Matches stellar-core Config.cpp defaults (false / 26 / 100 /
        // 100_000_000 / 50). With `use_config_for_genesis = false` these
        // overrides are inert and genesis is byte-identical to the pre-change
        // behavior (the genesis header still uses the hardcoded 0 / 100 /
        // 100_000_000 / 100 values below).
        Self {
            use_config_for_genesis: false,
            protocol_version: 26,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 50,
        }
    }
}

/// Initialize a fresh genesis ledger in an empty database.
///
/// This is the shared genesis initialization used by all code paths that need
/// to create a new ledger from scratch: in-memory App startup, the `new-db`
/// CLI command, and simulation. Mirrors stellar-core's `newDB()` →
/// `startNewLedger()` flow (ApplicationImpl.cpp:325-328, 374-379).
///
/// # Preconditions
///
/// The database must be completely empty — no LCL and no ledger headers.
/// Returns an error if any preexisting state is detected (defense-in-depth
/// against state leaks from stale or reused database files).
///
/// # What it writes
///
/// Bucket files are persisted to `bucket_dir` before the DB transaction.
/// All SQLite writes (emptiness check + state persistence) are performed
/// in a single transaction to ensure atomicity.
///
/// - Genesis ledger entries (root account + optional test accounts)
/// - Genesis bucket list + bucket files in `bucket_dir`
/// - Genesis ledger header (protocol 0, ledger_seq=1) + computed hash
/// - HAS (HistoryArchiveState) JSON
/// - Bucket level hashes
/// - Genesis tx history entry (empty TransactionSet)
/// - Genesis tx result entry (empty TransactionResultSet)
/// - Network passphrase
/// - LCL = 1
pub fn initialize_genesis(
    db: &henyey_db::Database,
    bucket_dir: Option<&std::path::Path>,
    network_passphrase: &str,
    genesis_test_account_count: u32,
    genesis_config: &GenesisConfig,
) -> anyhow::Result<()> {
    use henyey_db::schema::state_keys;
    use henyey_history::build_history_archive_state;
    use henyey_ledger::{calculate_skip_values, compute_header_hash};
    use stellar_xdr::curr::{
        BucketListType, Hash, LedgerHeader, LedgerHeaderExt, Limits, StellarValue, StellarValueExt,
        TimePoint, TransactionHistoryEntry, TransactionHistoryEntryExt,
        TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
        TransactionSet, VecM, WriteXdr,
    };

    // Header field values. When USE_CONFIG_FOR_GENESIS is set, stellar-core
    // overrides ledger_version/baseFee/baseReserve/maxTxSetSize from the
    // TESTING_UPGRADE_* config (LedgerManagerImpl.cpp:489-503). Otherwise the
    // genesis defaults (0 / 100 / 100_000_000 / 100) are used, byte-identical
    // to the pre-change behavior.
    let (ledger_version, base_fee, base_reserve, max_tx_set_size) =
        if genesis_config.use_config_for_genesis {
            (
                genesis_config.protocol_version,
                genesis_config.base_fee,
                genesis_config.base_reserve,
                genesis_config.max_tx_set_size,
            )
        } else {
            (0u32, 100u32, 100_000_000u32, 100u32)
        };

    // Build genesis account entries (root + optional test accounts).
    let mut genesis_entries = build_genesis_entries(
        network_passphrase,
        genesis_test_account_count,
        GENESIS_TOTAL_COINS,
    );

    // When USE_CONFIG_FOR_GENESIS is set and the genesis protocol is >= 20,
    // inject the full Soroban ConfigSetting entry set. Mirrors
    // SorobanNetworkConfig::initializeGenesisLedgerForTesting
    // (NetworkConfig.cpp:1708), which stellar-core's startNewLedger(genesis)
    // invokes (LedgerManagerImpl.cpp:421). Entry ordering is irrelevant —
    // add_batch sorts the batch by LedgerKey, matching core's canonical bucket
    // assembly.
    if genesis_config.use_config_for_genesis && ledger_version >= 20 {
        let network_id = NetworkId::from_passphrase(network_passphrase);
        let config_entries =
            henyey_ledger::build_genesis_soroban_config_entries(ledger_version, &network_id)
                .map_err(|e| {
                    anyhow::anyhow!("Failed to build genesis Soroban config entries: {}", e)
                })?;
        genesis_entries.extend(config_entries);
    }

    // Create bucket list and add all genesis entries.
    let mut bucket_list = BucketList::new();
    bucket_list
        .add_batch(1, 0, BucketListType::Live, genesis_entries, vec![], vec![])
        .map_err(|e| anyhow::anyhow!("Failed to add genesis entries to bucket list: {}", e))?;

    // Persist non-empty bucket files to disk.
    if let Some(dir) = bucket_dir {
        persist_genesis_buckets(&bucket_list, dir)?;
    }

    // Build genesis header.
    let bucket_list_hash = bucket_list.hash();
    let mut header = LedgerHeader {
        ledger_version,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash(*bucket_list_hash.as_bytes()),
        ledger_seq: 1,
        total_coins: GENESIS_TOTAL_COINS,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee,
        base_reserve,
        max_tx_set_size,
        skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
        ext: LedgerHeaderExt::V0,
    };
    calculate_skip_values(&mut header);

    let header_hash = compute_header_hash(&header)?;
    let header_xdr = header.to_xdr(Limits::none())?;

    // Build HAS (HistoryArchiveState) for genesis.
    let has =
        build_history_archive_state(1, &bucket_list, None, Some(network_passphrase.to_string()))
            .map_err(|e| anyhow::anyhow!("Failed to build HAS: {}", e))?;
    let has_json = has.to_json()?;

    // Build bucket level hashes for DB storage.
    let bucket_levels: Vec<(Hash256, Hash256)> = bucket_list
        .levels()
        .iter()
        .map(|level| (level.curr.hash(), level.snap.hash()))
        .collect();

    // Build empty tx history / tx result entries for genesis (needed by
    // history archive publishing — the first checkpoint includes ledger 1).
    let genesis_tx_history = TransactionHistoryEntry {
        ledger_seq: 1,
        tx_set: TransactionSet {
            previous_ledger_hash: Hash(Hash256::ZERO.0),
            txs: VecM::default(),
        },
        ext: TransactionHistoryEntryExt::V0,
    };
    let genesis_tx_result = TransactionHistoryResultEntry {
        ledger_seq: 1,
        tx_result_set: TransactionResultSet {
            results: VecM::default(),
        },
        ext: TransactionHistoryResultEntryExt::default(),
    };

    // Verify emptiness and persist everything in a single transaction.
    // This eliminates TOCTOU between the check and the writes, and ensures
    // all DB state is committed atomically.
    db.transaction(|tx| {
        use henyey_db::queries::{BucketListQueries, HistoryQueries, LedgerQueries, StateQueries};

        // Defense-in-depth: verify the database is genuinely empty before writing
        // genesis state. Checks both LCL and ledger headers to catch partial/corrupt
        // state from stale or reused database files.
        let existing_lcl = tx.get_last_closed_ledger()?;
        if existing_lcl.is_some() {
            return Err(henyey_db::DbError::Integrity(format!(
                "Cannot initialize genesis: database already has LCL={:?}. \
                 Expected empty database.",
                existing_lcl,
            )));
        }
        let latest_header = tx.get_latest_ledger_seq()?;
        if latest_header.is_some() {
            return Err(henyey_db::DbError::Integrity(format!(
                "Cannot initialize genesis: database has ledger headers \
                 (latest={:?}) but no LCL. Expected empty database.",
                latest_header,
            )));
        }

        tx.store_ledger_header(&header, &header_xdr)?;
        tx.store_tx_history_entry(1, &genesis_tx_history)?;
        tx.store_tx_result_entry(1, &genesis_tx_result)?;
        tx.store_bucket_list(1, &bucket_levels)?;
        tx.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
        tx.set_state(state_keys::NETWORK_PASSPHRASE, network_passphrase)?;
        tx.set_last_closed_ledger(1)?;
        Ok(())
    })?;

    tracing::info!(
        ledger_seq = 1,
        header_hash = %header_hash,
        bucket_list_hash = %bucket_list_hash,
        "Genesis ledger initialized"
    );

    Ok(())
}

/// Build genesis ledger entries (root account + optional test accounts).
pub(crate) fn build_genesis_entries(
    passphrase: &str,
    test_account_count: u32,
    total_coins: i64,
) -> Vec<stellar_xdr::curr::LedgerEntry> {
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, LedgerEntry, LedgerEntryData, LedgerEntryExt,
        PublicKey, SequenceNumber, Thresholds, Uint256, VecM,
    };

    let network_id = NetworkId::from_passphrase(passphrase);
    let root_secret = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
    let root_public = root_secret.public_key();
    let root_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *root_public.as_bytes(),
    )));

    let (root_balance, test_balance) = if test_account_count > 0 {
        let total_accounts = test_account_count as i64 + 1;
        let base = total_coins / total_accounts;
        let remainder = total_coins % total_accounts;
        (base + remainder, base)
    } else {
        (total_coins, 0i64)
    };

    let make_entry = |account_id: AccountId, balance: i64| -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    };

    let mut entries = vec![make_entry(root_account_id, root_balance)];

    for i in 0..test_account_count {
        let name = format!("TestAccount-{}", i);
        let seed = deterministic_seed(&name);
        let secret = henyey_crypto::SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let acct_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*public.as_bytes())));
        entries.push(make_entry(acct_id, test_balance));
    }

    entries
}

/// Persist non-empty bucket files to disk so that `load_last_known_ledger`
/// can restore state on the next `run` startup.
pub(crate) fn persist_genesis_buckets(
    bucket_list: &BucketList,
    bucket_dir: &std::path::Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(bucket_dir)?;
    for level in bucket_list.levels() {
        for bucket in [&level.curr, &level.snap] {
            if bucket.backing_file_path().is_none() && !bucket.hash().is_zero() {
                let path =
                    bucket_dir.join(henyey_bucket::canonical_bucket_filename(&bucket.hash()));
                if !path.exists() {
                    bucket
                        .save_to_xdr_file(&path)
                        .map_err(|e| anyhow::anyhow!("Failed to save genesis bucket: {}", e))?;
                }
            }
        }
    }
    Ok(())
}

impl App {
    /// Bootstrap the node from genesis state stored in the database.
    ///
    /// Used by the force-scp flow for standalone single-node networks.
    /// Reads the genesis ledger header from the DB, recreates the bucket list
    /// with the root account entry (and test accounts if configured), and
    /// initializes the LedgerManager.
    pub async fn bootstrap_from_db(&self) -> anyhow::Result<()> {
        use henyey_db::queries::LedgerQueries;
        use henyey_ledger::compute_header_hash;

        // Read LCL header from DB (offloaded to blocking pool)
        let (_lcl_seq, header) = self
            .db_blocking("bootstrap-load-lcl", |db| {
                db.with_connection(|conn| {
                    let lcl_seq = conn
                        .get_last_closed_ledger()?
                        .ok_or_else(|| henyey_db::DbError::Integrity("No LCL in DB".to_string()))?;
                    let header = conn.load_ledger_header(lcl_seq)?.ok_or_else(|| {
                        henyey_db::DbError::Integrity(format!("No header for LCL {}", lcl_seq))
                    })?;
                    Ok((lcl_seq, header))
                })
                .map_err(Into::into)
            })
            .await?;

        let header_hash = compute_header_hash(&header)
            .map_err(|e| anyhow::anyhow!("Failed to compute header hash: {}", e))?;

        let genesis_entries = build_genesis_entries(
            &self.config.network.passphrase,
            self.config.testing.genesis_test_account_count,
            header.total_coins,
        );

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();

        // Recreate bucket list from genesis entries for LedgerManager init.
        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_dir.clone());
        {
            use stellar_xdr::curr::BucketListType;
            bucket_list
                .add_batch(1, 0, BucketListType::Live, genesis_entries, vec![], vec![])
                .map_err(|e| anyhow::anyhow!("Failed to create genesis bucket list: {}", e))?;
        }
        persist_genesis_buckets(&bucket_list, &bucket_dir)?;

        // Verify hash matches the header.
        let computed_hash = bucket_list.hash();
        let expected_hash = Hash256::from_bytes(header.bucket_list_hash.0);
        if computed_hash != expected_hash {
            anyhow::bail!(
                "Genesis bucket list hash mismatch: computed {} vs header {}",
                computed_hash,
                expected_hash
            );
        }

        // Initialize LedgerManager
        let hot_archive = HotArchiveBucketList::new();
        self.ledger_manager
            .initialize(bucket_list, hot_archive, header, header_hash)
            .map_err(|e| anyhow::anyhow!("Failed to initialize LedgerManager: {}", e))?;

        let info = self.ledger_info();
        tracing::info!(
            lcl_seq = info.ledger_seq,
            "Bootstrapped from genesis state via force-scp"
        );

        self.set_state(super::AppState::Synced).await;

        // Seed validation context so tx queue rejects invalid Soroban txs immediately.
        self.seed_validation_context();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_db::queries::{LedgerQueries, StateQueries};
    use henyey_db::schema::state_keys;

    const PASSPHRASE: &str = "Test SDF Network ; September 2015";

    #[test]
    fn test_initialize_genesis_writes_complete_state() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        initialize_genesis(&db, None, PASSPHRASE, 0, &GenesisConfig::default()).unwrap();

        db.with_connection(|conn| {
            // LCL = 1
            let lcl = conn.get_last_closed_ledger()?;
            assert_eq!(lcl, Some(1));

            // Ledger header exists
            let header = conn.load_ledger_header(1)?;
            assert!(header.is_some());
            let header = header.unwrap();
            assert_eq!(header.ledger_seq, 1);
            assert_eq!(header.ledger_version, 0);
            assert_eq!(header.total_coins, GENESIS_TOTAL_COINS);

            // Network passphrase stored
            let stored_passphrase = conn.get_state(state_keys::NETWORK_PASSPHRASE)?;
            assert_eq!(stored_passphrase.as_deref(), Some(PASSPHRASE));

            // HAS stored
            let has = conn.get_state(state_keys::HISTORY_ARCHIVE_STATE)?;
            assert!(has.is_some());

            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_initialize_genesis_rejects_nonempty_db_with_lcl() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        // First init succeeds
        initialize_genesis(&db, None, PASSPHRASE, 0, &GenesisConfig::default()).unwrap();
        // Second init should fail (DB has LCL)
        let result = initialize_genesis(&db, None, PASSPHRASE, 0, &GenesisConfig::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Cannot initialize genesis"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_initialize_genesis_rejects_db_with_headers_but_no_lcl() {
        let db = henyey_db::Database::open_in_memory().unwrap();

        // Write a stale header without setting LCL.
        use stellar_xdr::curr::{
            Hash, LedgerHeader, LedgerHeaderExt, Limits, StellarValue, StellarValueExt, TimePoint,
            VecM, WriteXdr,
        };
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 42,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: LedgerHeaderExt::V0,
        };
        let header_xdr = header.to_xdr(Limits::none()).unwrap();
        db.with_connection(|conn| conn.store_ledger_header(&header, &header_xdr))
            .unwrap();

        let result = initialize_genesis(&db, None, PASSPHRASE, 0, &GenesisConfig::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Cannot initialize genesis"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_initialize_genesis_with_test_accounts() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        initialize_genesis(&db, None, PASSPHRASE, 3, &GenesisConfig::default()).unwrap();

        db.with_connection(|conn| {
            let lcl = conn.get_last_closed_ledger()?;
            assert_eq!(lcl, Some(1));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_initialize_genesis_persists_bucket_files() {
        let dir = tempfile::tempdir().unwrap();
        let bucket_dir = dir.path().join("buckets");
        let db = henyey_db::Database::open_in_memory().unwrap();

        initialize_genesis(
            &db,
            Some(&bucket_dir),
            PASSPHRASE,
            0,
            &GenesisConfig::default(),
        )
        .unwrap();

        // At least one bucket file should exist.
        let entries: Vec<_> = std::fs::read_dir(&bucket_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !entries.is_empty(),
            "bucket files should be persisted to disk"
        );
    }

    /// Helper: build a `GenesisConfig` with `use_config_for_genesis = true` at a
    /// fixed protocol, using stellar-core's TESTING_UPGRADE_* defaults.
    fn config_for_genesis(protocol: u32) -> GenesisConfig {
        GenesisConfig {
            use_config_for_genesis: true,
            protocol_version: protocol,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 50,
        }
    }

    /// Collect the `ConfigSettingId`s present in the genesis Soroban config set
    /// for the given protocol.
    fn genesis_config_setting_ids(protocol: u32) -> Vec<stellar_xdr::curr::ConfigSettingId> {
        use stellar_xdr::curr::LedgerEntryData;
        let network_id = NetworkId::from_passphrase(PASSPHRASE);
        let entries =
            henyey_ledger::build_genesis_soroban_config_entries(protocol, &network_id).unwrap();
        let mut ids: Vec<_> = entries
            .iter()
            .filter_map(|e| match &e.data {
                LedgerEntryData::ConfigSetting(cs) => Some(cs.discriminant()),
                _ => None,
            })
            .collect();
        ids.sort_by_key(|id| *id as i32);
        ids
    }

    /// Regression test for #3492: with USE_CONFIG_FOR_GENESIS=true the genesis
    /// header carries the configured protocol + fee overrides, and the genesis
    /// bucket list contains the FULL Soroban ConfigSetting entry set.
    ///
    /// Fails on `origin/main` (genesis hardcodes ledger_version=0 and emits
    /// zero ConfigSetting entries — bootstrap.rs:80).
    #[test]
    fn test_genesis_config_for_genesis_matches_core_3492() {
        use stellar_xdr::curr::ConfigSettingId;

        let db = henyey_db::Database::open_in_memory().unwrap();
        let protocol = 25;
        initialize_genesis(&db, None, PASSPHRASE, 0, &config_for_genesis(protocol)).unwrap();

        // (a) Header carries the configured protocol + TESTING_UPGRADE_* fees.
        db.with_connection(|conn| {
            let header = conn.load_ledger_header(1)?.unwrap();
            assert_eq!(
                header.ledger_version, protocol,
                "genesis header must use the configured protocol version"
            );
            assert_eq!(
                header.base_fee, 100,
                "base_fee from TESTING_UPGRADE_DESIRED_FEE"
            );
            assert_eq!(
                header.base_reserve, 100_000_000,
                "base_reserve from TESTING_UPGRADE_RESERVE"
            );
            assert_eq!(
                header.max_tx_set_size, 50,
                "max_tx_set_size from TESTING_UPGRADE_MAX_TX_SET_SIZE (50, not 100)"
            );
            Ok(())
        })
        .unwrap();

        // (b) The genesis Soroban config set is present, with the EXACT expected
        // per-ID membership at protocol 25 (V20's 14 + V23's 3 = 17 entries; no
        // V26 frozen-key entries yet). Per-ID presence, not just "non-empty".
        let ids = genesis_config_setting_ids(protocol);
        let expected: Vec<ConfigSettingId> = {
            let mut v = vec![
                ConfigSettingId::ContractMaxSizeBytes,
                ConfigSettingId::ContractComputeV0,
                ConfigSettingId::ContractLedgerCostV0,
                ConfigSettingId::ContractHistoricalDataV0,
                ConfigSettingId::ContractEventsV0,
                ConfigSettingId::ContractBandwidthV0,
                ConfigSettingId::ContractCostParamsCpuInstructions,
                ConfigSettingId::ContractCostParamsMemoryBytes,
                ConfigSettingId::ContractDataKeySizeBytes,
                ConfigSettingId::ContractDataEntrySizeBytes,
                ConfigSettingId::StateArchival,
                ConfigSettingId::ContractExecutionLanes,
                ConfigSettingId::LiveSorobanStateSizeWindow,
                ConfigSettingId::EvictionIterator,
                // V23 additions:
                ConfigSettingId::ContractParallelComputeV0,
                ConfigSettingId::ContractLedgerCostExtV0,
                ConfigSettingId::ScpTiming,
            ];
            v.sort_by_key(|id| *id as i32);
            v
        };
        assert_eq!(
            ids, expected,
            "genesis config-setting ID set at p25 must match core exactly"
        );
        assert_eq!(ids.len(), 17, "p25 genesis config-setting count");
    }

    /// Backward-compat guard: with USE_CONFIG_FOR_GENESIS absent/false the
    /// genesis is BYTE-IDENTICAL to the legacy behavior — ledger_version 0, no
    /// config entries, and a pinned header/bucket-list hash. Would FAIL if the
    /// fix regressed the default path.
    #[test]
    fn test_genesis_no_config_for_genesis_unchanged() {
        use henyey_ledger::compute_header_hash;

        let db = henyey_db::Database::open_in_memory().unwrap();
        initialize_genesis(&db, None, PASSPHRASE, 0, &GenesisConfig::default()).unwrap();

        db.with_connection(|conn| {
            let header = conn.load_ledger_header(1)?.unwrap();
            // Legacy genesis header constants.
            assert_eq!(header.ledger_version, 0);
            assert_eq!(header.base_fee, 100);
            assert_eq!(header.base_reserve, 100_000_000);
            assert_eq!(header.max_tx_set_size, 100);

            // Pin the genesis hashes for the testnet passphrase. These are the
            // pre-change values; any drift means the default path regressed.
            let header_hash = compute_header_hash(&header).unwrap();
            assert_eq!(
                header_hash.to_hex(),
                "63d98f536ee68d1b27b5b89f23af5311b7569a24faf1403ad0b52b633b07be99",
                "genesis header hash regressed for the no-config path"
            );
            assert_eq!(
                hex::encode(header.bucket_list_hash.0),
                "572a2e32ff248a07b0e70fd1f6d318c1facd20b6cc08c33d5775259868125a16",
                "genesis bucket-list hash regressed for the no-config path"
            );
            Ok(())
        })
        .unwrap();
    }
}
