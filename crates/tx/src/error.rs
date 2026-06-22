//! Error types for transaction processing.
//!
//! This module defines the main error type [`TxError`] used throughout the crate
//! for handling failures in transaction validation, execution, and state operations.
//!
//! # Error Categories
//!
//! - **Validation errors**: Fee, sequence, signature, and structure issues
//! - **Execution errors**: Operation failures, Soroban errors
//! - **State errors**: Ledger access and crypto failures
//! - **Internal errors**: XDR serialization and unexpected conditions

use thiserror::Error;

/// Errors that can occur during transaction processing.
///
/// This enum covers all error conditions that can arise during transaction
/// validation, execution, and state management. It uses `thiserror` for
/// convenient error message formatting and trait implementations.
///
/// # Example
///
/// ```ignore
/// use henyey_tx::TxError;
///
/// fn process_tx() -> Result<(), TxError> {
///     // ... processing logic ...
///     Err(TxError::InsufficientFee {
///         required: 200,
///         provided: 100,
///     })
/// }
/// ```
#[derive(Debug, Error)]
pub enum TxError {
    /// Transaction validation failed.
    #[error("transaction validation failed: {0}")]
    ValidationFailed(String),

    /// Invalid signature.
    #[error("invalid signature")]
    InvalidSignature,

    /// Insufficient fee.
    #[error("insufficient fee: required {required}, got {provided}")]
    InsufficientFee { required: i64, provided: i64 },

    /// Bad sequence number.
    #[error("bad sequence number: expected {expected}, got {actual}")]
    BadSequence { expected: i64, actual: i64 },

    /// Source account not found.
    #[error("source account not found")]
    SourceAccountNotFound,

    /// Account not found (with context).
    #[error("account not found: {0}")]
    AccountNotFound(String),

    /// Insufficient balance.
    #[error("insufficient balance: required {required}, available {available}")]
    InsufficientBalance { required: i64, available: i64 },

    /// Operation failed.
    #[error("operation failed: {0}")]
    OperationFailed(String),

    /// Soroban execution error.
    #[error("Soroban error: {0}")]
    Soroban(String),

    /// Ledger error.
    #[error("ledger error: {0}")]
    LedgerError(String),

    /// Crypto error.
    #[error("crypto error: {0}")]
    Crypto(#[from] henyey_crypto::CryptoError),

    /// XDR error.
    #[error("XDR error: {0}")]
    Xdr(#[from] stellar_xdr::Error),

    /// Checked arithmetic error (balance overflow, underflow, etc.).
    #[error("balance error: {0}")]
    Balance(#[from] henyey_common::checked_types::BalanceError),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Sponsorship capacity exceeded (num_sponsoring or num_sponsored overflow).
    /// Maps to opTOO_MANY_SPONSORING at the operation result level.
    #[error("too many sponsoring")]
    TooManySponsoring,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test TxError::ValidationFailed display.
    #[test]
    fn test_validation_failed_display() {
        let err = TxError::ValidationFailed("bad structure".to_string());
        let display = format!("{}", err);
        assert!(display.contains("validation failed"));
        assert!(display.contains("bad structure"));
    }

    /// Test TxError::InvalidSignature display.
    #[test]
    fn test_invalid_signature_display() {
        let err = TxError::InvalidSignature;
        let display = format!("{}", err);
        assert!(display.contains("invalid signature"));
    }

    /// Test TxError::InsufficientFee display.
    #[test]
    fn test_insufficient_fee_display() {
        let err = TxError::InsufficientFee {
            required: 200,
            provided: 100,
        };
        let display = format!("{}", err);
        assert!(display.contains("insufficient fee"));
        assert!(display.contains("200"));
        assert!(display.contains("100"));
    }

    /// Test TxError::BadSequence display.
    #[test]
    fn test_bad_sequence_display() {
        let err = TxError::BadSequence {
            expected: 10,
            actual: 5,
        };
        let display = format!("{}", err);
        assert!(display.contains("bad sequence"));
        assert!(display.contains("10"));
        assert!(display.contains("5"));
    }

    /// Test TxError::SourceAccountNotFound display.
    #[test]
    fn test_source_account_not_found_display() {
        let err = TxError::SourceAccountNotFound;
        let display = format!("{}", err);
        assert!(display.contains("source account not found"));
    }

    /// Test TxError::AccountNotFound display.
    #[test]
    fn test_account_not_found_display() {
        let err = TxError::AccountNotFound("GA123...".to_string());
        let display = format!("{}", err);
        assert!(display.contains("account not found"));
        assert!(display.contains("GA123"));
    }

    /// Test TxError::InsufficientBalance display.
    #[test]
    fn test_insufficient_balance_display() {
        let err = TxError::InsufficientBalance {
            required: 1000,
            available: 500,
        };
        let display = format!("{}", err);
        assert!(display.contains("insufficient balance"));
        assert!(display.contains("1000"));
        assert!(display.contains("500"));
    }

    /// Test TxError::OperationFailed display.
    #[test]
    fn test_operation_failed_display() {
        let err = TxError::OperationFailed("payment failed".to_string());
        let display = format!("{}", err);
        assert!(display.contains("operation failed"));
        assert!(display.contains("payment failed"));
    }

    /// Test TxError::Soroban display.
    #[test]
    fn test_soroban_display() {
        let err = TxError::Soroban("contract trapped".to_string());
        let display = format!("{}", err);
        assert!(display.contains("Soroban"));
        assert!(display.contains("contract trapped"));
    }

    /// Test TxError::LedgerError display.
    #[test]
    fn test_ledger_error_display() {
        let err = TxError::LedgerError("entry not found".to_string());
        let display = format!("{}", err);
        assert!(display.contains("ledger error"));
        assert!(display.contains("entry not found"));
    }

    /// Test TxError::Internal display.
    #[test]
    fn test_internal_display() {
        let err = TxError::Internal("unexpected state".to_string());
        let display = format!("{}", err);
        assert!(display.contains("internal error"));
        assert!(display.contains("unexpected state"));
    }

    /// Test TxError Debug implementation.
    #[test]
    fn test_tx_error_debug() {
        let err = TxError::InsufficientFee {
            required: 100,
            provided: 50,
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("InsufficientFee"));
    }
}
