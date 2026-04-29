//! Ballot protocol envelope emission and recording.
//!
//! Local self-emission mirrors stellar-core's `emitCurrentStateStatement()` →
//! `Slot::processEnvelope(envW, true)` → `BallotProtocol::processEnvelope(self)`
//! path: sanity check, freshness, value validation, reject invalid, clear
//! fully_validated for MaybeValid, record, advance, emit.

use super::*;
use crate::ValidationLevel;

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

    /// Create and sign an envelope for the given pledges without recording it.
    ///
    /// Returns the statement and signed envelope. Recording is deferred to
    /// `emit_current_state` which validates before storing.
    fn create_signed_envelope<D: SCPDriver>(
        &self,
        pledges: ScpStatementPledges,
        ctx: &SlotContext<'_, D>,
    ) -> (ScpStatement, ScpEnvelope) {
        let statement = ScpStatement {
            node_id: ctx.local_node_id.clone(),
            slot_index: ctx.slot_index,
            pledges,
        };

        let mut envelope = ScpEnvelope {
            statement: statement.clone(),
            signature: stellar_xdr::curr::Signature(Vec::new().try_into().unwrap_or_default()),
        };

        ctx.driver.sign_envelope(&mut envelope);
        (statement, envelope)
    }

    /// Build a PREPARE statement envelope.
    ///
    /// Returns `(statement, envelope, can_emit)`. Always returns Some — even when
    /// `current_ballot` is None, a zero-ballot PREPARE is created (matching
    /// stellar-core `createStatement(SCP_ST_PREPARE)` with null `mCurrentBallot`).
    /// `can_emit` is false for zero-ballot PREPAREs (stellar-core:
    /// `canEmit = mCurrentBallot != nullptr`).
    fn emit_prepare<D: SCPDriver>(
        &self,
        ctx: &SlotContext<'_, D>,
    ) -> Option<(ScpStatement, ScpEnvelope, bool)> {
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

        let (statement, envelope) =
            self.create_signed_envelope(ScpStatementPledges::Prepare(prep), ctx);
        Some((statement, envelope, can_emit))
    }

    /// Build a CONFIRM statement envelope.
    ///
    /// Returns None when `current_ballot` is None (no ballot to confirm).
    fn emit_confirm<D: SCPDriver>(
        &self,
        ctx: &SlotContext<'_, D>,
    ) -> Option<(ScpStatement, ScpEnvelope, bool)> {
        let ballot = self.current_ballot.as_ref()?;
        let conf = ScpStatementConfirm {
            ballot: ballot.clone(),
            n_prepared: self.prepared.as_ref().map(|b| b.counter).unwrap_or(0),
            n_commit: self.commit.as_ref().map(|b| b.counter).unwrap_or(0),
            n_h: self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
            quorum_set_hash: hash_quorum_set(ctx.local_quorum_set).into(),
        };

        let (statement, envelope) =
            self.create_signed_envelope(ScpStatementPledges::Confirm(conf), ctx);
        Some((statement, envelope, true))
    }

    /// Build an EXTERNALIZE statement envelope.
    ///
    /// Returns None when `commit` is None (no commit to externalize).
    fn emit_externalize<D: SCPDriver>(
        &self,
        ctx: &SlotContext<'_, D>,
    ) -> Option<(ScpStatement, ScpEnvelope, bool)> {
        let commit = self.commit.as_ref()?;
        let ext = ScpStatementExternalize {
            commit: commit.clone(),
            n_h: self.high_ballot.as_ref().map(|b| b.counter).unwrap_or(0),
            commit_quorum_set_hash: hash_quorum_set(ctx.local_quorum_set).into(),
        };

        let (statement, envelope) =
            self.create_signed_envelope(ScpStatementPledges::Externalize(ext), ctx);
        Some((statement, envelope, true))
    }

    /// Emit current state with full self-processing validation.
    ///
    /// Mirrors stellar-core `emitCurrentStateStatement()` → `processEnvelope(self)`:
    /// 1. Create and sign the self-envelope
    /// 2. Sanity check (`isStatementSane(statement, self)`)
    /// 3. Freshness check (`isNewerStatement`)
    /// 4. Validate values (`validateValues`)
    /// 5. Reject if Invalid
    /// 6. Clear `fully_validated` if MaybeValid/MaybeValidDeferred (non-Externalize only)
    /// 7. Record envelope
    /// 8. Advance slot (non-Externalize) or just record (Externalize)
    /// 9. Emit via `send_latest_envelope`
    pub(super) fn emit_current_state<D: SCPDriver>(&mut self, ctx: &SlotContext<'_, D>) {
        // 1. Create and sign the self-envelope (phase-specific)
        let Some((statement, envelope, can_emit)) = (match self.phase {
            BallotPhase::Prepare => self.emit_prepare(ctx),
            BallotPhase::Confirm => self.emit_confirm(ctx),
            BallotPhase::Externalize => self.emit_externalize(ctx),
        }) else {
            return;
        };

        // 2. Sanity check (stellar-core: isStatementSane(statement, self))
        // Defense-in-depth: locally generated statements should be sane.
        if !self.is_statement_sane(&statement, ctx) {
            tracing::error!(
                target: "henyey::envelope_path",
                slot = ctx.slot_index,
                "not sane statement from self in emit_current_state, skipping",
            );
            return;
        }

        // 3. Freshness check (stellar-core: isNewerStatement)
        if !self.is_newer_statement(ctx.local_node_id, &statement) {
            return;
        }

        // 4. Validate statement values (stellar-core: validateValues)
        let validation = self.validate_statement_values(&statement, ctx.driver, ctx.slot_index);

        // 5. Reject Invalid (stellar-core: CLOG_ERROR + return INVALID for self)
        if validation == ValidationLevel::Invalid {
            tracing::error!(
                target: "henyey::envelope_path",
                slot = ctx.slot_index,
                "invalid value from self in emit_current_state, skipping",
            );
            return;
        }

        // 6-8. Phase-specific handling
        if self.phase != BallotPhase::Externalize {
            // Non-Externalize: clear fully_validated, record, advance
            // (stellar-core BallotProtocol.cpp:206-215)
            if validation.clears_fully_validated() {
                self.fully_validated = false;
                self.needs_clear_slot_validation = true;
            }

            self.latest_envelopes
                .insert(ctx.local_node_id.clone(), envelope.clone());

            if can_emit {
                self.last_envelope = Some(envelope);
            }

            // Recursive self-processing: advance_slot so cascading state
            // transitions complete within a single receiveEnvelope call.
            self.advance_slot(&statement, ctx);
        } else {
            // Externalize: record only if value matches commit, no advance_slot.
            // (stellar-core BallotProtocol.cpp:220-224)
            if !self.statement_value_matches_commit(&statement) {
                tracing::error!(
                    target: "henyey::envelope_path",
                    slot = ctx.slot_index,
                    "externalize statement with invalid value from self, skipping",
                );
                return;
            }

            self.latest_envelopes
                .insert(ctx.local_node_id.clone(), envelope.clone());

            if can_emit {
                self.last_envelope = Some(envelope);
            }
        }

        // 9. Emit the latest envelope (gated on fully_validated + message_level + dedup)
        self.send_latest_envelope(ctx.driver);
    }
}
