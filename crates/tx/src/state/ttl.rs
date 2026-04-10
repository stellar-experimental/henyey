//! TTL (time-to-live) entry management for Soroban contract state expiration.

use super::*;
use henyey_common::LedgerSeq;

impl LedgerStateManager {
    /// Get a TTL entry by key hash (read-only).
    pub fn get_ttl(&self, key_hash: &Hash) -> Option<&TtlEntry> {
        self.ttl_entries.get(key_hash)
    }

    /// Get the TTL live_until_ledger_seq at ledger start.
    ///
    /// This returns the TTL value from the bucket list snapshot captured at the
    /// start of the ledger, before any transactions modified it. This is used
    /// by Soroban execution to match stellar-core behavior where transactions
    /// see the bucket list state at ledger start, not changes from previous txs.
    pub fn get_ttl_at_ledger_start(&self, key_hash: &Hash) -> Option<u32> {
        self.ttl_bucket_list_snapshot.get(key_hash).copied()
    }

    /// Capture the current TTL values as the bucket list snapshot.
    ///
    /// This should be called once at the start of each ledger, after loading
    /// state from the bucket list but before executing any transactions.
    /// The captured values will be used by Soroban for TTL lookups to ensure
    /// consistent behavior with stellar-core.
    pub fn capture_ttl_bucket_list_snapshot(&mut self) {
        self.ttl_bucket_list_snapshot.clear();
        for (key_hash, ttl) in &self.ttl_entries {
            self.ttl_bucket_list_snapshot
                .insert(key_hash.clone(), ttl.live_until_ledger_seq);
        }
    }

    /// Get a mutable reference to a TTL entry.
    pub fn get_ttl_mut(&mut self, key_hash: &Hash) -> Option<&mut TtlEntry> {
        if self.ttl_entries.contains_key(key_hash) {
            // Save snapshot if not already saved or if it's None (for newly created entries).
            // For newly created entries, we update the snapshot to the current value so
            // subsequent operations can track changes with STATE/UPDATED pairs.
            // Rollback correctness is ensured by the created_ttl set.
            if !self
                .ttl_snapshots
                .get(key_hash)
                .is_some_and(|s| s.is_some())
            {
                let snapshot = self.ttl_entries.get(key_hash).cloned();
                self.ttl_snapshots.insert(key_hash.clone(), snapshot);
            }
            let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
                key_hash: key_hash.clone(),
            });
            self.capture_op_snapshot_for_key(&ledger_key);
            self.snapshot_last_modified_key(&ledger_key);
            // Track modification
            if !self.modified_ttl.contains(key_hash) {
                self.modified_ttl.push(key_hash.clone());
            }
            self.ttl_entries.get_mut(key_hash)
        } else {
            None
        }
    }

    /// Create a new TTL entry.
    pub fn create_ttl(&mut self, entry: TtlEntry) {
        let key_hash = entry.key_hash.clone();
        let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: key_hash.clone(),
        });

        tracing::debug!(
            key_hash = ?entry.key_hash,
            live_until = entry.live_until_ledger_seq,
            "create_ttl: ENTERING"
        );

        // Save snapshot (None because it didn't exist)
        let existing_snapshot = self.ttl_snapshots.get(&key_hash).cloned();
        self.ttl_snapshots.entry(key_hash.clone()).or_insert(None);
        tracing::debug!(
            key_hash = ?entry.key_hash,
            ?existing_snapshot,
            "create_ttl: snapshot state"
        );
        self.snapshot_last_modified_key(&ledger_key);
        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Record in delta
        let ledger_entry = self.ttl_to_ledger_entry(&entry);
        tracing::debug!(
            key_hash = ?entry.key_hash,
            "create_ttl: calling record_create"
        );
        self.delta.record_create(ledger_entry);

        // Insert into state
        self.ttl_entries.insert(key_hash.clone(), entry);

        // Track that this entry was created in this transaction (for rollback)
        self.created_ttl.insert(key_hash.clone());

        // Track modification
        if !self.modified_ttl.contains(&key_hash) {
            self.modified_ttl.push(key_hash);
        }
    }

    /// Update an existing TTL entry.
    ///
    /// This function only records a delta update if the TTL value actually changes.
    /// This is critical for correct bucket list behavior: when multiple transactions
    /// in the same ledger access the same entry, later transactions may call update_ttl
    /// with a value that earlier transactions already set. Recording a no-op update
    /// would cause bucket list divergence from stellar-core.
    pub fn update_ttl(&mut self, entry: TtlEntry) {
        let key_hash = entry.key_hash.clone();
        let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: key_hash.clone(),
        });

        tracing::debug!(
            key_hash = ?entry.key_hash,
            live_until = entry.live_until_ledger_seq,
            "update_ttl: ENTERING"
        );

        // Check if the TTL value is actually changing
        if let Some(existing) = self.ttl_entries.get(&key_hash) {
            if existing.live_until_ledger_seq == entry.live_until_ledger_seq {
                // TTL value unchanged - skip recording any update.
                // This can happen when multiple transactions in the same ledger
                // access the same entry: TX 5 extends TTL to 700457, then TX 7
                // also tries to update to 700457. From the host's perspective
                // (using ledger-start TTL), TX 7's ttl_extended=true, but the
                // value is already 700457 in our state. Recording this no-op
                // would cause bucket list divergence.
                tracing::debug!(
                    ?key_hash,
                    live_until = entry.live_until_ledger_seq,
                    "TTL update skipped: value unchanged"
                );
                return;
            }
        }

        // Save snapshot if not already saved (preserves original value for rollback)
        if !self.ttl_snapshots.contains_key(&key_hash) {
            let snapshot = self.ttl_entries.get(&key_hash).cloned();
            self.ttl_snapshots.insert(key_hash.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

        // Update state — delta recording is deferred to flush_modified_entries()
        // which compares current state against the snapshot.
        self.ttl_entries.insert(key_hash.clone(), entry);

        // Track modification
        if !self.modified_ttl.contains(&key_hash) {
            self.modified_ttl.push(key_hash);
        }
    }

    /// Update an existing TTL entry without recording in the delta.
    ///
    /// This is used for TTL-only auto-bump changes where the data entry wasn't modified
    /// but the TTL was extended. stellar-core does NOT include these TTL updates
    /// in the transaction meta, so we must update state without creating delta entries.
    ///
    /// The state update is still needed for correct bucket list computation.
    pub fn update_ttl_no_delta(&mut self, entry: TtlEntry) {
        let key_hash = entry.key_hash.clone();
        let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: key_hash.clone(),
        });

        tracing::debug!(
            key_hash = ?entry.key_hash,
            live_until = entry.live_until_ledger_seq,
            "update_ttl_no_delta: updating TTL state without delta"
        );

        // Check if the TTL value is actually changing
        if let Some(existing) = self.ttl_entries.get(&key_hash) {
            if existing.live_until_ledger_seq == entry.live_until_ledger_seq {
                // TTL value unchanged - nothing to do
                return;
            }
        }

        // Update last_modified_key for bucket list computation
        self.set_last_modified_key(ledger_key, self.ledger_seq);

        // Update state only (no delta recording)
        self.ttl_entries.insert(key_hash.clone(), entry.clone());

        // Update snapshot to prevent flush_modified_entries from recording this
        self.ttl_snapshots.insert(key_hash.clone(), Some(entry));

        // Track modification (for bucket list, but not for delta/meta)
        if !self.modified_ttl.contains(&key_hash) {
            self.modified_ttl.push(key_hash);
        }
    }

    /// Record a read-only TTL bump in the delta for transaction meta, then defer
    /// the actual state update.
    ///
    /// Per stellar-core behavior:
    /// - Transaction meta includes all TTL changes (including RO bumps)
    /// - RO TTL bumps are deferred for state visibility (subsequent TXs don't see them)
    /// - At end of ledger, deferred bumps are flushed to state for bucket list
    ///
    /// This method:
    /// 1. Records pre/post state in delta (for transaction meta)
    /// 2. Does NOT update ttl_entries (so subsequent TX lookups return old value)
    /// 3. Stores the bump for later flushing to state
    pub fn record_ro_ttl_bump_for_meta(
        &mut self,
        key_hash: &Hash,
        live_until_ledger_seq: LedgerSeq,
    ) {
        let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: key_hash.clone(),
        });

        // Get pre-state (current value in ttl_entries, NOT including deferred bumps)
        let pre_state = self
            .ttl_entries
            .get(key_hash)
            .map(|ttl| self.ttl_to_ledger_entry(ttl));

        if pre_state.is_none() {
            tracing::warn!(
                key_hash = ?key_hash,
                live_until = live_until_ledger_seq.get(),
                "record_ro_ttl_bump_for_meta: TTL entry not found for RO bump"
            );
            return;
        }

        // Check if TTL is actually changing
        if let Some(existing) = self.ttl_entries.get(key_hash) {
            if existing.live_until_ledger_seq == live_until_ledger_seq.get() {
                // No change needed
                tracing::debug!(
                    key_hash = ?key_hash,
                    live_until = live_until_ledger_seq.get(),
                    "record_ro_ttl_bump_for_meta: skipping - value unchanged"
                );
                return;
            }
        }

        // Capture op snapshot for correct transaction meta ordering
        self.capture_op_snapshot_for_key(&ledger_key);
        // Note: We do NOT call snapshot_last_modified_key or set_last_modified_key here
        // because RO TTL bumps should NOT affect the visible state for subsequent TXs.
        // The lastModifiedLedgerSeq for the pre_state should remain the original value.

        // Build post-state manually with the CURRENT ledger as lastModifiedLedgerSeq.
        // We do NOT call set_last_modified_key because we don't want subsequent TXs
        // to see this change in their pre-state lookups.
        let ttl_entry = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: live_until_ledger_seq.get(),
        };
        let post_state = LedgerEntry {
            last_modified_ledger_seq: self.ledger_seq.get(),
            data: LedgerEntryData::Ttl(ttl_entry),
            ext: self.ledger_entry_ext_for(&ledger_key),
        };

        // Record in delta (for transaction meta) - pre_state -> post_state
        self.delta.record_update(pre_state.unwrap(), post_state);

        tracing::debug!(
            key_hash = ?key_hash,
            live_until = live_until_ledger_seq.get(),
            "record_ro_ttl_bump_for_meta: recorded in delta, deferring state update"
        );

        // Also store for later flushing to state (for bucket list)
        // Only keep the highest TTL bump for each key
        let entry = self
            .deferred_ro_ttl_bumps
            .entry(key_hash.clone())
            .or_insert(0);
        if live_until_ledger_seq.get() > *entry {
            *entry = live_until_ledger_seq.get();
        }
    }

    /// Defer a read-only TTL bump for later flushing (legacy method, prefer record_ro_ttl_bump_for_meta).
    ///
    /// Read-only TTL bumps (TTL changes for entries in the read-only footprint where
    /// only the TTL changed) must NOT appear in transaction meta, but MUST be written
    /// to the bucket list. This matches stellar-core's behavior where RO TTL bumps
    /// are accumulated in `mRoTTLBumps` and flushed at write barriers.
    ///
    /// Call `flush_deferred_ro_ttl_bumps()` at the end of ledger processing to add
    /// these bumps to the delta (after transaction meta is built, before bucket list
    /// is updated).
    pub fn defer_ro_ttl_bump(&mut self, key_hash: &Hash, live_until_ledger_seq: LedgerSeq) {
        // Only keep the highest TTL bump for each key
        let entry = self
            .deferred_ro_ttl_bumps
            .entry(key_hash.clone())
            .or_insert(0);
        if live_until_ledger_seq.get() > *entry {
            *entry = live_until_ledger_seq.get();
        }
        tracing::debug!(
            key_hash = ?key_hash,
            live_until = live_until_ledger_seq.get(),
            "defer_ro_ttl_bump: deferred TTL bump for bucket list"
        );
    }

    /// Flush pending RO TTL bumps for keys in a TX's write footprint.
    ///
    /// This matches stellar-core's `flushRoTTLBumpsInTxWriteFootprint`:
    /// before each TX in a cluster executes, any accumulated RO TTL bumps
    /// for Soroban entries in the TX's read-write footprint are flushed
    /// to `ttl_entries`. This ensures write TXs see bumped TTL values
    /// from earlier TXs' read-only bumps, producing correct rent fee
    /// calculations.
    pub fn flush_ro_ttl_bumps_for_write_footprint(&mut self, write_keys: &[LedgerKey]) {
        for key in write_keys {
            // Only flush for Soroban entry keys (ContractData, ContractCode)
            if !matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)) {
                continue;
            }

            // Compute the TTL key hash (SHA-256 of the XDR-encoded entry key)
            let key_hash = crate::soroban::compute_key_hash(key);

            // Check if there's a pending RO TTL bump for this key
            if let Some(bumped_live_until) = self.deferred_ro_ttl_bumps.remove(&key_hash) {
                if let Some(existing) = self.ttl_entries.get(&key_hash) {
                    if bumped_live_until > existing.live_until_ledger_seq {
                        tracing::debug!(
                            ?key_hash,
                            old_live_until = existing.live_until_ledger_seq,
                            new_live_until = bumped_live_until,
                            "flush_ro_ttl_bumps_for_write_footprint: flushing bump for write key"
                        );
                        let ttl = TtlEntry {
                            key_hash,
                            live_until_ledger_seq: bumped_live_until,
                        };
                        self.update_ttl_no_delta(ttl);
                    }
                }
            }
        }
    }

    /// Flush deferred read-only TTL bumps to state.
    ///
    /// This should be called at the end of cluster processing, after all
    /// transactions have been executed. Any remaining deferred RO TTL bumps
    /// (not already flushed by `flush_ro_ttl_bumps_for_write_footprint`)
    /// are applied to `ttl_entries` so the bucket list sees the final values.
    ///
    /// The delta already has the TTL changes (recorded by record_ro_ttl_bump_for_meta
    /// during transaction execution). This flush only updates state.
    pub fn flush_deferred_ro_ttl_bumps(&mut self) {
        let bumps = std::mem::take(&mut self.deferred_ro_ttl_bumps);
        tracing::debug!(
            count = bumps.len(),
            "flush_deferred_ro_ttl_bumps: starting flush"
        );
        for (key_hash, live_until) in bumps {
            if let Some(existing) = self.ttl_entries.get(&key_hash) {
                // Only update if the deferred bump is higher than current value
                if live_until > existing.live_until_ledger_seq {
                    let ttl = TtlEntry {
                        key_hash: key_hash.clone(),
                        live_until_ledger_seq: live_until,
                    };
                    tracing::debug!(
                        key_hash = ?key_hash,
                        old_live_until = existing.live_until_ledger_seq,
                        new_live_until = live_until,
                        "flush_deferred_ro_ttl_bumps: updating TTL state"
                    );
                    // Use update_ttl_no_delta since the delta already has the change
                    // from record_ro_ttl_bump_for_meta. We just need to update state
                    // for the bucket list to see the final value.
                    self.update_ttl_no_delta(ttl);
                } else {
                    tracing::debug!(
                        key_hash = ?key_hash,
                        existing_live_until = existing.live_until_ledger_seq,
                        deferred_live_until = live_until,
                        "flush_deferred_ro_ttl_bumps: skipping - deferred not higher"
                    );
                }
            } else {
                tracing::warn!(
                    key_hash = ?key_hash,
                    live_until = live_until,
                    "flush_deferred_ro_ttl_bumps: TTL entry not found in state"
                );
            }
        }
    }

    /// Extend the TTL of an entry to the specified ledger sequence.
    pub fn extend_ttl(&mut self, key_hash: &Hash, live_until_ledger_seq: LedgerSeq) {
        if let Some(ttl_entry) = self.ttl_entries.get(key_hash).cloned() {
            // Only extend if the new TTL is greater
            if live_until_ledger_seq > ttl_entry.live_until_ledger_seq {
                // a STATE+UPDATED pair - the CREATED entry should reflect the final value.
                // We update the delta's created entry directly instead.
                if self.created_ttl.contains(key_hash) {
                    // Create updated entry
                    let updated = TtlEntry {
                        key_hash: ttl_entry.key_hash,
                        live_until_ledger_seq: live_until_ledger_seq.get(),
                    };
                    // Update the created entry in delta to reflect final value
                    self.delta.update_created_ttl(key_hash, &updated);
                    // Update state
                    self.ttl_entries.insert(key_hash.clone(), updated);
                } else {
                    // Save snapshot if not already saved (preserves original value for rollback)
                    self.ttl_snapshots
                        .entry(key_hash.clone())
                        .or_insert_with(|| Some(ttl_entry.clone()));
                    let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
                        key_hash: key_hash.clone(),
                    });
                    self.capture_op_snapshot_for_key(&ledger_key);
                    self.snapshot_last_modified_key(&ledger_key);

                    self.set_last_modified_key(ledger_key.clone(), self.ledger_seq);

                    // Create updated entry
                    let updated = TtlEntry {
                        key_hash: ttl_entry.key_hash,
                        live_until_ledger_seq: live_until_ledger_seq.get(),
                    };

                    // Update state — delta recording is deferred to flush_modified_entries()
                    self.ttl_entries.insert(key_hash.clone(), updated);

                    // Track modification
                    if !self.modified_ttl.contains(key_hash) {
                        self.modified_ttl.push(key_hash.clone());
                    }
                }
            }
        }
    }

    /// Delete a TTL entry.
    pub fn delete_ttl(&mut self, key_hash: &Hash) {
        let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: key_hash.clone(),
        });

        // Save snapshot if not already saved
        if !self.ttl_snapshots.contains_key(key_hash) {
            let snapshot = self.ttl_entries.get(key_hash).cloned();
            self.ttl_snapshots.insert(key_hash.clone(), snapshot);
        }
        self.capture_op_snapshot_for_key(&ledger_key);
        self.snapshot_last_modified_key(&ledger_key);

        // Get pre-state (current value BEFORE deletion)
        let pre_state = self
            .ttl_entries
            .get(key_hash)
            .map(|ttl| self.ttl_to_ledger_entry(ttl));

        // Record in delta with pre-state
        if let Some(pre) = pre_state {
            self.delta.record_delete(ledger_key.clone(), pre);
        }

        // Remove from state and track deletion
        self.clear_entry_sponsorship_metadata(&ledger_key);
        self.ttl_entries.remove(key_hash);
        self.remove_last_modified_key(&ledger_key);
        // Track this deletion to prevent reloading from bucket list
        self.deleted_ttl.insert(key_hash.clone());
    }

    /// Check if a TTL entry is live (not expired).
    pub fn is_entry_live(&self, key_hash: &Hash) -> bool {
        if let Some(ttl) = self.get_ttl(key_hash) {
            ttl.live_until_ledger_seq >= self.ledger_seq.get()
        } else {
            false
        }
    }
}
