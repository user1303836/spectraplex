//! Gold-tier protocol analytics materialization and aggregation.
//!
//! Computes Gold datasets (protocol_events, pool_snapshots) from Silver
//! decoded_events and token_transfers, and builds dashboard-ready
//! analytics responses (ProtocolActivity, TvlAnalytics).

use bigdecimal::BigDecimal;
use spectraplex_core::materializer::{
    DecodedEvent, EventTypeCount, PoolSnapshot, PoolTvlSummary, ProtocolActivity, ProtocolEvent,
    ProtocolTvlSummary, TokenTransfer, TvlAnalytics,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

/// Classify a decoded event name into a protocol event type.
fn classify_event(event_name: Option<&str>) -> &'static str {
    match event_name {
        Some(name) => {
            let lower = name.to_lowercase();
            if lower.contains("swap") {
                "swap"
            } else if lower.contains("mint") {
                "mint"
            } else if lower.contains("burn") {
                "burn"
            } else if lower.contains("addliquidity")
                || lower.contains("add_liquidity")
                || lower.contains("liquidityadded")
                || lower.contains("deposit")
            {
                "liquidity_added"
            } else if lower.contains("removeliquidity")
                || lower.contains("remove_liquidity")
                || lower.contains("liquidityremoved")
                || lower.contains("withdraw")
            {
                "liquidity_removed"
            } else if lower.contains("transfer") {
                "transfer"
            } else {
                "other"
            }
        }
        None => "other",
    }
}

/// Compute protocol event records from decoded_events.
///
/// Groups decoded events by their `program_or_contract` (treated as the
/// protocol address) and classifies each event by its name.
pub fn compute_protocol_events(
    decoded_events: &[DecodedEvent],
    pool_address_hint: Option<&str>,
) -> Vec<ProtocolEvent> {
    let now = chrono::Utc::now();
    decoded_events
        .iter()
        .map(|de| {
            let event_type = classify_event(de.event_name.as_deref());
            ProtocolEvent {
                id: Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("pe:{}", de.id).as_bytes()),
                network: de.network.clone(),
                protocol_address: de.program_or_contract.clone(),
                protocol_name: None,
                event_type: event_type.to_string(),
                event_details: de.decoded_fields.clone(),
                pool_address: pool_address_hint.map(|s| s.to_string()),
                raw_event_id: Some(de.id),
                timestamp: de.created_at.timestamp(),
                dataset_version_id: de.dataset_version_id,
                created_at: now,
            }
        })
        .collect()
}

/// Compute pool snapshot records from decoded events and token transfers.
///
/// This is a simplified materialization that derives pool state from
/// events that mention token pair interactions. In production, this would
/// be enhanced with actual reserve tracking from on-chain state.
pub fn compute_pool_snapshots(
    decoded_events: &[DecodedEvent],
    _token_transfers: &[TokenTransfer],
    pool_address: &str,
    token0: (&str, Option<&str>),
    token1: (&str, Option<&str>),
) -> Vec<PoolSnapshot> {
    let now = chrono::Utc::now();

    if decoded_events.is_empty() {
        return Vec::new();
    }

    // Derive a single snapshot from the latest event
    let latest = decoded_events.iter().max_by_key(|e| e.created_at).unwrap();

    let protocol_address = latest.program_or_contract.clone();

    // Extract reserve values from decoded_fields if available
    let reserve0 = latest
        .decoded_fields
        .get("reserve0")
        .or_else(|| latest.decoded_fields.get("amount0"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<BigDecimal>().ok())
        .unwrap_or_else(|| BigDecimal::from(0));

    let reserve1 = latest
        .decoded_fields
        .get("reserve1")
        .or_else(|| latest.decoded_fields.get("amount1"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<BigDecimal>().ok())
        .unwrap_or_else(|| BigDecimal::from(0));

    vec![PoolSnapshot {
        id: Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("ps:{}:{}:{}", latest.network, pool_address, latest.id).as_bytes(),
        ),
        network: latest.network.clone(),
        pool_address: pool_address.to_string(),
        protocol_address,
        protocol_name: None,
        token0_address: token0.0.to_string(),
        token0_symbol: token0.1.map(|s| s.to_string()),
        token1_address: token1.0.to_string(),
        token1_symbol: token1.1.map(|s| s.to_string()),
        reserve0,
        reserve1,
        tvl_usd: None,
        snapshot_timestamp: latest.created_at.timestamp(),
        block_number: None,
        dataset_version_id: latest.dataset_version_id,
        created_at: now,
    }]
}

/// Build per-protocol activity analytics from protocol event records.
pub fn build_protocol_activity(
    protocol_address: &str,
    events: &[ProtocolEvent],
) -> ProtocolActivity {
    let mut type_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut participants: HashSet<String> = HashSet::new();
    let mut time_start: Option<i64> = None;
    let mut time_end: Option<i64> = None;

    for event in events {
        if event.protocol_address != protocol_address {
            continue;
        }
        *type_counts.entry(event.event_type.clone()).or_insert(0) += 1;

        // Extract participant addresses from event_details if available
        if let Some(from) = event.event_details.get("from").and_then(|v| v.as_str()) {
            participants.insert(from.to_string());
        }
        if let Some(to) = event.event_details.get("to").and_then(|v| v.as_str()) {
            participants.insert(to.to_string());
        }
        if let Some(sender) = event.event_details.get("sender").and_then(|v| v.as_str()) {
            participants.insert(sender.to_string());
        }
        if let Some(recipient) = event
            .event_details
            .get("recipient")
            .and_then(|v| v.as_str())
        {
            participants.insert(recipient.to_string());
        }

        time_start = Some(time_start.map_or(event.timestamp, |t: i64| t.min(event.timestamp)));
        time_end = Some(time_end.map_or(event.timestamp, |t: i64| t.max(event.timestamp)));
    }

    let total_events: usize = type_counts.values().sum();

    let event_counts_by_type = type_counts
        .into_iter()
        .map(|(event_type, count)| EventTypeCount { event_type, count })
        .collect();

    ProtocolActivity {
        protocol_address: protocol_address.to_string(),
        event_counts_by_type,
        unique_participants: participants.len(),
        total_events,
        time_start,
        time_end,
    }
}

/// Build TVL analytics from pool snapshots.
pub fn build_tvl_analytics(snapshots: &[PoolSnapshot]) -> TvlAnalytics {
    let mut pools: Vec<PoolTvlSummary> = Vec::new();
    let mut protocol_map: HashMap<String, (Option<String>, Vec<Option<BigDecimal>>)> =
        HashMap::new();

    // Use latest snapshot per pool
    let mut latest_by_pool: HashMap<String, &PoolSnapshot> = HashMap::new();
    for snap in snapshots {
        let entry = latest_by_pool
            .entry(snap.pool_address.clone())
            .or_insert(snap);
        if snap.snapshot_timestamp > entry.snapshot_timestamp {
            *entry = snap;
        }
    }

    for snap in latest_by_pool.values() {
        pools.push(PoolTvlSummary {
            pool_address: snap.pool_address.clone(),
            protocol_address: snap.protocol_address.clone(),
            token0_symbol: snap.token0_symbol.clone(),
            token1_symbol: snap.token1_symbol.clone(),
            reserve0: snap.reserve0.clone(),
            reserve1: snap.reserve1.clone(),
            tvl_usd: snap.tvl_usd.clone(),
            snapshot_timestamp: snap.snapshot_timestamp,
        });

        let entry = protocol_map
            .entry(snap.protocol_address.clone())
            .or_insert_with(|| (snap.protocol_name.clone(), Vec::new()));
        entry.1.push(snap.tvl_usd.clone());
    }

    pools.sort_by(|a, b| a.pool_address.cmp(&b.pool_address));

    let mut protocols: Vec<ProtocolTvlSummary> = protocol_map
        .into_iter()
        .map(|(addr, (name, tvls))| {
            let total_tvl = if tvls.iter().all(|t| t.is_some()) {
                Some(
                    tvls.iter()
                        .filter_map(|t| t.as_ref())
                        .fold(BigDecimal::from(0), |acc, v| acc + v),
                )
            } else if tvls.iter().any(|t| t.is_some()) {
                Some(
                    tvls.iter()
                        .filter_map(|t| t.as_ref())
                        .fold(BigDecimal::from(0), |acc, v| acc + v),
                )
            } else {
                None
            };
            ProtocolTvlSummary {
                protocol_address: addr,
                protocol_name: name,
                pool_count: tvls.len(),
                total_tvl,
            }
        })
        .collect();
    protocols.sort_by(|a, b| a.protocol_address.cmp(&b.protocol_address));

    let total_tvl = {
        let all_tvls: Vec<&BigDecimal> = pools.iter().filter_map(|p| p.tvl_usd.as_ref()).collect();
        if all_tvls.is_empty() {
            None
        } else {
            Some(
                all_tvls
                    .into_iter()
                    .fold(BigDecimal::from(0), |acc, v| acc + v),
            )
        }
    };

    TvlAnalytics {
        pools,
        total_tvl,
        protocols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;

    fn make_decoded_event(
        contract: &str,
        event_name: Option<&str>,
        network: &str,
        fields: serde_json::Value,
    ) -> DecodedEvent {
        DecodedEvent {
            id: Uuid::new_v4(),
            raw_transaction_id: None,
            network: network.to_string(),
            program_or_contract: contract.to_string(),
            event_signature: None,
            event_name: event_name.map(|s| s.to_string()),
            log_index: 0,
            decoded_fields: fields,
            raw_fields: serde_json::json!({}),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn classify_event_names() {
        assert_eq!(classify_event(Some("Swap")), "swap");
        assert_eq!(classify_event(Some("swap")), "swap");
        assert_eq!(classify_event(Some("Mint")), "mint");
        assert_eq!(classify_event(Some("Burn")), "burn");
        assert_eq!(classify_event(Some("AddLiquidity")), "liquidity_added");
        assert_eq!(classify_event(Some("add_liquidity")), "liquidity_added");
        assert_eq!(classify_event(Some("Deposit")), "liquidity_added");
        assert_eq!(classify_event(Some("RemoveLiquidity")), "liquidity_removed");
        assert_eq!(classify_event(Some("Withdraw")), "liquidity_removed");
        assert_eq!(classify_event(Some("Transfer")), "transfer");
        assert_eq!(classify_event(Some("Approval")), "other");
        assert_eq!(classify_event(None), "other");
    }

    #[test]
    fn compute_protocol_events_maps_decoded_events() {
        let events = vec![
            make_decoded_event(
                "0xUniswap",
                Some("Swap"),
                "ethereum-mainnet",
                serde_json::json!({"amount0": "100", "amount1": "-50"}),
            ),
            make_decoded_event(
                "0xUniswap",
                Some("Mint"),
                "ethereum-mainnet",
                serde_json::json!({"amount0": "200", "amount1": "400"}),
            ),
            make_decoded_event(
                "0xSushi",
                Some("Transfer"),
                "ethereum-mainnet",
                serde_json::json!({}),
            ),
        ];
        let protocol_events = compute_protocol_events(&events, Some("0xPool"));
        assert_eq!(protocol_events.len(), 3);
        assert_eq!(protocol_events[0].event_type, "swap");
        assert_eq!(protocol_events[0].protocol_address, "0xUniswap");
        assert_eq!(protocol_events[0].pool_address.as_deref(), Some("0xPool"));
        assert_eq!(protocol_events[1].event_type, "mint");
        assert_eq!(protocol_events[2].event_type, "transfer");
        assert_eq!(protocol_events[2].protocol_address, "0xSushi");
    }

    #[test]
    fn compute_protocol_events_empty() {
        let result = compute_protocol_events(&[], None);
        assert!(result.is_empty());
    }

    #[test]
    fn compute_pool_snapshots_from_events() {
        let events = vec![make_decoded_event(
            "0xUniswap",
            Some("Swap"),
            "ethereum-mainnet",
            serde_json::json!({"reserve0": "1000", "reserve1": "2000000"}),
        )];
        let snapshots = compute_pool_snapshots(
            &events,
            &[],
            "0xPool",
            ("0xWETH", Some("WETH")),
            ("0xUSDC", Some("USDC")),
        );
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].pool_address, "0xPool");
        assert_eq!(snapshots[0].protocol_address, "0xUniswap");
        assert_eq!(snapshots[0].token0_symbol.as_deref(), Some("WETH"));
        assert_eq!(snapshots[0].token1_symbol.as_deref(), Some("USDC"));
        assert_eq!(snapshots[0].reserve0, BigDecimal::from_str("1000").unwrap());
        assert_eq!(
            snapshots[0].reserve1,
            BigDecimal::from_str("2000000").unwrap()
        );
    }

    #[test]
    fn compute_pool_snapshots_empty() {
        let result = compute_pool_snapshots(&[], &[], "0xPool", ("0xA", None), ("0xB", None));
        assert!(result.is_empty());
    }

    #[test]
    fn protocol_activity_aggregation() {
        let events = vec![
            ProtocolEvent {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                protocol_address: "0xUniswap".to_string(),
                protocol_name: None,
                event_type: "swap".to_string(),
                event_details: serde_json::json!({"from": "0xAlice", "to": "0xBob"}),
                pool_address: None,
                raw_event_id: None,
                timestamp: 1000,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            ProtocolEvent {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                protocol_address: "0xUniswap".to_string(),
                protocol_name: None,
                event_type: "swap".to_string(),
                event_details: serde_json::json!({"from": "0xAlice", "sender": "0xCharlie"}),
                pool_address: None,
                raw_event_id: None,
                timestamp: 2000,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            ProtocolEvent {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                protocol_address: "0xUniswap".to_string(),
                protocol_name: None,
                event_type: "mint".to_string(),
                event_details: serde_json::json!({"sender": "0xAlice"}),
                pool_address: None,
                raw_event_id: None,
                timestamp: 3000,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let activity = build_protocol_activity("0xUniswap", &events);
        assert_eq!(activity.protocol_address, "0xUniswap");
        assert_eq!(activity.total_events, 3);
        // Unique participants: 0xAlice, 0xBob, 0xCharlie
        assert_eq!(activity.unique_participants, 3);
        assert_eq!(activity.time_start, Some(1000));
        assert_eq!(activity.time_end, Some(3000));
        // Two event types: swap (2) and mint (1)
        assert_eq!(activity.event_counts_by_type.len(), 2);
        let swap_count = activity
            .event_counts_by_type
            .iter()
            .find(|e| e.event_type == "swap")
            .unwrap();
        assert_eq!(swap_count.count, 2);
        let mint_count = activity
            .event_counts_by_type
            .iter()
            .find(|e| e.event_type == "mint")
            .unwrap();
        assert_eq!(mint_count.count, 1);
    }

    #[test]
    fn protocol_activity_empty() {
        let activity = build_protocol_activity("0xNone", &[]);
        assert_eq!(activity.total_events, 0);
        assert_eq!(activity.unique_participants, 0);
        assert!(activity.time_start.is_none());
        assert!(activity.time_end.is_none());
    }

    #[test]
    fn tvl_analytics_single_pool() {
        let snapshots = vec![PoolSnapshot {
            id: Uuid::new_v4(),
            network: "ethereum-mainnet".to_string(),
            pool_address: "0xPool".to_string(),
            protocol_address: "0xProto".to_string(),
            protocol_name: Some("Uniswap".to_string()),
            token0_address: "0xWETH".to_string(),
            token0_symbol: Some("WETH".to_string()),
            token1_address: "0xUSDC".to_string(),
            token1_symbol: Some("USDC".to_string()),
            reserve0: BigDecimal::from(1000),
            reserve1: BigDecimal::from(2000000),
            tvl_usd: Some(BigDecimal::from(4000000)),
            snapshot_timestamp: 1700000000,
            block_number: Some(18000000),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }];

        let analytics = build_tvl_analytics(&snapshots);
        assert_eq!(analytics.pools.len(), 1);
        assert_eq!(analytics.total_tvl, Some(BigDecimal::from(4000000)));
        assert_eq!(analytics.protocols.len(), 1);
        assert_eq!(analytics.protocols[0].pool_count, 1);
        assert_eq!(
            analytics.protocols[0].total_tvl,
            Some(BigDecimal::from(4000000))
        );
    }

    #[test]
    fn tvl_analytics_multiple_pools() {
        let snapshots = vec![
            PoolSnapshot {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                pool_address: "0xPoolA".to_string(),
                protocol_address: "0xProto".to_string(),
                protocol_name: Some("Uniswap".to_string()),
                token0_address: "0xWETH".to_string(),
                token0_symbol: Some("WETH".to_string()),
                token1_address: "0xUSDC".to_string(),
                token1_symbol: Some("USDC".to_string()),
                reserve0: BigDecimal::from(1000),
                reserve1: BigDecimal::from(2000000),
                tvl_usd: Some(BigDecimal::from(4000000)),
                snapshot_timestamp: 1700000000,
                block_number: None,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            PoolSnapshot {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                pool_address: "0xPoolB".to_string(),
                protocol_address: "0xProto".to_string(),
                protocol_name: Some("Uniswap".to_string()),
                token0_address: "0xDAI".to_string(),
                token0_symbol: Some("DAI".to_string()),
                token1_address: "0xUSDC".to_string(),
                token1_symbol: Some("USDC".to_string()),
                reserve0: BigDecimal::from(500000),
                reserve1: BigDecimal::from(500000),
                tvl_usd: Some(BigDecimal::from(1000000)),
                snapshot_timestamp: 1700000000,
                block_number: None,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let analytics = build_tvl_analytics(&snapshots);
        assert_eq!(analytics.pools.len(), 2);
        assert_eq!(analytics.total_tvl, Some(BigDecimal::from(5000000)));
        assert_eq!(analytics.protocols.len(), 1);
        assert_eq!(analytics.protocols[0].pool_count, 2);
        assert_eq!(
            analytics.protocols[0].total_tvl,
            Some(BigDecimal::from(5000000))
        );
    }

    #[test]
    fn tvl_analytics_no_usd_values() {
        let snapshots = vec![PoolSnapshot {
            id: Uuid::new_v4(),
            network: "ethereum-mainnet".to_string(),
            pool_address: "0xPool".to_string(),
            protocol_address: "0xProto".to_string(),
            protocol_name: None,
            token0_address: "0xA".to_string(),
            token0_symbol: None,
            token1_address: "0xB".to_string(),
            token1_symbol: None,
            reserve0: BigDecimal::from(100),
            reserve1: BigDecimal::from(200),
            tvl_usd: None,
            snapshot_timestamp: 1700000000,
            block_number: None,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }];

        let analytics = build_tvl_analytics(&snapshots);
        assert!(analytics.total_tvl.is_none());
        assert!(analytics.protocols[0].total_tvl.is_none());
    }

    #[test]
    fn tvl_analytics_empty() {
        let analytics = build_tvl_analytics(&[]);
        assert!(analytics.pools.is_empty());
        assert!(analytics.protocols.is_empty());
        assert!(analytics.total_tvl.is_none());
    }

    #[test]
    fn tvl_analytics_uses_latest_snapshot_per_pool() {
        let snapshots = vec![
            PoolSnapshot {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                pool_address: "0xPool".to_string(),
                protocol_address: "0xProto".to_string(),
                protocol_name: None,
                token0_address: "0xA".to_string(),
                token0_symbol: None,
                token1_address: "0xB".to_string(),
                token1_symbol: None,
                reserve0: BigDecimal::from(100),
                reserve1: BigDecimal::from(200),
                tvl_usd: Some(BigDecimal::from(1000)),
                snapshot_timestamp: 1000,
                block_number: None,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            PoolSnapshot {
                id: Uuid::new_v4(),
                network: "ethereum-mainnet".to_string(),
                pool_address: "0xPool".to_string(),
                protocol_address: "0xProto".to_string(),
                protocol_name: None,
                token0_address: "0xA".to_string(),
                token0_symbol: None,
                token1_address: "0xB".to_string(),
                token1_symbol: None,
                reserve0: BigDecimal::from(500),
                reserve1: BigDecimal::from(600),
                tvl_usd: Some(BigDecimal::from(5000)),
                snapshot_timestamp: 2000,
                block_number: None,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let analytics = build_tvl_analytics(&snapshots);
        // Should pick the later snapshot (timestamp 2000)
        assert_eq!(analytics.pools.len(), 1);
        assert_eq!(analytics.total_tvl, Some(BigDecimal::from(5000)));
    }
}
