//! Cross-version XDR byte-level conversion helpers.
//!
//! Henyey uses workspace XDR types (`stellar_xdr`, currently v27-aligned)
//! as its canonical representation, but must convert to protocol-specific
//! `soroban-env-host` types (P24, P25, P26) for execution and rent calculation.
//!
//! Each `soroban-env-host` pins its own XDR (P24→v24, P25→v25, P26→v26), all
//! distinct from the workspace XDR, so byte-level conversion is required for
//! every host version.
//!
//! These conversions serialize to XDR bytes and deserialize into the target
//! version's types. This works because the wire format is compatible across
//! versions for these types (e.g., `ContractCostParams` is a length-prefixed
//! array with the same entry format).
//!
//! **stellar-core does not need these conversions** — it links a single host
//! version at build time. This bridging layer is henyey-specific.

use soroban_env_host_p24 as soroban_env_host24;
use soroban_env_host_p25 as soroban_env_host25;
use soroban_env_host_p26 as soroban_env_host26;
use stellar_xdr::{ContractCostParams, LedgerEntry, Limits, WriteXdr};

/// Error from cross-version XDR byte-level conversion.
///
/// Captures the conversion phase (serialize vs. deserialize), the target type
/// name, and the underlying XDR error. Callers decide logging policy — this
/// type does not log.
#[derive(Debug, thiserror::Error)]
#[error("XDR cross-version conversion failed ({phase} {type_name}): {source}")]
pub struct XdrConversionError {
    /// Whether the failure was during serialization or deserialization.
    pub phase: &'static str,
    /// Human-readable name of the target type.
    pub type_name: &'static str,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync>,
}

/// Convert workspace (v26) `ContractCostParams` to P25 `ContractCostParams`.
///
/// v26 may have more cost type entries than v25 knows about (e.g., 86 vs 85).
/// The XDR encoding is a length-prefixed array, so v25 will accept any count
/// up to its max (1024). Both versions use the same wire format for
/// `ContractCostParamEntry`, so the byte-level roundtrip works correctly.
pub fn try_convert_cost_params_ws_to_p25(
    params: &ContractCostParams,
) -> Result<soroban_env_host25::xdr::ContractCostParams, XdrConversionError> {
    let bytes = params
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "ContractCostParams",
            source: e.into(),
        })?;
    use soroban_env_host25::xdr::ReadXdr as ReadXdrP25;
    soroban_env_host25::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host25::xdr::Limits::none(),
    )
    .map_err(|e| XdrConversionError {
        phase: "deserialize",
        type_name: "P25 ContractCostParams",
        source: e.into(),
    })
}

/// Convert workspace (v26) `LedgerEntry` to P25 `LedgerEntry`.
pub fn try_convert_ledger_entry_ws_to_p25(
    entry: &LedgerEntry,
) -> Result<soroban_env_host25::xdr::LedgerEntry, XdrConversionError> {
    let bytes = entry
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "LedgerEntry",
            source: e.into(),
        })?;
    use soroban_env_host25::xdr::ReadXdr as ReadXdrP25;
    soroban_env_host25::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host25::xdr::Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "deserialize",
            type_name: "P25 LedgerEntry",
            source: e.into(),
        })
}

/// Convert workspace (v26) `ContractCostParams` to P24 `ContractCostParams`.
pub fn try_convert_cost_params_to_p24(
    params: &ContractCostParams,
) -> Result<soroban_env_host24::xdr::ContractCostParams, XdrConversionError> {
    let bytes = params
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "ContractCostParams",
            source: e.into(),
        })?;
    use soroban_env_host24::xdr::ReadXdr as ReadXdrP24;
    soroban_env_host24::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host24::xdr::Limits::none(),
    )
    .map_err(|e| XdrConversionError {
        phase: "deserialize",
        type_name: "P24 ContractCostParams",
        source: e.into(),
    })
}

/// Convert workspace (v26) `LedgerEntry` to P24 `LedgerEntry`.
pub fn try_convert_ledger_entry_to_p24(
    entry: &LedgerEntry,
) -> Result<soroban_env_host24::xdr::LedgerEntry, XdrConversionError> {
    let bytes = entry
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "LedgerEntry",
            source: e.into(),
        })?;
    use soroban_env_host24::xdr::ReadXdr as ReadXdrP24;
    soroban_env_host24::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "deserialize",
            type_name: "P24 LedgerEntry",
            source: e.into(),
        })
}

/// Convert workspace `ContractCostParams` to P26 `ContractCostParams`.
pub fn try_convert_cost_params_ws_to_p26(
    params: &ContractCostParams,
) -> Result<soroban_env_host26::xdr::ContractCostParams, XdrConversionError> {
    let bytes = params
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "ContractCostParams",
            source: e.into(),
        })?;
    use soroban_env_host26::xdr::ReadXdr as ReadXdrP26;
    soroban_env_host26::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host26::xdr::Limits::none(),
    )
    .map_err(|e| XdrConversionError {
        phase: "deserialize",
        type_name: "P26 ContractCostParams",
        source: e.into(),
    })
}

/// Convert workspace `LedgerEntry` to P26 `LedgerEntry`.
pub fn try_convert_ledger_entry_ws_to_p26(
    entry: &LedgerEntry,
) -> Result<soroban_env_host26::xdr::LedgerEntry, XdrConversionError> {
    let bytes = entry
        .to_xdr(Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "serialize",
            type_name: "LedgerEntry",
            source: e.into(),
        })?;
    use soroban_env_host26::xdr::ReadXdr as ReadXdrP26;
    soroban_env_host26::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host26::xdr::Limits::none())
        .map_err(|e| XdrConversionError {
            phase: "deserialize",
            type_name: "P26 LedgerEntry",
            source: e.into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        ContractCostParamEntry, ContractDataDurability, ContractDataEntry, ContractId,
        ExtensionPoint, Hash, LedgerEntryData, LedgerEntryExt, ScAddress, ScVal,
    };

    fn sample_cost_params() -> ContractCostParams {
        ContractCostParams(
            vec![ContractCostParamEntry {
                ext: ExtensionPoint::V0,
                const_term: 100,
                linear_term: 10,
            }]
            .try_into()
            .unwrap(),
        )
    }

    fn sample_ledger_entry() -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                key: ScVal::Void,
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_try_convert_cost_params_ws_to_p25_roundtrip() {
        let params = sample_cost_params();
        let p25 = try_convert_cost_params_ws_to_p25(&params).unwrap();
        assert_eq!(p25.0.len(), 1);
    }

    #[test]
    fn test_try_convert_cost_params_ws_to_p25_empty() {
        let params = ContractCostParams(vec![].try_into().unwrap());
        let p25 = try_convert_cost_params_ws_to_p25(&params).unwrap();
        assert_eq!(p25.0.len(), 0);
    }

    #[test]
    fn test_try_convert_ledger_entry_ws_to_p25_roundtrip() {
        let entry = sample_ledger_entry();
        let p25 = try_convert_ledger_entry_ws_to_p25(&entry).unwrap();
        assert_eq!(p25.last_modified_ledger_seq, 100);
    }

    #[test]
    fn test_try_convert_cost_params_to_p24_roundtrip() {
        let params = sample_cost_params();
        let p24 = try_convert_cost_params_to_p24(&params).unwrap();
        assert_eq!(p24.0.len(), 1);
    }

    #[test]
    fn test_try_convert_ledger_entry_to_p24_roundtrip() {
        let entry = sample_ledger_entry();
        let p24 = try_convert_ledger_entry_to_p24(&entry).unwrap();
        assert_eq!(p24.last_modified_ledger_seq, 100);
    }

    #[test]
    fn test_try_convert_cost_params_ws_to_p26_roundtrip() {
        let params = sample_cost_params();
        let p26 = try_convert_cost_params_ws_to_p26(&params).unwrap();
        assert_eq!(p26.0.len(), 1);
    }

    #[test]
    fn test_try_convert_ledger_entry_ws_to_p26_roundtrip() {
        let entry = sample_ledger_entry();
        let p26 = try_convert_ledger_entry_ws_to_p26(&entry).unwrap();
        assert_eq!(p26.last_modified_ledger_seq, 100);
    }

    #[test]
    fn test_conversion_error_on_incompatible_bytes() {
        // Serialize a LedgerEntry and try to deserialize as ContractCostParams —
        // this should fail at the deserialization step.
        let entry = sample_ledger_entry();
        let entry_bytes = entry.to_xdr(Limits::none()).unwrap();

        // Manually attempt deserialization as P25 ContractCostParams
        use soroban_env_host25::xdr::ReadXdr as ReadXdrP25;
        let result = soroban_env_host25::xdr::ContractCostParams::from_xdr(
            &entry_bytes,
            soroban_env_host25::xdr::Limits::none(),
        );
        assert!(
            result.is_err(),
            "Deserializing LedgerEntry bytes as ContractCostParams should fail"
        );
    }

    #[test]
    fn test_xdr_conversion_error_display() {
        let err = XdrConversionError {
            phase: "deserialize",
            type_name: "P25 ContractCostParams",
            source: "test error".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("deserialize"));
        assert!(msg.contains("P25 ContractCostParams"));
    }
}
