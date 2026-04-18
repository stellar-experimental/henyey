//! Handler for the `getEvents` JSON-RPC method.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{
    ContractEvent, ContractEventBody, ContractEventType, Limits, ReadXdr, ScVal, WriteXdr,
};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, format_unix_timestamp_utc, RpcXdrError, XdrFormat};

/// Default number of events returned per query.
const DEFAULT_EVENTS_LIMIT: u64 = 100;
/// Maximum number of events that can be requested in a single query.
const MAX_EVENTS_LIMIT: u64 = 10_000;

// SECURITY: request body bounded by HTTP framework body size limit; serde rejects invalid types
pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let lctx = util::LedgerContext::from_app(ctx).await?;

    let start_ledger = params
        .get("startLedger")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .ok_or_else(|| JsonRpcError::invalid_params("missing 'startLedger' parameter"))?;

    let end_ledger = params
        .get("endLedger")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Parse filters
    let (event_type, contract_ids, topic_filters) =
        parse_event_filters(params.get("filters").and_then(|v| v.as_array()))?;

    // Parse pagination
    let pagination = params.get("pagination");
    let limit = pagination
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_EVENTS_LIMIT)
        .min(MAX_EVENTS_LIMIT) as u32;
    let cursor = pagination
        .and_then(|p| p.get("cursor"))
        .and_then(|v| v.as_str());

    // Convert borrowed cursor to owned for the blocking closure
    let cursor_owned = cursor.map(String::from);

    // Query events and batch-load close times in a single blocking DB call.
    // On pruning race (require_close_times fails), fall back to batch_close_times
    // and skip events with missing headers rather than returning a 500.
    let (events, close_time_cache) = util::blocking_db(ctx, move |db| {
        db.with_connection(|conn| {
            use henyey_db::{EventQueries, LedgerQueries};
            let events = conn.query_events(&henyey_db::EventQueryParams {
                start_ledger,
                end_ledger,
                event_type,
                contract_ids: &contract_ids,
                topics: &topic_filters,
                cursor: cursor_owned.as_deref(),
                limit,
            })?;
            let seqs: Vec<u32> = events
                .iter()
                .map(|e| e.ledger_seq)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let close_times = match conn.require_close_times(&seqs) {
                Ok(ct) => ct,
                Err(henyey_db::DbError::Integrity(msg)) => {
                    tracing::warn!(
                        error = %msg,
                        "pruning race in getEvents; falling back to partial close times"
                    );
                    conn.batch_close_times(&seqs)?
                }
                Err(e) => return Err(e),
            };
            Ok((events, close_times))
        })
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = ?e, "get_events DB error");
        JsonRpcError::internal("database error")
    })?;

    // Build response using pre-fetched close times
    let mut event_json: Vec<serde_json::Value> = Vec::with_capacity(events.len());
    for event in &events {
        // Skip events whose header was pruned during a maintenance race.
        let Some(close_time_unix) = close_time_cache.get(&event.ledger_seq).copied() else {
            tracing::warn!(
                ledger_seq = event.ledger_seq,
                "skipping event with pruned header in getEvents"
            );
            continue;
        };
        let close_time = format_unix_timestamp_utc(close_time_unix);

        let event_type_str = match event.event_type {
            ContractEventType::Contract => "contract",
            ContractEventType::System => "system",
            ContractEventType::Diagnostic => "diagnostic",
        };

        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), json!(event_type_str));
        obj.insert("ledger".into(), json!(event.ledger_seq));
        obj.insert("ledgerClosedAt".into(), json!(close_time));
        obj.insert("id".into(), json!(event.id));
        obj.insert("operationIndex".into(), json!(event.op_index));
        obj.insert("transactionIndex".into(), json!(event.tx_index));
        obj.insert("txHash".into(), json!(event.tx_hash));
        obj.insert(
            "inSuccessfulContractCall".into(),
            json!(event.in_successful_contract_call),
        );

        // contractId
        match &event.contract_id {
            Some(cid) => obj.insert("contractId".into(), json!(cid)),
            None => obj.insert("contractId".into(), json!("")),
        };

        // Extract value and topic from the ContractEvent XDR, format-aware.
        // Warn and skip corrupt events rather than failing the entire response.
        match insert_event_fields(&mut obj, &event.event_xdr, &event.topics, format) {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    event_id = %event.id,
                    ledger_seq = event.ledger_seq,
                    error = ?e,
                    "skipping corrupt event in getEvents response"
                );
                continue;
            }
        }

        event_json.push(serde_json::Value::Object(obj));
    }

    let last_cursor = events.last().map(|e| e.id.as_str()).unwrap_or("");

    let mut result = json!({
        "events": event_json,
        "cursor": last_cursor,
    });
    lctx.insert_json_fields(result.as_object_mut().unwrap());
    // get_events formats oldestLedgerCloseTime as ISO 8601 (unlike other handlers)
    result["oldestLedgerCloseTime"] = json!(format_unix_timestamp_utc(lctx.oldest_close_time));
    Ok(result)
}

/// Maximum number of filters allowed per request.
const MAX_FILTERS: usize = 5;
/// Maximum number of contract IDs per filter.
const MAX_CONTRACT_IDS_PER_FILTER: usize = 5;
/// Maximum number of topic segments per filter.
const MAX_TOPICS_PER_FILTER: usize = 5;
/// Maximum number of alternatives per topic segment.
const MAX_TOPIC_SEGMENTS: usize = 4;

type EventTypeFilter = Option<&'static str>;
type ContractIdFilters = Vec<String>;
type TopicFilters = Vec<Vec<String>>;
type EventFilters = (EventTypeFilter, ContractIdFilters, TopicFilters);

/// Parse event filter parameters from the JSON-RPC request.
///
/// Returns `(event_type, contract_ids, topic_filters)`.
fn parse_event_filters(
    filters: Option<&Vec<serde_json::Value>>,
) -> Result<EventFilters, JsonRpcError> {
    let mut event_type: Option<&'static str> = None;
    let mut contract_ids = Vec::new();
    let mut topic_filters = Vec::new();

    let Some(filter_array) = filters else {
        return Ok((event_type, contract_ids, topic_filters));
    };

    if filter_array.len() > MAX_FILTERS {
        return Err(JsonRpcError::invalid_params(format!(
            "too many filters: max {} allowed",
            MAX_FILTERS
        )));
    }

    for filter in filter_array {
        if let Some(t) = filter.get("type").and_then(|v| v.as_str()) {
            event_type = Some(match t {
                "contract" => "contract",
                "system" => "system",
                other => {
                    return Err(JsonRpcError::invalid_params(format!(
                        "unsupported event type: '{}' (allowed: contract, system)",
                        other,
                    )));
                }
            });
        }

        if let Some(cids) = filter.get("contractIds").and_then(|v| v.as_array()) {
            if cids.len() > MAX_CONTRACT_IDS_PER_FILTER {
                return Err(JsonRpcError::invalid_params(format!(
                    "too many contractIds per filter: max {} allowed",
                    MAX_CONTRACT_IDS_PER_FILTER
                )));
            }
            contract_ids.extend(cids.iter().filter_map(|v| v.as_str().map(String::from)));
        }

        // Topics is an array of arrays: [["topic1_a", "topic1_b"], ["*"], ...]
        // Each inner array is OR alternatives for that position.
        if let Some(topics_arr) = filter.get("topics").and_then(|v| v.as_array()) {
            parse_topic_filters(topics_arr, &mut topic_filters)?;
        }
    }

    Ok((event_type, contract_ids, topic_filters))
}

/// Parse a topics array into a list of topic filter alternatives.
///
/// Each element of `topics_arr` is an array of OR alternatives for that position.
/// `**` means "match all remaining positions" — stop adding further filters.
fn parse_topic_filters(
    topics_arr: &[serde_json::Value],
    topic_filters: &mut Vec<Vec<String>>,
) -> Result<(), JsonRpcError> {
    if topics_arr.len() > MAX_TOPICS_PER_FILTER {
        return Err(JsonRpcError::invalid_params(format!(
            "too many topics per filter: max {} allowed",
            MAX_TOPICS_PER_FILTER
        )));
    }
    for topic_set in topics_arr {
        if let Some(alternatives) = topic_set.as_array() {
            if alternatives.len() > MAX_TOPIC_SEGMENTS {
                return Err(JsonRpcError::invalid_params(format!(
                    "too many alternatives per topic segment: max {} allowed",
                    MAX_TOPIC_SEGMENTS
                )));
            }
            let alt_strings: Vec<String> = alternatives
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if alt_strings.iter().any(|s| s == "**") {
                break;
            }
            topic_filters.push(alt_strings);
        }
    }
    Ok(())
}

/// Insert event value and topic fields into the JSON object, format-aware.
fn insert_event_fields(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    event_xdr_b64: &str,
    topics: &[String],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    match format {
        XdrFormat::Base64 => {
            // value: base64 of the ScVal data from the event body
            let val = extract_event_value(event_xdr_b64)?;
            let xdr_bytes = val.to_xdr(Limits::none()).map_err(|e| {
                JsonRpcError::from(RpcXdrError::XdrSerialize {
                    type_name: "ScVal",
                    cause: e.to_string(),
                })
            })?;
            obj.insert("value".into(), json!(BASE64.encode(&xdr_bytes)));
            // topic: array of base64-encoded topic ScVals (already stored as base64)
            obj.insert("topic".into(), json!(topics));
        }
        XdrFormat::Json => {
            // value: JSON representation of the ScVal
            let val = extract_event_value(event_xdr_b64)?;
            let json_val = serde_json::to_value(&val)
                .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
            obj.insert("valueJson".into(), json_val);
            // topic: array of JSON ScVals
            let mut topic_json = Vec::with_capacity(topics.len());
            for t in topics {
                let bytes = BASE64.decode(t).map_err(|e| {
                    JsonRpcError::from(RpcXdrError::Base64Decode {
                        cause: e.to_string(),
                    })
                })?;
                let scval = ScVal::from_xdr(&bytes, Limits::none()).map_err(|e| {
                    JsonRpcError::from(RpcXdrError::XdrParse {
                        type_name: "ScVal (topic)",
                        cause: e.to_string(),
                    })
                })?;
                let jv = serde_json::to_value(&scval)
                    .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
                topic_json.push(jv);
            }
            obj.insert("topicJson".into(), json!(topic_json));
        }
    }
    Ok(())
}

/// Extract the value ScVal from a ContractEvent's body.
fn extract_event_value(event_xdr_b64: &str) -> Result<ScVal, JsonRpcError> {
    let bytes = BASE64.decode(event_xdr_b64).map_err(|e| {
        JsonRpcError::from(RpcXdrError::Base64Decode {
            cause: e.to_string(),
        })
    })?;
    let event = ContractEvent::from_xdr(&bytes, Limits::none()).map_err(|e| {
        JsonRpcError::from(RpcXdrError::XdrParse {
            type_name: "ContractEvent",
            cause: e.to_string(),
        })
    })?;
    match event.body {
        ContractEventBody::V0(body) => Ok(body.data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_filters(filters: serde_json::Value) -> Option<Vec<serde_json::Value>> {
        filters.as_array().cloned()
    }

    #[test]
    fn test_parse_filters_empty() {
        let (event_type, contract_ids, topics) = parse_event_filters(None).unwrap();
        assert!(event_type.is_none());
        assert!(contract_ids.is_empty());
        assert!(topics.is_empty());
    }

    #[test]
    fn test_parse_filters_contract_id() {
        let filters = json!([{"contractIds": ["CABC123"]}]);
        let arr = make_filters(filters).unwrap();
        let (_, contract_ids, _) = parse_event_filters(Some(&arr)).unwrap();
        assert_eq!(contract_ids, vec!["CABC123"]);
    }

    #[test]
    fn test_parse_filters_topic_wildcard() {
        let filters = json!([{"topics": [["*"]]}]);
        let arr = make_filters(filters).unwrap();
        let (_, _, topics) = parse_event_filters(Some(&arr)).unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0], vec!["*"]);
    }

    #[test]
    fn test_parse_filters_topic_double_star() {
        // ** means "match all remaining" — parser should stop adding further segments
        let filters = json!([{"topics": [["topic1"], ["**"], ["should_be_ignored"]]}]);
        let arr = make_filters(filters).unwrap();
        let (_, _, topics) = parse_event_filters(Some(&arr)).unwrap();
        // Only the first segment before ** should be captured
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0], vec!["topic1"]);
    }

    #[test]
    fn test_parse_filters_max_exceeded() {
        // 6 filters > MAX_FILTERS (5)
        let filters = json!([{}, {}, {}, {}, {}, {}]);
        let arr = make_filters(filters).unwrap();
        let result = parse_event_filters(Some(&arr));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filters_max_contract_ids() {
        // 6 contract IDs > MAX_CONTRACT_IDS_PER_FILTER (5)
        let filters = json!([{"contractIds": ["a","b","c","d","e","f"]}]);
        let arr = make_filters(filters).unwrap();
        let result = parse_event_filters(Some(&arr));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filters_max_topics() {
        // 6 topic segments > MAX_TOPICS_PER_FILTER (5) — should fail
        let filters = json!([{"topics": [["a"],["b"],["c"],["d"],["e"],["f"]]}]);
        let arr = make_filters(filters).unwrap();
        let result = parse_event_filters(Some(&arr));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_filters_diagnostic_rejected() {
        let filters = json!([{"type": "diagnostic"}]);
        let arr = make_filters(filters).unwrap();
        let result = parse_event_filters(Some(&arr));
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_event_value_invalid_base64() {
        let result = extract_event_value("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_event_value_invalid_xdr() {
        // Valid base64 but invalid XDR
        let garbage = BASE64.encode(&[0xff, 0xfe, 0xfd]);
        let result = extract_event_value(&garbage);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_event_value_valid() {
        use stellar_xdr::curr::{ContractEvent, ContractEventBody, ContractEventV0, WriteXdr};
        let event = ContractEvent {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![].try_into().unwrap(),
                data: ScVal::U32(42),
            }),
        };
        let bytes = event.to_xdr(Limits::none()).unwrap();
        let b64 = BASE64.encode(&bytes);
        let val = extract_event_value(&b64).unwrap();
        assert_eq!(val, ScVal::U32(42));
    }

    #[test]
    fn test_insert_event_fields_base64_valid() {
        use stellar_xdr::curr::{ContractEvent, ContractEventBody, ContractEventV0, WriteXdr};
        let event = ContractEvent {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![].try_into().unwrap(),
                data: ScVal::U32(42),
            }),
        };
        let bytes = event.to_xdr(Limits::none()).unwrap();
        let b64 = BASE64.encode(&bytes);
        let mut obj = serde_json::Map::new();
        insert_event_fields(&mut obj, &b64, &[], XdrFormat::Base64).unwrap();
        assert!(obj.contains_key("value"));
        // value should be non-empty base64
        let val_str = obj["value"].as_str().unwrap();
        assert!(!val_str.is_empty());
    }

    #[test]
    fn test_insert_event_fields_corrupt_event_errors() {
        let mut obj = serde_json::Map::new();
        let result = insert_event_fields(&mut obj, "not-valid!!!", &[], XdrFormat::Base64);
        assert!(result.is_err());
    }

    #[test]
    fn test_insert_event_fields_json_corrupt_topic_errors() {
        use stellar_xdr::curr::{ContractEvent, ContractEventBody, ContractEventV0, WriteXdr};
        let event = ContractEvent {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract_id: None,
            type_: ContractEventType::Contract,
            body: ContractEventBody::V0(ContractEventV0 {
                topics: vec![].try_into().unwrap(),
                data: ScVal::U32(42),
            }),
        };
        let bytes = event.to_xdr(Limits::none()).unwrap();
        let b64 = BASE64.encode(&bytes);
        // Valid event but corrupt topic base64
        let mut obj = serde_json::Map::new();
        let result = insert_event_fields(
            &mut obj,
            &b64,
            &["not-valid-base64!!!".to_string()],
            XdrFormat::Json,
        );
        assert!(result.is_err());
    }
}
