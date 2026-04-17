//! Gold-tier Hyperliquid analytics materialization and aggregation.
//!
//! Computes Gold datasets (hl_pnl_summary, hl_trade_history) from Silver
//! Hyperliquid data (hl_fills, hl_funding), and builds dashboard-ready
//! analytics responses (TraderAnalytics, MarketAnalytics).

use bigdecimal::BigDecimal;
use spectraplex_core::materializer::{
    CoinMarketSummary, CoinPnlSummary, HlFillRecord, HlFundingPayment, HlPnlSummary,
    HlTradeHistory, MarketAnalytics, TraderAnalytics,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Compute PnL summary records from fills and funding payments.
///
/// Groups fills by (wallet_address, coin) — aggregating closed_pnl, fees,
/// and fill count — then merges funding totals for the same keys. Returns
/// one `HlPnlSummary` per (wallet, coin) pair.
pub fn compute_pnl_summary(
    wallet: &str,
    network: &str,
    fills: &[HlFillRecord],
    funding: &[HlFundingPayment],
    period_start: i64,
    period_end: i64,
) -> Vec<HlPnlSummary> {
    struct CoinAgg {
        total_closed_pnl: BigDecimal,
        total_fees: BigDecimal,
        total_funding: BigDecimal,
        fill_count: i64,
        trade_sizes: Vec<BigDecimal>,
        win_count: i64,
        loss_count: i64,
    }

    let mut map: HashMap<String, CoinAgg> = HashMap::new();

    for fill in fills {
        let agg = map.entry(fill.coin.clone()).or_insert_with(|| CoinAgg {
            total_closed_pnl: BigDecimal::from(0),
            total_fees: BigDecimal::from(0),
            total_funding: BigDecimal::from(0),
            fill_count: 0,
            trade_sizes: Vec::new(),
            win_count: 0,
            loss_count: 0,
        });
        agg.fill_count += 1;
        if let Some(ref cpnl) = fill.closed_pnl {
            agg.total_closed_pnl += cpnl;
            if cpnl > &BigDecimal::from(0) {
                agg.win_count += 1;
            } else if cpnl < &BigDecimal::from(0) {
                agg.loss_count += 1;
            }
        }
        if let Some(ref fee) = fill.fee {
            agg.total_fees += fee;
        }
        agg.trade_sizes.push(fill.size.clone());
    }

    for fp in funding {
        let agg = map.entry(fp.coin.clone()).or_insert_with(|| CoinAgg {
            total_closed_pnl: BigDecimal::from(0),
            total_fees: BigDecimal::from(0),
            total_funding: BigDecimal::from(0),
            fill_count: 0,
            trade_sizes: Vec::new(),
            win_count: 0,
            loss_count: 0,
        });
        agg.total_funding += &fp.amount;
    }

    let now = chrono::Utc::now();
    let mut results: Vec<HlPnlSummary> = map
        .into_iter()
        .map(|(coin, agg)| {
            let trade_count = agg.win_count + agg.loss_count;
            let avg_trade_size = if agg.trade_sizes.is_empty() {
                BigDecimal::from(0)
            } else {
                let total_size: BigDecimal = agg
                    .trade_sizes
                    .iter()
                    .fold(BigDecimal::from(0), |acc, s| acc + s);
                total_size / BigDecimal::from(agg.trade_sizes.len() as i64)
            };
            let net_pnl = &agg.total_closed_pnl + &agg.total_funding - &agg.total_fees;
            HlPnlSummary {
                id: Uuid::new_v4(),
                wallet_address: wallet.to_string(),
                coin,
                network: network.to_string(),
                period_start,
                period_end,
                total_closed_pnl: agg.total_closed_pnl,
                total_funding: agg.total_funding,
                total_fees: agg.total_fees,
                net_pnl,
                trade_count,
                fill_count: agg.fill_count,
                avg_trade_size,
                win_count: agg.win_count,
                loss_count: agg.loss_count,
                dataset_version_id: None,
                created_at: now,
            }
        })
        .collect();
    results.sort_by(|a, b| a.coin.cmp(&b.coin));
    results
}

/// Build trade history from fills by grouping into logical open/close sequences.
///
/// A trade begins when a fill has a direction containing "Open" and ends
/// when a fill has a direction containing "Close". If fills cannot be
/// grouped into explicit open/close pairs (e.g. all fills lack direction),
/// each fill with a non-zero closed_pnl is treated as a standalone trade.
pub fn build_trade_history(
    wallet: &str,
    network: &str,
    fills: &[HlFillRecord],
) -> Vec<HlTradeHistory> {
    let now = chrono::Utc::now();

    // Group fills by coin
    let mut by_coin: HashMap<String, Vec<&HlFillRecord>> = HashMap::new();
    for fill in fills {
        by_coin.entry(fill.coin.clone()).or_default().push(fill);
    }

    let mut trades = Vec::new();

    for (coin, mut coin_fills) in by_coin {
        // Sort fills by fill_time ascending
        coin_fills.sort_by_key(|f| f.fill_time);

        let has_direction_info = coin_fills.iter().any(|f| {
            f.direction
                .as_deref()
                .is_some_and(|d| d.contains("Open") || d.contains("Close"))
        });

        if has_direction_info {
            // Group into open/close sequences
            let mut open_fills: Vec<&HlFillRecord> = Vec::new();

            for fill in &coin_fills {
                let dir = fill.direction.as_deref().unwrap_or("");
                if dir.contains("Open") {
                    open_fills.push(fill);
                } else if dir.contains("Close") && !open_fills.is_empty() {
                    // Build trade from accumulated opens + this close
                    let entry_price = weighted_avg_price(&open_fills);
                    let total_size: BigDecimal = open_fills
                        .iter()
                        .fold(BigDecimal::from(0), |acc, f| acc + &f.size);
                    let total_fees: BigDecimal = open_fills
                        .iter()
                        .chain(std::iter::once(fill))
                        .fold(BigDecimal::from(0), |acc, f| {
                            acc + f.fee.as_ref().unwrap_or(&BigDecimal::from(0))
                        });
                    let side = open_fills
                        .first()
                        .map(|f| f.side.clone())
                        .unwrap_or_default();
                    let opened_at = open_fills.first().map(|f| f.fill_time).unwrap_or(0);
                    let num_fills = (open_fills.len() + 1) as i64;

                    trades.push(HlTradeHistory {
                        id: Uuid::new_v4(),
                        wallet_address: wallet.to_string(),
                        coin: coin.clone(),
                        network: network.to_string(),
                        side,
                        entry_price,
                        exit_price: fill.price.clone(),
                        size: total_size,
                        opened_at,
                        closed_at: fill.fill_time,
                        realized_pnl: fill
                            .closed_pnl
                            .clone()
                            .unwrap_or_else(|| BigDecimal::from(0)),
                        fees: total_fees,
                        num_fills,
                        dataset_version_id: None,
                        created_at: now,
                    });
                    open_fills.clear();
                } else if dir.contains("Close") {
                    // Close without a preceding open — standalone trade
                    trades.push(HlTradeHistory {
                        id: Uuid::new_v4(),
                        wallet_address: wallet.to_string(),
                        coin: coin.clone(),
                        network: network.to_string(),
                        side: fill.side.clone(),
                        entry_price: fill.price.clone(),
                        exit_price: fill.price.clone(),
                        size: fill.size.clone(),
                        opened_at: fill.fill_time,
                        closed_at: fill.fill_time,
                        realized_pnl: fill
                            .closed_pnl
                            .clone()
                            .unwrap_or_else(|| BigDecimal::from(0)),
                        fees: fill.fee.clone().unwrap_or_else(|| BigDecimal::from(0)),
                        num_fills: 1,
                        dataset_version_id: None,
                        created_at: now,
                    });
                }
            }
        } else {
            // Fallback: each fill with closed_pnl becomes a standalone trade
            for fill in &coin_fills {
                if fill.closed_pnl.is_some() {
                    trades.push(HlTradeHistory {
                        id: Uuid::new_v4(),
                        wallet_address: wallet.to_string(),
                        coin: coin.clone(),
                        network: network.to_string(),
                        side: fill.side.clone(),
                        entry_price: fill.price.clone(),
                        exit_price: fill.price.clone(),
                        size: fill.size.clone(),
                        opened_at: fill.fill_time,
                        closed_at: fill.fill_time,
                        realized_pnl: fill
                            .closed_pnl
                            .clone()
                            .unwrap_or_else(|| BigDecimal::from(0)),
                        fees: fill.fee.clone().unwrap_or_else(|| BigDecimal::from(0)),
                        num_fills: 1,
                        dataset_version_id: None,
                        created_at: now,
                    });
                }
            }
        }
    }

    trades.sort_by_key(|t| t.closed_at);
    trades
}

/// Build per-trader analytics from PnL summaries and trade histories.
pub fn build_trader_analytics(
    wallet: &str,
    pnl_summaries: &[HlPnlSummary],
    trade_histories: &[HlTradeHistory],
) -> TraderAnalytics {
    let total_net_pnl = pnl_summaries
        .iter()
        .fold(BigDecimal::from(0), |acc, s| acc + &s.net_pnl);

    let total_volume = trade_histories
        .iter()
        .fold(BigDecimal::from(0), |acc, t| acc + &t.size * &t.entry_price);

    let total_trades = trade_histories.len() as i64;

    let total_wins: i64 = pnl_summaries.iter().map(|s| s.win_count).sum();
    let total_losses: i64 = pnl_summaries.iter().map(|s| s.loss_count).sum();
    let total_resolved = total_wins + total_losses;
    let win_rate = if total_resolved > 0 {
        total_wins as f64 / total_resolved as f64
    } else {
        0.0
    };

    let mut coin_map: HashMap<String, (BigDecimal, BigDecimal, i64, i64, i64)> = HashMap::new();
    for s in pnl_summaries {
        let entry = coin_map
            .entry(s.coin.clone())
            .or_insert_with(|| (BigDecimal::from(0), BigDecimal::from(0), 0, 0, 0));
        entry.0 += &s.net_pnl;
        entry.3 += s.win_count;
        entry.4 += s.loss_count;
    }
    for t in trade_histories {
        let entry = coin_map
            .entry(t.coin.clone())
            .or_insert_with(|| (BigDecimal::from(0), BigDecimal::from(0), 0, 0, 0));
        entry.1 += &t.size * &t.entry_price;
        entry.2 += 1;
    }

    let mut coin_breakdown: Vec<CoinPnlSummary> = coin_map
        .into_iter()
        .map(
            |(coin, (net_pnl, volume, trade_count, win_count, loss_count))| CoinPnlSummary {
                coin,
                net_pnl,
                volume,
                trade_count,
                win_count,
                loss_count,
            },
        )
        .collect();
    coin_breakdown.sort_by(|a, b| a.coin.cmp(&b.coin));

    TraderAnalytics {
        wallet_address: wallet.to_string(),
        total_net_pnl,
        total_volume,
        total_trades,
        win_rate,
        coin_breakdown,
    }
}

/// Build per-coin market analytics from PnL summaries and trade histories.
pub fn build_market_analytics(
    pnl_summaries: &[HlPnlSummary],
    trade_histories: &[HlTradeHistory],
) -> MarketAnalytics {
    struct CoinAgg {
        volume: BigDecimal,
        traders: HashSet<String>,
        trade_count: i64,
        total_pnl: BigDecimal,
        sizes: Vec<BigDecimal>,
    }

    let mut coin_map: HashMap<String, CoinAgg> = HashMap::new();

    for s in pnl_summaries {
        let entry = coin_map.entry(s.coin.clone()).or_insert_with(|| CoinAgg {
            volume: BigDecimal::from(0),
            traders: HashSet::new(),
            trade_count: 0,
            total_pnl: BigDecimal::from(0),
            sizes: Vec::new(),
        });
        entry.traders.insert(s.wallet_address.clone());
        entry.total_pnl += &s.net_pnl;
    }

    for t in trade_histories {
        let entry = coin_map.entry(t.coin.clone()).or_insert_with(|| CoinAgg {
            volume: BigDecimal::from(0),
            traders: HashSet::new(),
            trade_count: 0,
            total_pnl: BigDecimal::from(0),
            sizes: Vec::new(),
        });
        entry.volume += &t.size * &t.entry_price;
        entry.traders.insert(t.wallet_address.clone());
        entry.trade_count += 1;
        entry.sizes.push(t.size.clone());
    }

    let mut all_traders: HashSet<String> = HashSet::new();
    let mut total_volume = BigDecimal::from(0);

    let mut coins: Vec<CoinMarketSummary> = coin_map
        .into_iter()
        .map(|(coin, agg)| {
            for t in &agg.traders {
                all_traders.insert(t.clone());
            }
            total_volume += &agg.volume;
            let avg_trade_size = if agg.sizes.is_empty() {
                BigDecimal::from(0)
            } else {
                let sum: BigDecimal = agg.sizes.iter().fold(BigDecimal::from(0), |acc, s| acc + s);
                sum / BigDecimal::from(agg.sizes.len() as i64)
            };
            CoinMarketSummary {
                coin,
                total_volume: agg.volume,
                unique_traders: agg.traders.len(),
                total_trades: agg.trade_count,
                total_pnl: agg.total_pnl,
                avg_trade_size,
            }
        })
        .collect();
    coins.sort_by(|a, b| a.coin.cmp(&b.coin));

    MarketAnalytics {
        coins,
        total_volume,
        total_unique_traders: all_traders.len(),
    }
}

/// Weighted average price of a set of fills (size-weighted).
fn weighted_avg_price(fills: &[&HlFillRecord]) -> BigDecimal {
    if fills.is_empty() {
        return BigDecimal::from(0);
    }
    let total_size: BigDecimal = fills
        .iter()
        .fold(BigDecimal::from(0), |acc, f| acc + &f.size);
    if total_size == BigDecimal::from(0) {
        return fills[0].price.clone();
    }
    let weighted_sum: BigDecimal = fills
        .iter()
        .fold(BigDecimal::from(0), |acc, f| acc + &f.price * &f.size);
    weighted_sum / total_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;

    #[allow(clippy::too_many_arguments)]
    fn make_fill(
        coin: &str,
        side: &str,
        price: &str,
        size: &str,
        closed_pnl: Option<&str>,
        fee: Option<&str>,
        direction: Option<&str>,
        fill_time: i64,
    ) -> HlFillRecord {
        HlFillRecord {
            id: Uuid::new_v4(),
            raw_transaction_id: None,
            network: "hyperliquid-mainnet".to_string(),
            coin: coin.to_string(),
            side: side.to_string(),
            price: BigDecimal::from_str(price).unwrap(),
            size: BigDecimal::from_str(size).unwrap(),
            direction: direction.map(|s| s.to_string()),
            closed_pnl: closed_pnl.map(|s| BigDecimal::from_str(s).unwrap()),
            fee: fee.map(|s| BigDecimal::from_str(s).unwrap()),
            fee_token: Some("USDC".to_string()),
            fill_time,
            order_id: None,
            trade_id: None,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn make_funding(coin: &str, amount: &str, payment_time: i64) -> HlFundingPayment {
        HlFundingPayment {
            id: Uuid::new_v4(),
            raw_transaction_id: None,
            network: "hyperliquid-mainnet".to_string(),
            coin: coin.to_string(),
            amount: BigDecimal::from_str(amount).unwrap(),
            funding_rate: None,
            payment_time,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn pnl_summary_aggregates_fills_and_funding() {
        let fills = vec![
            make_fill(
                "ETH",
                "B",
                "2000",
                "1.0",
                Some("100"),
                Some("2"),
                Some("Close Long"),
                1000,
            ),
            make_fill(
                "ETH",
                "B",
                "2100",
                "0.5",
                Some("-50"),
                Some("1"),
                Some("Close Long"),
                2000,
            ),
        ];
        let funding = vec![
            make_funding("ETH", "10.5", 1500),
            make_funding("ETH", "-3.0", 2500),
        ];

        let summaries =
            compute_pnl_summary("0xTrader", "hyperliquid-mainnet", &fills, &funding, 0, 3000);
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.coin, "ETH");
        assert_eq!(s.wallet_address, "0xTrader");
        // closed_pnl: 100 + (-50) = 50
        assert_eq!(s.total_closed_pnl, BigDecimal::from_str("50").unwrap());
        // funding: 10.5 + (-3.0) = 7.5
        assert_eq!(s.total_funding, BigDecimal::from_str("7.5").unwrap());
        // fees: 2 + 1 = 3
        assert_eq!(s.total_fees, BigDecimal::from_str("3").unwrap());
        // net_pnl: 50 + 7.5 - 3 = 54.5
        assert_eq!(s.net_pnl, BigDecimal::from_str("54.5").unwrap());
        assert_eq!(s.fill_count, 2);
        assert_eq!(s.win_count, 1);
        assert_eq!(s.loss_count, 1);
    }

    #[test]
    fn pnl_summary_multiple_coins() {
        let fills = vec![
            make_fill(
                "ETH",
                "B",
                "2000",
                "1.0",
                Some("100"),
                Some("2"),
                None,
                1000,
            ),
            make_fill(
                "BTC",
                "A",
                "50000",
                "0.1",
                Some("500"),
                Some("5"),
                None,
                2000,
            ),
        ];
        let summaries =
            compute_pnl_summary("0xTrader", "hyperliquid-mainnet", &fills, &[], 0, 3000);
        assert_eq!(summaries.len(), 2);
        // Sorted by coin name
        assert_eq!(summaries[0].coin, "BTC");
        assert_eq!(summaries[1].coin, "ETH");
    }

    #[test]
    fn pnl_summary_empty_inputs() {
        let summaries = compute_pnl_summary("0xTrader", "hyperliquid-mainnet", &[], &[], 0, 3000);
        assert!(summaries.is_empty());
    }

    #[test]
    fn trade_history_groups_open_close() {
        let fills = vec![
            make_fill(
                "ETH",
                "B",
                "2000",
                "1.0",
                None,
                Some("1"),
                Some("Open Long"),
                1000,
            ),
            make_fill(
                "ETH",
                "B",
                "2100",
                "0.5",
                None,
                Some("0.5"),
                Some("Open Long"),
                2000,
            ),
            make_fill(
                "ETH",
                "A",
                "2200",
                "1.5",
                Some("250"),
                Some("1.5"),
                Some("Close Long"),
                3000,
            ),
        ];
        let trades = build_trade_history("0xTrader", "hyperliquid-mainnet", &fills);
        assert_eq!(trades.len(), 1);
        let t = &trades[0];
        assert_eq!(t.coin, "ETH");
        assert_eq!(t.side, "B");
        // Weighted avg entry: (2000*1.0 + 2100*0.5) / 1.5 = 3050/1.5 ≈ 2033.33...
        assert!(t.entry_price > BigDecimal::from_str("2033").unwrap());
        assert!(t.entry_price < BigDecimal::from_str("2034").unwrap());
        assert_eq!(t.exit_price, BigDecimal::from_str("2200").unwrap());
        assert_eq!(t.size, BigDecimal::from_str("1.5").unwrap());
        assert_eq!(t.opened_at, 1000);
        assert_eq!(t.closed_at, 3000);
        assert_eq!(t.realized_pnl, BigDecimal::from_str("250").unwrap());
        assert_eq!(t.num_fills, 3);
        // total fees: 1 + 0.5 + 1.5 = 3
        assert_eq!(t.fees, BigDecimal::from_str("3").unwrap());
    }

    #[test]
    fn trade_history_fallback_no_directions() {
        let fills = vec![
            make_fill("ETH", "B", "2000", "1.0", Some("50"), Some("1"), None, 1000),
            make_fill("ETH", "B", "2100", "0.5", None, Some("0.5"), None, 2000),
        ];
        let trades = build_trade_history("0xTrader", "hyperliquid-mainnet", &fills);
        // Only fill with closed_pnl produces a trade
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].realized_pnl, BigDecimal::from_str("50").unwrap());
    }

    #[test]
    fn trade_history_empty_fills() {
        let trades = build_trade_history("0xTrader", "hyperliquid-mainnet", &[]);
        assert!(trades.is_empty());
    }

    #[test]
    fn trader_analytics_aggregation() {
        let summaries = vec![
            HlPnlSummary {
                id: Uuid::new_v4(),
                wallet_address: "0xTrader".to_string(),
                coin: "ETH".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                period_start: 0,
                period_end: 3000,
                total_closed_pnl: BigDecimal::from(100),
                total_funding: BigDecimal::from(10),
                total_fees: BigDecimal::from(5),
                net_pnl: BigDecimal::from(105),
                trade_count: 3,
                fill_count: 5,
                avg_trade_size: BigDecimal::from(1),
                win_count: 2,
                loss_count: 1,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            HlPnlSummary {
                id: Uuid::new_v4(),
                wallet_address: "0xTrader".to_string(),
                coin: "BTC".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                period_start: 0,
                period_end: 3000,
                total_closed_pnl: BigDecimal::from(-20),
                total_funding: BigDecimal::from(5),
                total_fees: BigDecimal::from(2),
                net_pnl: BigDecimal::from(-17),
                trade_count: 1,
                fill_count: 2,
                avg_trade_size: BigDecimal::from_str("0.1").unwrap(),
                win_count: 0,
                loss_count: 1,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let trades = vec![HlTradeHistory {
            id: Uuid::new_v4(),
            wallet_address: "0xTrader".to_string(),
            coin: "ETH".to_string(),
            network: "hyperliquid-mainnet".to_string(),
            side: "B".to_string(),
            entry_price: BigDecimal::from(2000),
            exit_price: BigDecimal::from(2200),
            size: BigDecimal::from(1),
            opened_at: 1000,
            closed_at: 3000,
            realized_pnl: BigDecimal::from(200),
            fees: BigDecimal::from(3),
            num_fills: 3,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        }];

        let analytics = build_trader_analytics("0xTrader", &summaries, &trades);
        assert_eq!(analytics.wallet_address, "0xTrader");
        // 105 + (-17) = 88
        assert_eq!(analytics.total_net_pnl, BigDecimal::from(88));
        assert_eq!(analytics.total_trades, 1);
        // win_rate = 2 / (2+1+0+1) = 2/4 = 0.5
        assert!((analytics.win_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(analytics.coin_breakdown.len(), 2);
    }

    #[test]
    fn market_analytics_aggregation() {
        let summaries = vec![
            HlPnlSummary {
                id: Uuid::new_v4(),
                wallet_address: "0xAlice".to_string(),
                coin: "ETH".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                period_start: 0,
                period_end: 3000,
                total_closed_pnl: BigDecimal::from(100),
                total_funding: BigDecimal::from(0),
                total_fees: BigDecimal::from(0),
                net_pnl: BigDecimal::from(100),
                trade_count: 1,
                fill_count: 1,
                avg_trade_size: BigDecimal::from(1),
                win_count: 1,
                loss_count: 0,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            HlPnlSummary {
                id: Uuid::new_v4(),
                wallet_address: "0xBob".to_string(),
                coin: "ETH".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                period_start: 0,
                period_end: 3000,
                total_closed_pnl: BigDecimal::from(-50),
                total_funding: BigDecimal::from(0),
                total_fees: BigDecimal::from(0),
                net_pnl: BigDecimal::from(-50),
                trade_count: 1,
                fill_count: 1,
                avg_trade_size: BigDecimal::from(1),
                win_count: 0,
                loss_count: 1,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let trades = vec![
            HlTradeHistory {
                id: Uuid::new_v4(),
                wallet_address: "0xAlice".to_string(),
                coin: "ETH".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                side: "B".to_string(),
                entry_price: BigDecimal::from(2000),
                exit_price: BigDecimal::from(2100),
                size: BigDecimal::from(1),
                opened_at: 1000,
                closed_at: 2000,
                realized_pnl: BigDecimal::from(100),
                fees: BigDecimal::from(1),
                num_fills: 2,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
            HlTradeHistory {
                id: Uuid::new_v4(),
                wallet_address: "0xBob".to_string(),
                coin: "ETH".to_string(),
                network: "hyperliquid-mainnet".to_string(),
                side: "A".to_string(),
                entry_price: BigDecimal::from(2100),
                exit_price: BigDecimal::from(2050),
                size: BigDecimal::from(2),
                opened_at: 1500,
                closed_at: 2500,
                realized_pnl: BigDecimal::from(-100),
                fees: BigDecimal::from(2),
                num_fills: 2,
                dataset_version_id: None,
                created_at: chrono::Utc::now(),
            },
        ];

        let analytics = build_market_analytics(&summaries, &trades);
        assert_eq!(analytics.coins.len(), 1);
        assert_eq!(analytics.coins[0].coin, "ETH");
        assert_eq!(analytics.total_unique_traders, 2);
        assert_eq!(analytics.coins[0].unique_traders, 2);
        assert_eq!(analytics.coins[0].total_trades, 2);
        // total_pnl: 100 + (-50) = 50
        assert_eq!(analytics.coins[0].total_pnl, BigDecimal::from(50));
    }

    #[test]
    fn hl_pnl_summary_serde_roundtrip() {
        let s = HlPnlSummary {
            id: Uuid::nil(),
            wallet_address: "0xTrader".to_string(),
            coin: "ETH".to_string(),
            network: "hyperliquid-mainnet".to_string(),
            period_start: 1000,
            period_end: 2000,
            total_closed_pnl: BigDecimal::from(100),
            total_funding: BigDecimal::from(10),
            total_fees: BigDecimal::from(5),
            net_pnl: BigDecimal::from(105),
            trade_count: 3,
            fill_count: 5,
            avg_trade_size: BigDecimal::from(1),
            win_count: 2,
            loss_count: 1,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: HlPnlSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.coin, "ETH");
        assert_eq!(back.net_pnl, BigDecimal::from(105));
    }

    #[test]
    fn hl_trade_history_serde_roundtrip() {
        let t = HlTradeHistory {
            id: Uuid::nil(),
            wallet_address: "0xTrader".to_string(),
            coin: "BTC".to_string(),
            network: "hyperliquid-mainnet".to_string(),
            side: "B".to_string(),
            entry_price: BigDecimal::from(50000),
            exit_price: BigDecimal::from(51000),
            size: BigDecimal::from_str("0.5").unwrap(),
            opened_at: 1000,
            closed_at: 3000,
            realized_pnl: BigDecimal::from(500),
            fees: BigDecimal::from(10),
            num_fills: 3,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: HlTradeHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.coin, "BTC");
        assert_eq!(back.realized_pnl, BigDecimal::from(500));
        assert_eq!(back.num_fills, 3);
    }

    #[test]
    fn trader_analytics_serde_roundtrip() {
        let a = TraderAnalytics {
            wallet_address: "0xTrader".to_string(),
            total_net_pnl: BigDecimal::from(100),
            total_volume: BigDecimal::from(50000),
            total_trades: 5,
            win_rate: 0.6,
            coin_breakdown: vec![CoinPnlSummary {
                coin: "ETH".to_string(),
                net_pnl: BigDecimal::from(100),
                volume: BigDecimal::from(50000),
                trade_count: 5,
                win_count: 3,
                loss_count: 2,
            }],
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: TraderAnalytics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wallet_address, "0xTrader");
        assert_eq!(back.coin_breakdown.len(), 1);
    }

    #[test]
    fn market_analytics_serde_roundtrip() {
        let m = MarketAnalytics {
            coins: vec![CoinMarketSummary {
                coin: "ETH".to_string(),
                total_volume: BigDecimal::from(100000),
                unique_traders: 5,
                total_trades: 10,
                total_pnl: BigDecimal::from(500),
                avg_trade_size: BigDecimal::from(1),
            }],
            total_volume: BigDecimal::from(100000),
            total_unique_traders: 5,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MarketAnalytics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.coins.len(), 1);
        assert_eq!(back.total_unique_traders, 5);
    }
}
