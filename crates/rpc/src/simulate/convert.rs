//! XDR byte-level conversion helpers between workspace (v26) and P25 (v25) types.
//!
//! The `soroban-env-host-p25` crate uses `stellar-xdr` v25, while the workspace
//! uses v26. These types are structurally identical for the XDR types we use, so
//! we convert via serialization round-trip.
//!
//! Both conversion functions return `Result<T, ConversionError>` by default.
//! Callers that need non-fatal behavior (e.g., diagnostic events) can use `.ok()`.

use std::fmt;

use soroban_env_host_p25 as soroban_host;

#[derive(Debug, Clone, Copy)]
pub(super) enum ConversionDirection {
    WsToP25,
    P25ToWs,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ConversionPhase {
    Serialize,
    Deserialize,
}

#[derive(Debug, Clone)]
pub(super) struct ConversionError {
    #[allow(dead_code)]
    pub direction: ConversionDirection,
    pub phase: ConversionPhase,
    pub source_type: &'static str,
    pub target_type: &'static str,
    pub cause: String,
}

impl fmt::Display for ConversionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = match self.phase {
            ConversionPhase::Serialize => "serialize source",
            ConversionPhase::Deserialize => "deserialize target",
        };
        write!(
            f,
            "XDR conversion failed: could not {} ({}→{}): {}",
            phase, self.source_type, self.target_type, self.cause,
        )
    }
}

impl std::error::Error for ConversionError {}

/// Convert a workspace (v26) type to a P25 (v25) type via XDR bytes.
pub(super) fn ws_to_p25<WS, P25>(ws_val: &WS) -> Result<P25, ConversionError>
where
    WS: stellar_xdr::curr::WriteXdr,
    P25: soroban_host::xdr::ReadXdr,
{
    let err = |phase, cause: String| ConversionError {
        direction: ConversionDirection::WsToP25,
        phase,
        source_type: std::any::type_name::<WS>(),
        target_type: std::any::type_name::<P25>(),
        cause,
    };
    let bytes = ws_val
        .to_xdr(stellar_xdr::curr::Limits::none())
        .map_err(|e| err(ConversionPhase::Serialize, e.to_string()))?;
    P25::from_xdr(&bytes, soroban_host::xdr::Limits::none())
        .map_err(|e| err(ConversionPhase::Deserialize, e.to_string()))
}

/// Convert a P25 (v25) type to a workspace (v26) type via XDR bytes.
pub(super) fn p25_to_ws<P25, WS>(p25_val: &P25) -> Result<WS, ConversionError>
where
    P25: soroban_host::xdr::WriteXdr,
    WS: stellar_xdr::curr::ReadXdr,
{
    let err = |phase, cause: String| ConversionError {
        direction: ConversionDirection::P25ToWs,
        phase,
        source_type: std::any::type_name::<P25>(),
        target_type: std::any::type_name::<WS>(),
        cause,
    };
    let bytes = p25_val
        .to_xdr(soroban_host::xdr::Limits::none())
        .map_err(|e| err(ConversionPhase::Serialize, e.to_string()))?;
    WS::from_xdr(&bytes, stellar_xdr::curr::Limits::none())
        .map_err(|e| err(ConversionPhase::Deserialize, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversion_error_display() {
        let err = ConversionError {
            direction: ConversionDirection::WsToP25,
            phase: ConversionPhase::Serialize,
            source_type: "Foo",
            target_type: "Bar",
            cause: "bad data".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("serialize source"));
        assert!(msg.contains("Foo"));
        assert!(msg.contains("Bar"));
        assert!(msg.contains("bad data"));
    }

    #[test]
    fn test_conversion_error_display_deserialize() {
        let err = ConversionError {
            direction: ConversionDirection::P25ToWs,
            phase: ConversionPhase::Deserialize,
            source_type: "Src",
            target_type: "Tgt",
            cause: "invalid bytes".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("deserialize target"));
        assert!(msg.contains("Src→Tgt"));
        assert!(msg.contains("invalid bytes"));
    }

    #[test]
    fn test_ws_to_p25_success() {
        // Round-trip a simple XDR type that exists in both versions.
        let ws_val = stellar_xdr::curr::Uint256([42u8; 32]);
        let result: Result<soroban_host::xdr::Uint256, _> = ws_to_p25(&ws_val);
        assert_eq!(result.unwrap().0, [42u8; 32]);
    }

    #[test]
    fn test_p25_to_ws_success() {
        let p25_val = soroban_host::xdr::Uint256([99u8; 32]);
        let result: Result<stellar_xdr::curr::Uint256, _> = p25_to_ws(&p25_val);
        assert_eq!(result.unwrap().0, [99u8; 32]);
    }

    #[test]
    fn test_p25_to_ws_deserialize_failure() {
        // Uint256 serializes as 32 bytes; trying to deserialize 32 bytes as a
        // Hash (also 32 bytes) should succeed since they're the same wire format.
        // Instead, use a type mismatch: serialize a Uint32 (4 bytes) and try to
        // deserialize as Uint256 (32 bytes).
        let p25_val = soroban_host::xdr::Uint256([0u8; 32]);
        // Serialize Uint256 (32 bytes) and try to read as a LedgerHeader — will fail.
        let result: Result<stellar_xdr::curr::LedgerHeader, _> = p25_to_ws(&p25_val);
        let err = result.unwrap_err();
        assert!(matches!(err.phase, ConversionPhase::Deserialize));
        assert!(matches!(err.direction, ConversionDirection::P25ToWs));
        assert!(!err.cause.is_empty());
    }
}
