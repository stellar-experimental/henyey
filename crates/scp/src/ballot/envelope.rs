//! Ballot protocol envelope emission and recording.

use super::*;

impl BallotProtocol {
    pub(crate) fn send_latest_envelope<D: SCPDriver>(&mut self, driver: &Arc<D>) {
        if self.current_message_level != 0 {
            return;
        }

        if !self.fully_validated {
            return;
        }

        let Some(envelope) = self.last_envelope.as_ref() else {
            return;
        };

        if self.last_envelope_emit.as_ref() == Some(envelope) {
            return;
        }

        self.last_envelope_emit = Some(envelope.clone());
        driver.emit_envelope(envelope);
    }

    /// Build and record a prepare statement envelope.
    /// Returns the statement if a new envelope was recorded (for self-processing).
    ///
    /// When `current_ballot` is `None` (pristine state, no `bumpState` call),
    /// a PREPARE with `ballot = {0, ""}` is still created and recorded as a
    /// self-envelope. This matches stellar-core `emitCurrentStateStatement` which always
    /// calls `createStatement()` and `processEnvelope(self)`, even when
    /// `mCurrentBallot` is null. The self-envelope is needed so that the local
    /// node counts itself in subsequent quorum calculations (e.g., prepared
    /// fields in the self-envelope contribute to `federated_accept`/`federated_ratify`).
    /// However, the envelope is NOT emitted to the network when `current_ballot`
    /// is `None` (matching stellar-core `canEmit = mCurrentBallot != nullptr`).
    fn emit_prepare<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) -> Option<ScpStatement> {
        // Use the current ballot if set, otherwise use a default zero ballot
        // (matching stellar-core which creates a PREPARE with default ballot {0, ""} when
        // mCurrentBallot is null).
        let can_emit = self.current_ballot.is_some();
        let ballot = self.current_ballot.clone().unwrap_or_else(|| ScpBallot {
            counter: 0,
            value: Value(Vec::new().try_into().unwrap_or_default()),
        });

        let prep = ScpStatementPrepare {
            quorum_set_hash: hash_quorum_set(ctx.local_quorum_set).into(),
            ballot,
            prepared: self.prepared.clone(),
            prepared_prime: self.prepared_prime.clone(),
            n_c: self.commit.as_ref().map(|b| b.counter).unwrap_or(0),
            n_h: self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
        };

        // Only update last_envelope (for network emission) when we have a
        // real ballot. Matches stellar-core `canEmit = mCurrentBallot != nullptr`.
        self.record_envelope(
            ScpStatementPledges::Prepare(prep),
            can_emit,
            ctx.local_node_id,
            ctx.driver,
            ctx.slot_index,
        )
    }

    /// Sign, record, and optionally publish an envelope built from the given pledges.
    ///
    /// Shared scaffolding for emit_prepare / emit_confirm / emit_externalize.
    /// When `set_last` is true the envelope is stored for network emission.
    fn record_envelope<D: SCPDriver>(
        &mut self,
        pledges: ScpStatementPledges,
        set_last: bool,
        local_node_id: &NodeId,
        driver: &Arc<D>,
        slot_index: u64,
    ) -> Option<ScpStatement> {
        let statement = ScpStatement {
            node_id: local_node_id.clone(),
            slot_index,
            pledges,
        };

        let mut envelope = ScpEnvelope {
            statement: statement.clone(),
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        driver.sign_envelope(&mut envelope);
        if self.record_local_envelope(local_node_id, envelope.clone()) {
            if set_last {
                self.last_envelope = Some(envelope);
            }
            return Some(statement);
        }
        None
    }

    /// Build and record a confirm statement envelope.
    /// Returns the statement if a new envelope was recorded (for self-processing).
    fn emit_confirm<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) -> Option<ScpStatement> {
        if let Some(ref ballot) = self.current_ballot {
            let conf = ScpStatementConfirm {
                ballot: ballot.clone(),
                n_prepared: self.prepared.as_ref().map(|b| b.counter).unwrap_or(0),
                n_commit: self.commit.as_ref().map(|b| b.counter).unwrap_or(0),
                n_h: self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
                quorum_set_hash: hash_quorum_set(ctx.local_quorum_set).into(),
            };

            self.record_envelope(
                ScpStatementPledges::Confirm(conf),
                true,
                ctx.local_node_id,
                ctx.driver,
                ctx.slot_index,
            )
        } else {
            None
        }
    }

    /// Build and record an externalize statement envelope.
    /// Returns the statement if a new envelope was recorded (for self-processing).
    fn emit_externalize<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) -> Option<ScpStatement> {
        if let Some(ref commit) = self.commit {
            let ext = ScpStatementExternalize {
                commit: commit.clone(),
                n_h: self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
                commit_quorum_set_hash: hash_quorum_set(ctx.local_quorum_set).into(),
            };

            self.record_envelope(
                ScpStatementPledges::Externalize(ext),
                true,
                ctx.local_node_id,
                ctx.driver,
                ctx.slot_index,
            )
        } else {
            None
        }
    }

    /// Emit current state and recursively self-process (matching stellar-core emitCurrentStateStatement).
    ///
    /// After emitting, feeds the self-envelope back into `advance_slot` so that
    /// cascading state transitions (e.g., accept-prepared → confirm-prepared →
    /// accept-commit) can happen within a single top-level `receiveEnvelope` call.
    /// The `current_message_level` guard in `send_latest_envelope` ensures only the
    /// final envelope is actually emitted to the network.
    pub(super) fn emit_current_state<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) {
        let maybe_statement = match self.phase {
            BallotPhase::Prepare => self.emit_prepare(ctx),
            BallotPhase::Confirm => self.emit_confirm(ctx),
            BallotPhase::Externalize => self.emit_externalize(ctx),
        };
        // Recursive self-processing: feed the self-envelope back into advance_slot
        // so cascading state transitions complete within a single receiveEnvelope.
        // This matches stellar-core emitCurrentStateStatement() calling processEnvelope(self).
        if let Some(statement) = maybe_statement {
            self.advance_slot(&statement, ctx);
        }
        // Emit the latest envelope after self-processing completes.
        // If advance_slot caused cascading state changes, last_envelope
        // was updated to the final envelope and already emitted via
        // advance_slot's send_latest_envelope call. The dedup check in
        // send_latest_envelope (last_envelope_emit) prevents double-emit.
        // If no cascading happened, this ensures the original envelope
        // is emitted. Matches stellar-core sendLatestEnvelope() in
        // emitCurrentStateStatement after processEnvelope(self).
        self.send_latest_envelope(ctx.driver);
    }

    fn record_local_envelope(&mut self, local_node_id: &NodeId, envelope: ScpEnvelope) -> bool {
        if !self.is_newer_statement(local_node_id, &envelope.statement) {
            return false;
        }
        self.latest_envelopes
            .insert(local_node_id.clone(), envelope);
        true
    }
}
