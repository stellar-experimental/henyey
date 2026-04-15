//! Genesis initialization and bootstrap logic.
//!
//! Contains functions for building genesis ledger entries, creating the
//! genesis bucket list, and bootstrapping the node from database state.

use henyey_bucket::{BucketList, HotArchiveBucketList};
use henyey_common::{deterministic_seed, NetworkId};
use henyey_db::queries::StateQueries;

use super::App;

impl App {
    /// Build genesis ledger entries (root account + optional test accounts).
    ///
    /// Mirrors the account creation logic in `initialize_genesis_ledger`.
    pub(super) fn build_genesis_entries(
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

    /// Create the genesis bucket list and persist non-empty buckets to disk.
    ///
    /// Verifies the computed bucket list hash matches the expected header hash.
    pub(super) fn create_genesis_bucket_list(
        bucket_dir: &std::path::Path,
        genesis_entries: Vec<stellar_xdr::curr::LedgerEntry>,
        header: &stellar_xdr::curr::LedgerHeader,
    ) -> anyhow::Result<BucketList> {
        use stellar_xdr::curr::BucketListType;

        std::fs::create_dir_all(bucket_dir)?;

        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_dir.to_path_buf());
        bucket_list
            .add_batch(1, 0, BucketListType::Live, genesis_entries, vec![], vec![])
            .map_err(|e| anyhow::anyhow!("Failed to create genesis bucket list: {}", e))?;

        // Persist all non-empty buckets to disk so they're available for
        // history publishing and restart recovery.
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

        // Verify hash matches
        let computed_hash = bucket_list.hash();
        let expected_hash = henyey_common::Hash256::from_bytes(header.bucket_list_hash.0);
        if computed_hash != expected_hash {
            anyhow::bail!(
                "Genesis bucket list hash mismatch: computed {} vs header {}",
                computed_hash,
                expected_hash
            );
        }

        Ok(bucket_list)
    }

    /// Bootstrap the node from genesis state stored in the database.
    ///
    /// Used by the force-scp flow for standalone single-node networks.
    /// Reads the genesis ledger header from the DB, recreates the bucket list
    /// with the root account entry (and test accounts if configured), and
    /// initializes the LedgerManager.
    pub async fn bootstrap_from_db(&self) -> anyhow::Result<()> {
        use henyey_db::queries::LedgerQueries;
        use henyey_ledger::compute_header_hash;

        // Read LCL header from DB
        let (_lcl_seq, header) = self.db.with_connection(|conn| {
            let lcl_seq = conn
                .get_last_closed_ledger()?
                .ok_or_else(|| henyey_db::DbError::Integrity("No LCL in DB".to_string()))?;
            let header = conn.load_ledger_header(lcl_seq)?.ok_or_else(|| {
                henyey_db::DbError::Integrity(format!("No header for LCL {}", lcl_seq))
            })?;
            Ok((lcl_seq, header))
        })?;

        let header_hash = compute_header_hash(&header)
            .map_err(|e| anyhow::anyhow!("Failed to compute header hash: {}", e))?;

        let genesis_entries = Self::build_genesis_entries(
            &self.config.network.passphrase,
            self.config.testing.genesis_test_account_count,
            header.total_coins,
        );

        let bucket_dir = self.bucket_manager.bucket_dir().to_path_buf();
        let bucket_list = Self::create_genesis_bucket_list(&bucket_dir, genesis_entries, &header)?;

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
