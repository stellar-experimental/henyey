//! Response construction: building the JSON-RPC response from simulation
//! results, encoding XDR fields, error formatting.

use serde_json::json;
use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    HostFunction, InvokeHostFunctionOp, OperationBody, SorobanTransactionData,
    SorobanTransactionDataExt, WriteXdr,
};

use crate::error::JsonRpcError;
use crate::util::{self, XdrFormat};

use super::resources::{adjust_resources, compute_invoke_resource_fee, estimate_tx_size_for_op};
use super::LedgerEntryDiff;

pub(super) struct InvokeResponseContext<'a> {
    pub soroban_info: &'a henyey_ledger::SorobanNetworkInfo,
    pub latest_ledger: u32,
    pub host_fn: &'a HostFunction,
    pub format: XdrFormat,
    pub instruction_leeway: u32,
}

pub(super) fn build_invoke_response(
    sim_result: soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult,
    diagnostic_events: Vec<soroban_host::xdr::DiagnosticEvent>,
    state_changes: Vec<LedgerEntryDiff>,
    ctx: InvokeResponseContext<'_>,
) -> Result<serde_json::Value, JsonRpcError> {
    use super::convert::p25_to_ws;

    // Convert P25 resources to workspace types
    let resources: stellar_xdr::curr::SorobanResources = p25_to_ws(&sim_result.resources)
        .map_err(|e| JsonRpcError::internal_logged("xdr_conversion", &e))?;

    // Apply resource adjustments (mirrors soroban-simulation default_adjustment)
    let mut adjusted_resources = resources.clone();
    adjust_resources(&mut adjusted_resources, ctx.instruction_leeway);

    // Compute rent changes for fee estimation
    let rent_changes = soroban_host::e2e_invoke::extract_rent_changes(&sim_result.ledger_changes);

    // Convert P25 auth to workspace for the InvokeHostFunctionOp
    let ws_auth: Vec<stellar_xdr::curr::SorobanAuthorizationEntry> = sim_result
        .auth
        .iter()
        .map(|a| p25_to_ws(a).map_err(|e| JsonRpcError::internal_logged("xdr_conversion", &e)))
        .collect::<Result<Vec<_>, _>>()?;

    // Estimate the transaction size for fee computation
    let op = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
        host_function: ctx.host_fn.clone(),
        auth: ws_auth[..]
            .try_into()
            .map_err(|_| JsonRpcError::internal("auth entries exceed VecM maximum length"))?,
    });
    let tx_size = estimate_tx_size_for_op(&op, &adjusted_resources);

    // Build SorobanTransactionData
    // Build the extension: V1 with archived entry indices when entries were
    // auto-restored during simulation, V0 otherwise.
    let ext = if sim_result.restored_rw_entry_indices.is_empty() {
        SorobanTransactionDataExt::V0
    } else {
        SorobanTransactionDataExt::V1(stellar_xdr::curr::SorobanResourcesExtV0 {
            archived_soroban_entries: sim_result
                .restored_rw_entry_indices
                .clone()
                .try_into()
                .map_err(|_| {
                    JsonRpcError::internal("restored entry indices exceed VecM maximum length")
                })?,
        })
    };

    let soroban_data = SorobanTransactionData {
        ext,
        resources: adjusted_resources,
        resource_fee: compute_invoke_resource_fee(
            &resources,
            &rent_changes,
            ctx.soroban_info,
            ctx.latest_ledger,
            sim_result.contract_events_and_return_value_size,
            tx_size,
            sim_result.restored_rw_entry_indices.len() as u32,
        ),
    };

    let min_resource_fee = soroban_data.resource_fee;

    let mut obj = serde_json::Map::new();

    // transactionData — upstream uses unsuffixed "transactionData" for base64
    insert_sim_xdr_field(&mut obj, "transactionData", &soroban_data, ctx.format)?;

    obj.insert("minResourceFee".into(), json!(min_resource_fee.to_string()));
    obj.insert(
        "cost".into(),
        json!({
            "cpuInsns": resources.instructions.to_string(),
            "memBytes": "0"
        }),
    );
    obj.insert("latestLedger".into(), json!(ctx.latest_ledger));

    // Diagnostic events — convert P25 to workspace, then serialize.
    // Conversion failures are logged but don't fail the response: diagnostic
    // events are informational and stellar-core treats them as non-fatal.
    if !diagnostic_events.is_empty() {
        let mut ws_events: Vec<stellar_xdr::curr::DiagnosticEvent> =
            Vec::with_capacity(diagnostic_events.len());
        for e in &diagnostic_events {
            match p25_to_ws(e) {
                Ok(ws) => ws_events.push(ws),
                Err(err) => {
                    tracing::warn!("skipping DiagnosticEvent: {err}");
                }
            }
        }
        if !ws_events.is_empty() {
            insert_sim_xdr_array_field(&mut obj, "events", &ws_events, ctx.format)?;
        }
    }

    // Encode auth entries and return value
    let return_value: Option<stellar_xdr::curr::ScVal> = match &sim_result.invoke_result {
        Ok(val) => {
            Some(p25_to_ws(val).map_err(|e| JsonRpcError::internal_logged("xdr_conversion", &e))?)
        }
        Err(_) => None,
    };

    if !ws_auth.is_empty() || return_value.is_some() {
        let mut result_obj = serde_json::Map::new();

        // auth array (already converted to workspace types)
        insert_sim_xdr_array_field(&mut result_obj, "auth", &ws_auth, ctx.format)?;

        // return value
        if let Some(rv) = &return_value {
            util::insert_xdr_field_styled(
                &mut result_obj,
                "xdr",
                rv,
                ctx.format,
                util::XdrKeyStyle::Unsuffixed,
            )?;
        }

        obj.insert(
            "results".into(),
            json!([serde_json::Value::Object(result_obj)]),
        );
    }

    // State changes (ledger entry diffs)
    if !state_changes.is_empty() {
        let changes_json = serialize_state_changes(&state_changes, ctx.format)?;
        obj.insert("stateChanges".into(), changes_json);
    }

    Ok(serde_json::Value::Object(obj))
}

/// Serialize state changes to JSON.
fn serialize_state_changes(
    diffs: &[LedgerEntryDiff],
    format: XdrFormat,
) -> Result<serde_json::Value, JsonRpcError> {
    let mut entries = Vec::with_capacity(diffs.len());

    for diff in diffs {
        let change_type = match (&diff.state_before, &diff.state_after) {
            (None, Some(_)) => "created",
            (Some(_), Some(_)) => "updated",
            (Some(_), None) => "deleted",
            (None, None) => continue,
        };

        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), json!(change_type));

        insert_sim_xdr_field(&mut entry, "key", &diff.key, format)?;
        insert_optional_sim_xdr_field(&mut entry, "before", diff.state_before.as_ref(), format)?;
        insert_optional_sim_xdr_field(&mut entry, "after", diff.state_after.as_ref(), format)?;

        entries.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::Value::Array(entries))
}

/// Build the response for ExtendFootprintTtl / RestoreFootprint.
///
/// These operations produce no results, no auth, and no return value — just
/// `transactionData` and `minResourceFee`.
pub(super) fn build_footprint_response(
    tx_data: SorobanTransactionData,
    latest_ledger: u32,
    format: XdrFormat,
) -> Result<serde_json::Value, JsonRpcError> {
    let min_resource_fee = tx_data.resource_fee;
    let mut obj = serde_json::Map::new();

    insert_sim_xdr_field(&mut obj, "transactionData", &tx_data, format)?;
    obj.insert("minResourceFee".into(), json!(min_resource_fee.to_string()));
    obj.insert(
        "cost".into(),
        json!({
            "cpuInsns": "0",
            "memBytes": "0"
        }),
    );
    obj.insert("latestLedger".into(), json!(latest_ledger));

    Ok(serde_json::Value::Object(obj))
}

pub(super) fn build_error_response(
    error: String,
    latest_ledger: u32,
) -> Result<serde_json::Value, JsonRpcError> {
    Ok(json!({
        "error": error,
        "transactionData": "",
        "minResourceFee": "0",
        "cost": {
            "cpuInsns": "0",
            "memBytes": "0"
        },
        "latestLedger": latest_ledger
    }))
}

// ---------------------------------------------------------------------------
// Simulate-specific XDR field helpers
// ---------------------------------------------------------------------------

/// Insert a single XDR value with simulate-specific naming.
///
/// Unlike `util::insert_xdr_field` (which appends `Xdr`/`Json` suffixes),
/// the `simulateTransaction` upstream response uses **unsuffixed** field names
/// for base64 mode (e.g. `transactionData`, `events`) and appends `Json` only
/// in JSON mode.
fn insert_sim_xdr_field<T: WriteXdr + serde::Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    val: &T,
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    util::insert_xdr_field_styled(obj, base_name, val, format, util::XdrKeyStyle::Unsuffixed)
}

/// Insert an optional XDR value: serializes `Some(val)` or inserts `Null`.
///
/// Base64: `"{base_name}": "<b64>"` or `"{base_name}": null`
/// Json: `"{base_name}Json": {...}` or `"{base_name}Json": null`
fn insert_optional_sim_xdr_field<T: WriteXdr + serde::Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    val: Option<&T>,
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    match val {
        Some(v) => insert_sim_xdr_field(obj, base_name, v, format),
        None => {
            let key = match format {
                XdrFormat::Base64 => base_name.to_string(),
                XdrFormat::Json => format!("{base_name}Json"),
            };
            obj.insert(key, serde_json::Value::Null);
            Ok(())
        }
    }
}

/// Insert an array of XDR values with simulate-specific naming.
///
/// Base64: `"{base_name}": ["<b64>", ...]`
/// Json: `"{base_name}Json": [{...}, ...]`
fn insert_sim_xdr_array_field<T: WriteXdr + serde::Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    items: &[T],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    util::insert_xdr_array_field_styled(
        obj,
        base_name,
        items,
        format,
        util::XdrKeyStyle::Unsuffixed,
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn test_account_key(key_byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
        })
    }

    fn make_test_ledger_entry(key_byte: u8) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
                balance: 10_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Default::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    // -----------------------------------------------------------------------
    // B7. insert_sim_xdr_field (4 tests) [REGRESSION]
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_xdr_field_base64_unsuffixed() {
        let mut obj = serde_json::Map::new();
        let data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        insert_sim_xdr_field(&mut obj, "transactionData", &data, XdrFormat::Base64).unwrap();
        assert!(
            obj.contains_key("transactionData"),
            "base64 mode should use unsuffixed key"
        );
        assert!(
            !obj.contains_key("transactionDataXdr"),
            "should NOT have Xdr suffix"
        );
    }

    #[test]
    fn test_sim_xdr_field_json_suffixed() {
        let mut obj = serde_json::Map::new();
        let data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        insert_sim_xdr_field(&mut obj, "transactionData", &data, XdrFormat::Json).unwrap();
        assert!(
            obj.contains_key("transactionDataJson"),
            "json mode should have Json suffix"
        );
        assert!(
            !obj.contains_key("transactionData"),
            "should NOT have unsuffixed key in JSON mode"
        );
    }

    #[test]
    fn test_sim_xdr_array_base64_unsuffixed() {
        let mut obj = serde_json::Map::new();
        let events: Vec<DiagnosticEvent> = vec![];
        insert_sim_xdr_array_field(&mut obj, "events", &events, XdrFormat::Base64).unwrap();
        assert!(
            obj.contains_key("events"),
            "base64 mode should use unsuffixed key"
        );
        assert!(!obj.contains_key("eventsXdr"), "should NOT have Xdr suffix");
    }

    #[test]
    fn test_sim_xdr_array_json_suffixed() {
        let mut obj = serde_json::Map::new();
        let events: Vec<DiagnosticEvent> = vec![];
        insert_sim_xdr_array_field(&mut obj, "events", &events, XdrFormat::Json).unwrap();
        assert!(
            obj.contains_key("eventsJson"),
            "json mode should have Json suffix"
        );
        assert!(
            !obj.contains_key("events"),
            "should NOT have unsuffixed key in JSON mode"
        );
    }

    // -----------------------------------------------------------------------
    // B8. serialize_state_changes (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_state_changes_created() {
        let key = test_account_key(1);
        let entry = make_test_ledger_entry(1);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: None,
            state_after: Some(entry),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "created");
        assert!(arr[0]["before"].is_null());
        assert!(arr[0]["after"].is_string());
    }

    #[test]
    fn test_state_changes_updated() {
        let key = test_account_key(2);
        let before = make_test_ledger_entry(2);
        let mut after = before.clone();
        if let LedgerEntryData::Account(ref mut acct) = after.data {
            acct.balance = 20_000_000;
        }
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: Some(before),
            state_after: Some(after),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "updated");
        assert!(arr[0]["before"].is_string());
        assert!(arr[0]["after"].is_string());
    }

    #[test]
    fn test_state_changes_deleted() {
        let key = test_account_key(3);
        let entry = make_test_ledger_entry(3);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: Some(entry),
            state_after: None,
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "deleted");
        assert!(arr[0]["before"].is_string());
        assert!(arr[0]["after"].is_null());
    }

    #[test]
    fn test_state_changes_json_format() {
        let key = test_account_key(4);
        let entry = make_test_ledger_entry(4);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: None,
            state_after: Some(entry),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Json).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "created");
        // JSON mode uses "keyJson", "beforeJson", "afterJson"
        assert!(arr[0].get("keyJson").is_some());
        assert!(arr[0].get("beforeJson").is_some());
        assert!(arr[0].get("afterJson").is_some());
        // Should NOT have base64 keys
        assert!(arr[0].get("key").is_none());
        assert!(arr[0].get("before").is_none());
        assert!(arr[0].get("after").is_none());
    }

    // -----------------------------------------------------------------------
    // B9-B10. build_error_response / build_footprint_response (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_error_response_structure() {
        let resp = build_error_response("something went wrong".into(), 42).unwrap();
        assert_eq!(resp["error"], "something went wrong");
        assert_eq!(resp["latestLedger"], 42);
        assert!(resp.get("transactionData").is_some());
        assert!(resp.get("minResourceFee").is_some());
        assert!(resp.get("cost").is_some());
    }

    #[test]
    fn test_build_error_response_defaults() {
        let resp = build_error_response("err".into(), 1).unwrap();
        assert_eq!(resp["transactionData"], "");
        assert_eq!(resp["minResourceFee"], "0");
        assert_eq!(resp["cost"]["cpuInsns"], "0");
        assert_eq!(resp["cost"]["memBytes"], "0");
    }

    #[test]
    fn test_build_footprint_response_base64() {
        let tx_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 12345,
        };
        let resp = build_footprint_response(tx_data, 100, XdrFormat::Base64).unwrap();
        assert!(resp.get("transactionData").is_some());
        assert!(resp["transactionData"].is_string());
        assert_eq!(resp["minResourceFee"], "12345");
        assert_eq!(resp["latestLedger"], 100);
        assert_eq!(resp["cost"]["cpuInsns"], "0");
    }

    #[test]
    fn test_build_footprint_response_json() {
        let tx_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 99,
        };
        let resp = build_footprint_response(tx_data, 50, XdrFormat::Json).unwrap();
        // JSON mode: "transactionDataJson" instead of "transactionData"
        assert!(resp.get("transactionDataJson").is_some());
        assert!(resp.get("transactionData").is_none());
    }
}
