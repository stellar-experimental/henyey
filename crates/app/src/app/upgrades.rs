//! Upgrade handling for the Stellar application.
//!
//! Contains methods for querying and setting protocol upgrade parameters,
//! including the current upgrade state, proposed upgrades, and config upgrade sets.

use super::App;

impl App {
    pub fn current_upgrade_state(&self) -> (u32, u32, u32, u32) {
        let header = self.ledger_manager.current_header();
        (
            header.ledger_version,
            header.base_fee,
            header.base_reserve,
            header.max_tx_set_size,
        )
    }

    pub fn proposed_upgrades(&self) -> Vec<stellar_xdr::LedgerUpgrade> {
        self.config.upgrades.to_ledger_upgrades()
    }

    /// Set runtime upgrade parameters (from HTTP `/upgrades?mode=set`).
    pub fn set_upgrade_parameters(
        &self,
        params: henyey_herder::upgrades::UpgradeParameters,
    ) -> std::result::Result<(), String> {
        self.herder.set_upgrade_parameters(params)
    }

    /// Get current runtime upgrade parameters.
    pub fn runtime_upgrade_parameters(&self) -> henyey_herder::upgrades::UpgradeParameters {
        self.herder.upgrade_parameters()
    }

    /// Validate a `ConfigUpgradeSetKey` for arm-time acceptance (`/upgrades`).
    ///
    /// Mirrors stellar-core `CommandHandler::upgrades`
    /// (CommandHandler.cpp:634-655): resolve the key via `makeFromKey` and
    /// require `isValidForApply == VALID`. Returns `Ok(true)` only when the
    /// entry resolves to a frame that is `Valid`. Returns `Ok(false)` when the
    /// entry is absent / wrong-durability / TTL-expired (`make_from_key` →
    /// `None`) or resolves to a frame that is not `Valid`.
    ///
    /// The gate is `make_from_key.is_some() && is_valid_for_apply() == Valid`
    /// ONLY — it deliberately EXCLUDES the `upgrade_needed` check, which is
    /// nomination-time only (folding it in would wrongly reject valid no-op
    /// upgrades, a parity divergence — see CommandHandler.cpp:647-651).
    ///
    /// Returns `Err` on I/O errors or invariant violations reading the ledger.
    pub fn validate_config_upgrade_set_key(
        &self,
        key: &stellar_xdr::ConfigUpgradeSetKey,
    ) -> Result<bool, henyey_ledger::LedgerError> {
        let frame = match self.ledger_manager.get_config_upgrade_set(key)? {
            Some(f) => f,
            None => return Ok(false),
        };
        Ok(frame.is_valid_for_apply() == henyey_ledger::ConfigUpgradeValidity::Valid)
    }

    /// Look up a `ConfigUpgradeSet` by key from the current ledger state.
    ///
    /// # Arguments
    ///
    /// * `key` - The ConfigUpgradeSetKey identifying the upgrade set
    ///
    /// # Returns
    ///
    /// * `Ok(Some(json))` - The ConfigUpgradeSet as a JSON-serializable value
    /// * `Ok(None)` - The upgrade set was not found or is invalid
    /// * `Err` - I/O error or invariant violation reading from ledger
    pub fn get_config_upgrade_set(
        &self,
        key: &stellar_xdr::ConfigUpgradeSetKey,
    ) -> Result<Option<serde_json::Value>, henyey_ledger::LedgerError> {
        let frame = match self.ledger_manager.get_config_upgrade_set(key)? {
            Some(f) => f,
            None => return Ok(None),
        };
        let upgrade_set = frame.to_xdr();

        // Convert to JSON-serializable format
        Ok(Some(serde_json::json!({
            "updated_entry": upgrade_set.updated_entry.iter().map(|entry| {
                format!("{:?}", entry)
            }).collect::<Vec<_>>()
        })))
    }
}
