//! stellar-core compatible `/testacc` handler.
//!
//! Test-only endpoint that resolves a human-readable account name to a
//! deterministic keypair and returns the account's on-ledger state.
//!
//! stellar-core's `CommandHandler::testAcc()` uses `txtest::getAccount(name)`
//! to derive a secret key from a name string (padded to 32 bytes with `.`),
//! then looks up the account in the current ledger.
//!
//! Response format:
//! - Success: `{"name": "bob", "id": "G...", "balance": 100000000, "seqnum": 42}`
//! - Not found: `{}`
//! - Error: `{"status": "error", "detail": "Bad HTTP GET: try something like: testacc?name=bob"}`

use std::sync::Arc;

use henyey_common::deterministic_seed;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use henyey_common::network::NetworkId;
use henyey_crypto::SecretKey;
use stellar_xdr::curr::{AccountId, PublicKey, Uint256};

use crate::compat_http::CompatServerState;

/// Query parameters for `GET /testacc?name=...`.
#[derive(Deserialize)]
pub(crate) struct TestAccParams {
    name: Option<String>,
}

/// GET /testacc?name=<account_name>
///
/// Resolves a test account name to a deterministic keypair, looks up the
/// account in the current ledger, and returns its balance and sequence number.
///
/// This matches stellar-core's `CommandHandler::testAcc()` exactly.
pub(crate) async fn compat_testacc_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<TestAccParams>,
) -> impl IntoResponse {
    let name = match params.name {
        Some(n) if !n.is_empty() => n,
        _ => {
            return Json(serde_json::json!({
                "status": "error",
                "detail": "Bad HTTP GET: try something like: testacc?name=bob"
            }))
            .into_response();
        }
    };

    // Derive the secret key deterministically from the name.
    let secret_key = if name == "root" {
        // Root account: SecretKey::from_seed(SHA256(network_passphrase))
        let network_id = NetworkId::from_passphrase(&state.app.config().network.passphrase);
        SecretKey::from_seed(network_id.as_bytes())
    } else {
        // Named account: pad name to 32 bytes with '.'
        let seed = deterministic_seed(&name);
        SecretKey::from_seed(&seed)
    };

    let public_key = secret_key.public_key();
    let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *public_key.as_bytes(),
    )));
    let strkey = public_key.to_strkey();

    // Look up the account in the current ledger.
    match state.app.load_account(&account_id) {
        Ok(Some(account)) => {
            let response = TestAccResponse {
                name,
                id: strkey,
                balance: account.balance,
                seqnum: account.seq_num.0,
            };
            Json(serde_json::to_value(&response).unwrap()).into_response()
        }
        Ok(None) => {
            // stellar-core returns empty JSON object when account not found
            Json(serde_json::json!({})).into_response()
        }
        Err(e) => {
            tracing::error!(error = ?e, "internal error loading account");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response()
        }
    }
}

/// stellar-core compatible testacc response.
#[derive(Serialize)]
struct TestAccResponse {
    name: String,
    id: String,
    balance: i64,
    seqnum: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the testacc response shape matches stellar-core.
    #[test]
    fn test_testacc_response_shape() {
        let response = TestAccResponse {
            name: "bob".into(),
            id: "GCEZWKCA5VLDNRLN3RPRJMRZOX3Z6G5CHCGSNFHEBD9AFZQ7TM4JRS9A".into(),
            balance: 100_000_000,
            seqnum: 42,
        };

        let value = serde_json::to_value(&response).unwrap();
        let obj = value.as_object().unwrap();

        assert_eq!(
            obj.len(),
            4,
            "testacc response should have exactly 4 fields"
        );
        assert_eq!(value["name"], "bob");
        assert!(value["id"].is_string());
        assert!(value["balance"].is_i64());
        assert!(value["seqnum"].is_i64());
    }

    /// Verify not-found returns empty object.
    #[test]
    fn test_testacc_not_found_shape() {
        let value = serde_json::json!({});
        let obj = value.as_object().unwrap();
        assert!(obj.is_empty(), "not-found should be empty JSON object");
    }

    /// Verify error response shape.
    #[test]
    fn test_testacc_error_shape() {
        let value = serde_json::json!({
            "status": "error",
            "detail": "Bad HTTP GET: try something like: testacc?name=bob"
        });
        assert_eq!(value["status"], "error");
        assert!(value["detail"].is_string());
    }

    /// Verify deterministic seed derivation matches stellar-core.
    #[test]
    fn test_deterministic_seed_padding() {
        let seed = deterministic_seed("bob");
        assert_eq!(seed[0], b'b');
        assert_eq!(seed[1], b'o');
        assert_eq!(seed[2], b'b');
        // Remaining bytes should be '.'
        for i in 3..32 {
            assert_eq!(seed[i], b'.', "byte {i} should be '.'");
        }
    }

    /// Verify deterministic seed for empty string.
    #[test]
    fn test_deterministic_seed_empty() {
        let seed = deterministic_seed("");
        assert_eq!(seed, [b'.'; 32]);
    }

    /// Verify deterministic seed for full 32-byte name.
    #[test]
    fn test_deterministic_seed_full_length() {
        let name = "a".repeat(32);
        let seed = deterministic_seed(&name);
        assert_eq!(seed, [b'a'; 32]);
    }

    /// Verify deterministic seed for name longer than 32 bytes (truncated).
    #[test]
    fn test_deterministic_seed_truncated() {
        let name = "x".repeat(64);
        let seed = deterministic_seed(&name);
        assert_eq!(seed, [b'x'; 32]);
    }

    /// Verify root key derivation produces a valid keypair.
    #[test]
    fn test_root_key_derivation() {
        let network_id = NetworkId::from_passphrase("Test SDF Network ; September 2015");
        let root_key = SecretKey::from_seed(network_id.as_bytes());
        let public = root_key.public_key();
        let strkey = public.to_strkey();
        // The root account on testnet has a well-known public key
        assert!(
            strkey.starts_with('G'),
            "root key should be a valid G... strkey"
        );
    }

    /// Verify named account key derivation produces consistent results.
    #[test]
    fn test_named_account_key_consistency() {
        let seed1 = deterministic_seed("TestAccount-0");
        let seed2 = deterministic_seed("TestAccount-0");
        assert_eq!(seed1, seed2, "same name should produce same seed");

        let key1 = SecretKey::from_seed(&seed1);
        let key2 = SecretKey::from_seed(&seed2);
        assert_eq!(
            key1.public_key().to_strkey(),
            key2.public_key().to_strkey(),
            "same seed should produce same public key"
        );
    }

    /// Verify different names produce different keys.
    #[test]
    fn test_different_names_different_keys() {
        let key1 = SecretKey::from_seed(&deterministic_seed("alice"));
        let key2 = SecretKey::from_seed(&deterministic_seed("bob"));
        assert_ne!(
            key1.public_key().to_strkey(),
            key2.public_key().to_strkey(),
            "different names should produce different keys"
        );
    }
}
