//! Protocol 25 Soroban host implementation.
//!
//! This module provides Soroban execution for protocol version 25.
//! It uses soroban-env-host-p25 which is pinned to the exact git revision
//! used by stellar-core for protocol 25.
//!
//! After XDR alignment: the workspace stellar-xdr 25.0.0 and soroban-env-host
//! P25's stellar-xdr 25.0.0 are the same crate, so all types are identical
//! and no XDR conversion is needed for the P25 path.

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use soroban_env_host_p25::xdr::{ReadXdr as ReadXdrP25, WriteXdr as WriteXdrP25};
    use soroban_env_host_p25::{storage::SnapshotSource, HostError};

    use stellar_xdr::curr::{
        AccountId, Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerKey, Limits, ReadXdr,
        WriteXdr,
    };

    use crate::soroban::host::EntryWithTtl;
    use crate::state::LedgerStateManager;

    /// Adapter that provides snapshot access to our ledger state for Soroban.
    struct LedgerSnapshotAdapter<'a> {
        state: &'a LedgerStateManager,
        current_ledger: u32,
    }

    impl<'a> LedgerSnapshotAdapter<'a> {
        fn new(state: &'a LedgerStateManager, current_ledger: u32) -> Self {
            Self {
                state,
                current_ledger,
            }
        }

        /// Get an entry using our workspace XDR types (for internal use).
        /// This is separate from the `SnapshotSource::get()` trait impl which uses
        /// soroban-env-host's XDR types.
        ///
        /// IMPORTANT: This function filters out expired Soroban entries (ContractData, ContractCode)
        /// to match stellar-core behavior. In stellar-core, entries with expired TTL are not passed to
        /// the host during invoke_host_function - expired temporary entries are skipped entirely,
        /// and expired persistent entries are treated as archived (requiring restoration).
        fn get_local(&self, key: &LedgerKey) -> Result<Option<EntryWithTtl>, HostError> {
            let live_until = get_entry_ttl(self.state, key, self.current_ledger);

            let entry = match key {
                LedgerKey::Account(account_key) => self
                    .state
                    .get_account(&account_key.account_id)
                    .map(|acc| LedgerEntry {
                        last_modified_ledger_seq: self.current_ledger,
                        data: LedgerEntryData::Account(acc.clone()),
                        ext: LedgerEntryExt::V0,
                    }),
                LedgerKey::Trustline(tl_key) => self
                    .state
                    .get_trustline_by_trustline_asset(&tl_key.account_id, &tl_key.asset)
                    .map(|tl| LedgerEntry {
                        last_modified_ledger_seq: self.current_ledger,
                        data: LedgerEntryData::Trustline(tl.clone()),
                        ext: LedgerEntryExt::V0,
                    }),
                LedgerKey::ContractData(cd_key) => {
                    // Check TTL: In stellar-core, ContractData entries are only passed to
                    // the host if they have a valid (non-expired) TTL entry. Entries without
                    // a TTL entry or with expired TTL are not passed to the host.
                    // - If TTL exists and is live (live_until >= current_ledger): pass to host
                    // - If TTL exists and is expired (live_until < current_ledger): skip
                    // - If TTL doesn't exist: skip (entry is not properly initialized or was deleted)
                    match live_until {
                        Some(lu) if lu >= self.current_ledger => {
                            // TTL exists and is live - return the entry
                            self.state
                                .get_contract_data(&cd_key.contract, &cd_key.key, cd_key.durability)
                                .map(|cd| LedgerEntry {
                                    last_modified_ledger_seq: self.current_ledger,
                                    data: LedgerEntryData::ContractData(cd.clone()),
                                    ext: LedgerEntryExt::V0,
                                })
                        }
                        Some(lu) => {
                            // TTL exists but is expired
                            tracing::debug!(
                                current_ledger = self.current_ledger,
                                live_until = lu,
                                "get_local: ContractData entry has expired TTL, not passing to host"
                            );
                            None
                        }
                        None => {
                            // No TTL entry found - in stellar-core this means the entry is not live
                            tracing::debug!(
                                current_ledger = self.current_ledger,
                                "get_local: ContractData entry has no TTL, not passing to host"
                            );
                            None
                        }
                    }
                }
                LedgerKey::ContractCode(cc_key) => {
                    // Same as ContractData - require valid TTL to pass to host
                    match live_until {
                        Some(lu) if lu >= self.current_ledger => {
                            // TTL exists and is live - return the entry
                            self.state
                                .get_contract_code(&cc_key.hash)
                                .map(|code| LedgerEntry {
                                    last_modified_ledger_seq: self.current_ledger,
                                    data: LedgerEntryData::ContractCode(code.clone()),
                                    ext: LedgerEntryExt::V0,
                                })
                        }
                        Some(lu) => {
                            // TTL exists but is expired
                            tracing::debug!(
                                current_ledger = self.current_ledger,
                                live_until = lu,
                                "get_local: ContractCode entry has expired TTL, not passing to host"
                            );
                            None
                        }
                        None => {
                            // No TTL entry found
                            tracing::debug!(
                                current_ledger = self.current_ledger,
                                "get_local: ContractCode entry has no TTL, not passing to host"
                            );
                            None
                        }
                    }
                }
                LedgerKey::Ttl(ttl_key) => {
                    self.state
                        .get_ttl(&ttl_key.key_hash)
                        .map(|ttl| LedgerEntry {
                            last_modified_ledger_seq: self.current_ledger,
                            data: LedgerEntryData::Ttl(ttl.clone()),
                            ext: LedgerEntryExt::V0,
                        })
                }
                _ => None,
            };

            match entry {
                Some(e) => Ok(Some((Rc::new(e), live_until))),
                None => Ok(None),
            }
        }
    }

    impl<'a> SnapshotSource for LedgerSnapshotAdapter<'a> {
        fn get(
            &self,
            key: &Rc<soroban_env_host_p25::xdr::LedgerKey>,
        ) -> Result<Option<soroban_env_host_p25::storage::EntryWithLiveUntil>, HostError> {
            // Convert P25 LedgerKey to workspace LedgerKey via XDR bytes
            let key_bytes = key
                .to_xdr(soroban_env_host_p25::xdr::Limits::none())
                .map_err(|_| {
                    soroban_env_host_p25::Error::from_type_and_code(
                        soroban_env_host_p25::xdr::ScErrorType::Context,
                        soroban_env_host_p25::xdr::ScErrorCode::InternalError,
                    )
                })?;
            let ws_key: LedgerKey =
                LedgerKey::from_xdr(&key_bytes, Limits::none()).map_err(|_| {
                    soroban_env_host_p25::Error::from_type_and_code(
                        soroban_env_host_p25::xdr::ScErrorType::Context,
                        soroban_env_host_p25::xdr::ScErrorCode::InternalError,
                    )
                })?;

            // Use get_local which works with workspace types
            match self.get_local(&ws_key)? {
                Some((entry, live_until)) => {
                    // Convert workspace LedgerEntry to P25 LedgerEntry
                    let entry_bytes = entry.to_xdr(Limits::none()).map_err(|_| {
                        soroban_env_host_p25::Error::from_type_and_code(
                            soroban_env_host_p25::xdr::ScErrorType::Context,
                            soroban_env_host_p25::xdr::ScErrorCode::InternalError,
                        )
                    })?;
                    let p25_entry = soroban_env_host_p25::xdr::LedgerEntry::from_xdr(
                        &entry_bytes,
                        soroban_env_host_p25::xdr::Limits::none(),
                    )
                    .map_err(|_| {
                        soroban_env_host_p25::Error::from_type_and_code(
                            soroban_env_host_p25::xdr::ScErrorType::Context,
                            soroban_env_host_p25::xdr::ScErrorCode::InternalError,
                        )
                    })?;
                    Ok(Some((Rc::new(p25_entry), live_until)))
                }
                None => Ok(None),
            }
        }
    }

    /// Get the TTL for a ledger entry.
    ///
    /// IMPORTANT: This function uses `get_ttl_at_ledger_start()` to return the TTL
    /// value from the bucket list snapshot at ledger start. This is critical for
    /// matching stellar-core behavior in parallel Soroban execution (V1 phases):
    ///
    /// - Transactions in different clusters of the same stage should NOT see each
    ///   other's changes (including newly created TTL entries)
    /// - Only entries that existed at ledger start should be passed to the host
    /// - Entries created by earlier transactions in the same ledger are filtered out
    ///
    /// This ensures that if TX 4 creates a new entry with TTL, TX 5 (in a different
    /// cluster of the same stage) will NOT see that entry when loading its footprint.
    fn get_entry_ttl(
        state: &LedgerStateManager,
        key: &LedgerKey,
        current_ledger: u32,
    ) -> Option<u32> {
        match key {
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {
                let key_hash = crate::soroban::compute_key_hash(key);
                // Use get_ttl_at_ledger_start() to match stellar-core behavior for parallel Soroban.
                // This returns the TTL from the bucket list snapshot at ledger start,
                // NOT the current TTL (which might include entries created by earlier TXs).
                let ttl = state.get_ttl_at_ledger_start(&key_hash);
                if let Some(live_until) = ttl {
                    if live_until < current_ledger {
                        tracing::debug!(
                            current_ledger,
                            live_until,
                            key_type = if matches!(key, LedgerKey::ContractCode(_)) {
                                "ContractCode"
                            } else {
                                "ContractData"
                            },
                            "Soroban entry TTL is EXPIRED (from bucket list snapshot)"
                        );
                    }
                } else {
                    // Check if the entry has a current TTL (created within this ledger)
                    let has_current_ttl = state.get_ttl(&key_hash).is_some();
                    if has_current_ttl {
                        tracing::debug!(
                            key_type = if matches!(key, LedgerKey::ContractCode(_)) {
                                "ContractCode"
                            } else {
                                "ContractData"
                            },
                            "Soroban entry has TTL but not in bucket list snapshot (created within ledger)"
                        );
                    } else {
                        tracing::debug!(
                            key_type = if matches!(key, LedgerKey::ContractCode(_)) {
                                "ContractCode"
                            } else {
                                "ContractData"
                            },
                            "Soroban entry has NO TTL record"
                        );
                    }
                }
                ttl
            }
            _ => None,
        }
    }

    use stellar_xdr::curr::{
        ContractDataDurability, ContractDataEntry, ContractId, LedgerKeyContractData, ScAddress,
        ScVal, TtlEntry,
    };

    /// Helper to create a test contract data key.
    fn make_contract_data_key(contract_hash: [u8; 32], key_val: i32) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash(contract_hash))),
            key: ScVal::I32(key_val),
            durability: ContractDataDurability::Persistent,
        })
    }

    /// Helper to create a test contract data entry.
    fn make_contract_data_entry(
        contract_hash: [u8; 32],
        key_val: i32,
        val: i32,
    ) -> ContractDataEntry {
        ContractDataEntry {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract: ScAddress::Contract(ContractId(Hash(contract_hash))),
            key: ScVal::I32(key_val),
            durability: ContractDataDurability::Persistent,
            val: ScVal::I32(val),
        }
    }

    /// Test that get_local returns entry when TTL is valid (live_until >= current_ledger).
    ///
    /// This is a regression test for the fix where entries without valid TTL should
    /// not be passed to the Soroban host. Previously, entries were returned even when
    /// TTL was missing or expired.
    ///
    /// Note: We call capture_ttl_bucket_list_snapshot() to simulate entries that
    /// existed at ledger start (i.e., in the bucket list). This is required because
    /// get_entry_ttl() uses get_ttl_at_ledger_start() for parallel Soroban isolation.
    #[test]
    fn test_get_local_with_valid_ttl() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        let contract_hash = [1u8; 32];
        let key = make_contract_data_key(contract_hash, 42);
        let entry = make_contract_data_entry(contract_hash, 42, 100);

        // Create the contract data entry
        state.create_contract_data(entry);

        // Create TTL entry with live_until >= current_ledger (valid)
        let key_hash = crate::soroban::compute_key_hash(&key);
        let ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: current_ledger + 100, // Valid: 1100 >= 1000
        };
        state.create_ttl(ttl);

        // Capture the TTL snapshot to simulate entries existing at ledger start.
        // This is necessary because get_entry_ttl() uses get_ttl_at_ledger_start()
        // for parallel Soroban execution isolation.
        state.capture_ttl_bucket_list_snapshot();

        // Test get_local returns the entry
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        let (entry, live_until) = result.expect("Entry with valid TTL should be returned");
        assert_eq!(live_until, Some(current_ledger + 100));
        assert!(matches!(entry.data, LedgerEntryData::ContractData(_)));
    }

    /// Test that get_local returns None when TTL is expired (live_until < current_ledger).
    ///
    /// This matches stellar-core behavior where entries with expired TTL are not
    /// passed to the Soroban host during invoke_host_function.
    ///
    /// Note: We call capture_ttl_bucket_list_snapshot() to simulate entries that
    /// existed at ledger start (i.e., in the bucket list). This is required because
    /// get_entry_ttl() uses get_ttl_at_ledger_start() for parallel Soroban isolation.
    #[test]
    fn test_get_local_with_expired_ttl() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        let contract_hash = [2u8; 32];
        let key = make_contract_data_key(contract_hash, 42);
        let entry = make_contract_data_entry(contract_hash, 42, 100);

        // Create the contract data entry
        state.create_contract_data(entry);

        // Create TTL entry with live_until < current_ledger (expired)
        let key_hash = crate::soroban::compute_key_hash(&key);
        let ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: current_ledger - 1, // Expired: 999 < 1000
        };
        state.create_ttl(ttl);

        // Capture the TTL snapshot to simulate entries existing at ledger start.
        state.capture_ttl_bucket_list_snapshot();

        // Test get_local returns None for expired entry
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        assert!(
            result.is_none(),
            "Entry with expired TTL should not be returned"
        );
    }

    /// Test that get_local returns None when TTL entry doesn't exist.
    ///
    /// This matches stellar-core behavior where entries without a TTL entry
    /// are not considered live and are not passed to the Soroban host.
    #[test]
    fn test_get_local_without_ttl() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        let contract_hash = [3u8; 32];
        let key = make_contract_data_key(contract_hash, 42);
        let entry = make_contract_data_entry(contract_hash, 42, 100);

        // Create the contract data entry but NO TTL entry
        state.create_contract_data(entry);

        // Test get_local returns None when TTL doesn't exist
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        assert!(result.is_none(), "Entry without TTL should not be returned");
    }

    /// Test that get_local returns entry when TTL equals current_ledger (boundary case).
    ///
    /// An entry with live_until == current_ledger is still live (it lives through
    /// the current ledger).
    ///
    /// Note: We call capture_ttl_bucket_list_snapshot() to simulate entries that
    /// existed at ledger start (i.e., in the bucket list). This is required because
    /// get_entry_ttl() uses get_ttl_at_ledger_start() for parallel Soroban isolation.
    #[test]
    fn test_get_local_with_ttl_at_boundary() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        let contract_hash = [4u8; 32];
        let key = make_contract_data_key(contract_hash, 42);
        let entry = make_contract_data_entry(contract_hash, 42, 100);

        // Create the contract data entry
        state.create_contract_data(entry);

        // Create TTL entry with live_until == current_ledger (boundary - still live)
        let key_hash = crate::soroban::compute_key_hash(&key);
        let ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: current_ledger, // Boundary: 1000 >= 1000
        };
        state.create_ttl(ttl);

        // Capture the TTL snapshot to simulate entries existing at ledger start.
        state.capture_ttl_bucket_list_snapshot();

        // Test get_local returns the entry (boundary is still live)
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        assert!(
            result.is_some(),
            "Entry with TTL at boundary should be returned (still live)"
        );
    }

    /// Test that non-Soroban entries (Account, Trustline) are returned without TTL check.
    #[test]
    fn test_get_local_account_no_ttl_required() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        // Create an account
        let account_id = AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([5u8; 32]),
        ));
        let account = stellar_xdr::curr::AccountEntry {
            account_id: account_id.clone(),
            balance: 1_000_000_000,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: stellar_xdr::curr::Thresholds([1, 0, 0, 0]),
            signers: stellar_xdr::curr::VecM::default(),
            ext: stellar_xdr::curr::AccountEntryExt::V0,
        };
        state.create_account(account);

        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        // Test get_local returns the account (no TTL required for classic entries)
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        assert!(
            result.is_some(),
            "Account entry should be returned without TTL"
        );
        let (entry, live_until) = result.unwrap();
        assert!(live_until.is_none(), "Account should have no TTL");
        assert!(matches!(entry.data, LedgerEntryData::Account(_)));
    }

    /// Test that entries created WITHIN the current ledger are NOT visible to Soroban.
    ///
    /// This is a critical regression test for parallel Soroban execution (V1 phases).
    /// In stellar-core, transactions in different clusters of the same stage
    /// should NOT see each other's changes. This is achieved by using the bucket list
    /// snapshot at ledger start for TTL lookups.
    ///
    /// Scenario:
    /// 1. Ledger starts, bucket list snapshot is captured (empty TTL snapshot)
    /// 2. TX 4 creates a new ContractData entry with TTL
    /// 3. TX 5 tries to read that entry
    /// 4. TX 5 should NOT see the entry (TTL not in bucket list snapshot)
    ///
    /// This test verifies the fix for ledger 842789 TX 5 mismatch where our code
    /// incorrectly showed the entry as visible, causing TX 5 to succeed when it
    /// should have trapped.
    #[test]
    fn test_get_local_entry_created_within_ledger_not_visible() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let current_ledger = 1000u32;

        // Step 1: Capture the bucket list snapshot BEFORE any entries are created.
        // This simulates the start of the ledger before any transactions run.
        state.capture_ttl_bucket_list_snapshot();

        // Step 2: TX 4 creates a new ContractData entry with TTL.
        // This simulates an earlier transaction creating an entry within the ledger.
        let contract_hash = [6u8; 32];
        let key = make_contract_data_key(contract_hash, 42);
        let entry = make_contract_data_entry(contract_hash, 42, 100);
        state.create_contract_data(entry);

        let key_hash = crate::soroban::compute_key_hash(&key);
        let ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: current_ledger + 100, // Valid TTL
        };
        state.create_ttl(ttl);

        // Verify the entry exists in current state
        assert!(
            state.get_ttl(&key_hash).is_some(),
            "TTL should exist in current state"
        );

        // Step 3: TX 5 tries to read the entry via LedgerSnapshotAdapter.
        // Since the TTL was created AFTER the bucket list snapshot was captured,
        // get_ttl_at_ledger_start() should return None.
        let snapshot = LedgerSnapshotAdapter::new(&state, current_ledger);
        let result = snapshot.get_local(&key).expect("get_local should succeed");

        // Step 4: The entry should NOT be visible because its TTL is not in the
        // bucket list snapshot (it was created within the ledger).
        assert!(
            result.is_none(),
            "Entry created within ledger should NOT be visible to subsequent Soroban TXs"
        );
    }
}
