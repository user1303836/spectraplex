use bigdecimal::BigDecimal;
use spectraplex_core::models::{EntryType, LedgerEntry, Transaction};
use tracing::warn;

use crate::deterministic_id;

/// ERC-20 Transfer event signature: keccak256("Transfer(address,address,uint256)")
const ERC20_TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Parse an EVM transaction (stored as Bronze layer) into Silver-layer LedgerEntries.
///
/// Handles:
/// - ERC-20 Transfer events (decoded from log topics/data)
/// - Native ETH value transfers (from raw_metadata.value if present)
/// - Gas fees (from raw_metadata)
pub fn parse_evm_transaction(tx: &Transaction) -> anyhow::Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();
    let mut entry_index: u32 = 0;
    let wallet = tx.wallet_address.to_lowercase();

    // 1. Try to parse as an ERC-20 Transfer log
    if let Some(topics) = tx.raw_metadata.get("topics").and_then(|t| t.as_array()) {
        if let Some(topic0) = topics.first().and_then(|t| t.as_str()) {
            if topic0 == ERC20_TRANSFER_TOPIC && topics.len() >= 3 {
                let from = topic_to_address(topics[1].as_str().unwrap_or_default());
                let to = topic_to_address(topics[2].as_str().unwrap_or_default());

                // Decode uint256 amount from data field
                let data_hex = tx
                    .raw_metadata
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("0x0");
                let amount_bd = match hex_to_bigdecimal(data_hex) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(tx_hash = %tx.tx_hash, field = "data", raw = %data_hex, "Skipping ERC-20 transfer with malformed amount: {e}");
                        return Ok(entries);
                    }
                };

                // The contract address is the token address
                let token_address = tx
                    .raw_metadata
                    .get("address")
                    .and_then(|a| a.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let decimals = token_decimals(&token_address);
                let symbol = token_symbol(&token_address);
                let normalized = normalize_bigdecimal(amount_bd, decimals);

                // Determine if this wallet sent or received
                if from == wallet {
                    // Outgoing transfer
                    entries.push(LedgerEntry {
                        id: deterministic_id(tx.id, entry_index),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: symbol.clone(),
                        amount: negate(normalized.clone()),
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
                    entry_index += 1;
                }

                if to == wallet {
                    // Incoming transfer
                    entries.push(LedgerEntry {
                        id: deterministic_id(tx.id, entry_index),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: symbol,
                        amount: normalized,
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
                    entry_index += 1;
                }
            }
        }
    }

    // 2. Native ETH value (if present in metadata, e.g. from transaction receipt)
    if let Some(value_hex) = tx.raw_metadata.get("value").and_then(|v| v.as_str()) {
        let wei = hex_to_u128(value_hex);
        if wei > 0 {
            let eth_amount = wei_to_eth(wei);

            // Determine direction from "from"/"to" fields if present
            let from = tx
                .raw_metadata
                .get("from")
                .and_then(|f| f.as_str())
                .unwrap_or_default()
                .to_lowercase();
            let to = tx
                .raw_metadata
                .get("to")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_lowercase();

            if from == wallet {
                entries.push(LedgerEntry {
                    id: deterministic_id(tx.id, entry_index),
                    transaction_id: tx.id,
                    user_id: tx.user_id,
                    wallet_address: tx.wallet_address.clone(),
                    asset_symbol: "ETH".to_string(),
                    amount: negate(eth_amount.clone()),
                    entry_type: EntryType::Transfer,
                    fiat_value: None,
                });
                entry_index += 1;
            }

            if to == wallet {
                entries.push(LedgerEntry {
                    id: deterministic_id(tx.id, entry_index),
                    transaction_id: tx.id,
                    user_id: tx.user_id,
                    wallet_address: tx.wallet_address.clone(),
                    asset_symbol: "ETH".to_string(),
                    amount: eth_amount,
                    entry_type: EntryType::Transfer,
                    fiat_value: None,
                });
                entry_index += 1;
            }
        }
    }

    // 3. Gas fee (if gas_used and effective_gas_price present)
    if let (Some(gas_used_hex), Some(gas_price_hex)) = (
        tx.raw_metadata.get("gas_used").and_then(|g| g.as_str()),
        tx.raw_metadata
            .get("effective_gas_price")
            .and_then(|g| g.as_str()),
    ) {
        let gas_used = hex_to_u128(gas_used_hex);
        let gas_price = hex_to_u128(gas_price_hex);
        let fee_wei = gas_used.saturating_mul(gas_price);

        if fee_wei > 0 {
            let fee_eth = wei_to_eth(fee_wei);
            entries.push(LedgerEntry {
                id: deterministic_id(tx.id, entry_index),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "ETH".to_string(),
                amount: negate(fee_eth),
                entry_type: EntryType::Fee,
                fiat_value: None,
            });
            entry_index += 1;
        }
    }

    let _ = entry_index;
    Ok(entries)
}

/// Extract an address from a 32-byte padded topic (last 20 bytes).
fn topic_to_address(topic: &str) -> String {
    let stripped = topic.strip_prefix("0x").unwrap_or(topic);
    if stripped.len() >= 40 {
        // Take last 40 hex chars (20 bytes = Ethereum address)
        let addr = &stripped[stripped.len() - 40..];
        format!("0x{addr}")
    } else {
        stripped.to_string()
    }
}

/// Parse a hex string (with optional 0x prefix) into BigDecimal.
/// Handles full uint256 range without truncation.
fn hex_to_bigdecimal(hex: &str) -> anyhow::Result<BigDecimal> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    if stripped.is_empty() {
        return Ok(BigDecimal::from(0));
    }
    // Parse hex digits in chunks to build a BigDecimal without overflow
    let mut result = BigDecimal::from(0);
    let base = BigDecimal::from(16);
    for ch in stripped.chars() {
        let digit = match ch.to_ascii_lowercase() {
            '0'..='9' => ch as u32 - '0' as u32,
            'a'..='f' => ch as u32 - 'a' as u32 + 10,
            _ => {
                return Err(anyhow::anyhow!(
                    "Invalid hex character '{}' in value: {}",
                    ch,
                    hex
                ))
            }
        };
        result = result * &base + BigDecimal::from(digit);
    }
    Ok(result)
}

/// Parse a hex string (with optional 0x prefix) into u128.
/// Suitable for values known to fit in u128 (e.g., gas values).
fn hex_to_u128(hex: &str) -> u128 {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    if stripped.is_empty() {
        return 0;
    }
    u128::from_str_radix(stripped, 16).unwrap_or(0)
}

/// Convert wei (u128) to ETH as BigDecimal (divide by 10^18).
fn wei_to_eth(wei: u128) -> BigDecimal {
    use bigdecimal::FromPrimitive;
    let wei_bd = BigDecimal::from_u128(wei).unwrap_or_default();
    let divisor = BigDecimal::from_u128(1_000_000_000_000_000_000u128).unwrap_or_default();
    wei_bd / divisor
}

/// Lookup the number of decimals for well-known ERC-20 tokens.
/// Defaults to 18 for unknown tokens.
fn token_decimals(contract_address: &str) -> u32 {
    match contract_address.to_lowercase().as_str() {
        // ---------------------------------------------------------------
        // USDC (6 decimals) — multi-chain deployments
        // ---------------------------------------------------------------
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => 6, // USDC – Ethereum
        "0xaf88d065e77c8cc2239327c5edb3a432268e5831" => 6, // USDC – Arbitrum (native)
        "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8" => 6, // USDC.e – Arbitrum (bridged)
        "0x0b2c639c533813f4aa9d7837caf62653d097ff85" => 6, // USDC – Optimism (native)
        "0x7f5c764cbc14f9669b88837ca1490cca17c31607" => 6, // USDC.e – Optimism (bridged)
        "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359" => 6, // USDC – Polygon (native)
        "0x2791bca1f2de4661ed88a30c99a7a9449aa84174" => 6, // USDC.e – Polygon (bridged)
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913" => 6, // USDC – Base (native)
        "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca" => 6, // USDbC – Base (bridged)
        "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e" => 6, // USDC – Avalanche (native)
        "0xa7d7079b0fead91f3e65f86e8915cb59c1a4c664" => 6, // USDC.e – Avalanche (bridged)

        // ---------------------------------------------------------------
        // USDT (6 decimals) — multi-chain deployments
        // ---------------------------------------------------------------
        "0xdac17f958d2ee523a2206206994597c13d831ec7" => 6, // USDT – Ethereum
        "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9" => 6, // USDT – Arbitrum
        "0x94b008aa00579c1307b0ef2c499ad98a8ce58e58" => 6, // USDT – Optimism
        "0xc2132d05d31c914a87c6611c10748aeb04b58e8f" => 6, // USDT – Polygon
        "0x9702230a8ea53601f5cd2dc00fdbc13d4df4a8c7" => 6, // USDT – Avalanche

        // ---------------------------------------------------------------
        // WBTC (8 decimals)
        // ---------------------------------------------------------------
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => 8, // WBTC – Ethereum
        "0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f" => 8, // WBTC – Arbitrum
        "0x68f180fcce6836688e9084f035309e29bf0a2095" => 8, // WBTC – Optimism
        "0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6" => 8, // WBTC – Polygon
        "0x50b7545627a5162f82a992c33b87adc75187b218" => 8, // WBTC – Avalanche

        // ---------------------------------------------------------------
        // 18-decimal tokens – Ethereum mainnet
        // ---------------------------------------------------------------
        "0x6b175474e89094c44da98b954eedeac495271d0f" => 18, // DAI
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => 18, // WETH
        "0x514910771af9ca656af840dff83e8264ecf986ca" => 18, // LINK
        "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984" => 18, // UNI
        "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce" => 18, // SHIB
        "0x7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9" => 18, // AAVE
        "0xc00e94cb662c3520282e6f5717214004a7f26888" => 18, // COMP
        "0x9f8f72aa9304c8b593d555f12ef6589cc3a579a2" => 18, // MKR
        "0xc011a73ee8576fb46f5e1c5751ca3b9fe0af2a6f" => 18, // SNX
        "0xd533a949740bb3306d119cc777fa900ba034cd52" => 18, // CRV
        "0x6b3595068778dd592e39a122f4f5a5cf09c90fe2" => 18, // SUSHI
        "0x0bc529c00c6401aef6d220be8c6ea1667f6ad93e" => 18, // YFI
        "0xba100000625a3754423978a60c9317c58a424e3d" => 18, // BAL
        "0x7d1afa7b718fb893db30a3abc0cfc608aacfebb0" => 18, // MATIC (Ethereum)
        "0x4d224452801aced8b2f0aebe155379bb5d594381" => 18, // APE
        "0x5a98fcbea516cf06857215779fd812ca3bef1b32" => 18, // LDO
        "0xd33526068d116ce69f19a9ee46f0bd304f21a51f" => 18, // RPL
        "0x3432b6a60d23ca0dfca7761b7ab56459d9c964d0" => 18, // FXS
        "0x4e3fbd56cd56c3e72c1403e103b45db9da5b9d2b" => 18, // CVX
        "0x853d955acef822db058eb8505911ed77f175b99e" => 18, // FRAX
        "0x5f98805a4e8be255a32880fdec7f6728c6568ba0" => 18, // LUSD
        "0x57ab1ec28d129707052df4df418d58a2d46d5f51" => 18, // sUSD

        // ---------------------------------------------------------------
        // DAI – multi-chain deployments (18 decimals)
        // ---------------------------------------------------------------
        "0xda10009cbd5d07dd0cecc66161fc93d7c9000da1" => 18, // DAI – Arbitrum & Optimism
        "0x8f3cf7ad23cd3cadbd9735aff958023239c6a063" => 18, // DAI – Polygon
        "0xd586e7f844cea2f87f50152665bcbc2c279d8d70" => 18, // DAI.e – Avalanche

        // ---------------------------------------------------------------
        // WETH – L2 deployments (18 decimals)
        // ---------------------------------------------------------------
        "0x82af49447d8a07e3bd95bd0d56f35241523fbab1" => 18, // WETH – Arbitrum
        "0x4200000000000000000000000000000000000006" => 18, // WETH – Optimism & Base
        "0x7ceb23fd6bc0add59e62ac25578270cff1b9f619" => 18, // WETH – Polygon
        "0x49d5c2bdffac6ce2bfdb6fd9b3c6573c5b1d790a" => 18, // WETH.e – Avalanche

        _ => 18,
    }
}

/// Lookup a human-readable symbol for well-known ERC-20 tokens.
/// Falls back to the contract address for unknown tokens.
fn token_symbol(contract_address: &str) -> String {
    match contract_address.to_lowercase().as_str() {
        // USDC — multi-chain
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => "USDC".to_string(),
        "0xaf88d065e77c8cc2239327c5edb3a432268e5831" => "USDC".to_string(),
        "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8" => "USDC.e".to_string(),
        "0x0b2c639c533813f4aa9d7837caf62653d097ff85" => "USDC".to_string(),
        "0x7f5c764cbc14f9669b88837ca1490cca17c31607" => "USDC.e".to_string(),
        "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359" => "USDC".to_string(),
        "0x2791bca1f2de4661ed88a30c99a7a9449aa84174" => "USDC.e".to_string(),
        "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913" => "USDC".to_string(),
        "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca" => "USDbC".to_string(),
        "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e" => "USDC".to_string(),
        "0xa7d7079b0fead91f3e65f86e8915cb59c1a4c664" => "USDC.e".to_string(),

        // USDT — multi-chain
        "0xdac17f958d2ee523a2206206994597c13d831ec7" => "USDT".to_string(),
        "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9" => "USDT".to_string(),
        "0x94b008aa00579c1307b0ef2c499ad98a8ce58e58" => "USDT".to_string(),
        "0xc2132d05d31c914a87c6611c10748aeb04b58e8f" => "USDT".to_string(),
        "0x9702230a8ea53601f5cd2dc00fdbc13d4df4a8c7" => "USDT".to_string(),

        // WBTC — multi-chain
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => "WBTC".to_string(),
        "0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f" => "WBTC".to_string(),
        "0x68f180fcce6836688e9084f035309e29bf0a2095" => "WBTC".to_string(),
        "0x1bfd67037b42cf73acf2047067bd4f2c47d9bfd6" => "WBTC".to_string(),
        "0x50b7545627a5162f82a992c33b87adc75187b218" => "WBTC".to_string(),

        // 18-decimal tokens — Ethereum mainnet
        "0x6b175474e89094c44da98b954eedeac495271d0f" => "DAI".to_string(),
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => "WETH".to_string(),
        "0x514910771af9ca656af840dff83e8264ecf986ca" => "LINK".to_string(),
        "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984" => "UNI".to_string(),
        "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce" => "SHIB".to_string(),
        "0x7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9" => "AAVE".to_string(),
        "0xc00e94cb662c3520282e6f5717214004a7f26888" => "COMP".to_string(),
        "0x9f8f72aa9304c8b593d555f12ef6589cc3a579a2" => "MKR".to_string(),
        "0xc011a73ee8576fb46f5e1c5751ca3b9fe0af2a6f" => "SNX".to_string(),
        "0xd533a949740bb3306d119cc777fa900ba034cd52" => "CRV".to_string(),
        "0x6b3595068778dd592e39a122f4f5a5cf09c90fe2" => "SUSHI".to_string(),
        "0x0bc529c00c6401aef6d220be8c6ea1667f6ad93e" => "YFI".to_string(),
        "0xba100000625a3754423978a60c9317c58a424e3d" => "BAL".to_string(),
        "0x7d1afa7b718fb893db30a3abc0cfc608aacfebb0" => "MATIC".to_string(),
        "0x4d224452801aced8b2f0aebe155379bb5d594381" => "APE".to_string(),
        "0x5a98fcbea516cf06857215779fd812ca3bef1b32" => "LDO".to_string(),
        "0xd33526068d116ce69f19a9ee46f0bd304f21a51f" => "RPL".to_string(),
        "0x3432b6a60d23ca0dfca7761b7ab56459d9c964d0" => "FXS".to_string(),
        "0x4e3fbd56cd56c3e72c1403e103b45db9da5b9d2b" => "CVX".to_string(),
        "0x853d955acef822db058eb8505911ed77f175b99e" => "FRAX".to_string(),
        "0x5f98805a4e8be255a32880fdec7f6728c6568ba0" => "LUSD".to_string(),
        "0x57ab1ec28d129707052df4df418d58a2d46d5f51" => "sUSD".to_string(),

        // DAI — multi-chain
        "0xda10009cbd5d07dd0cecc66161fc93d7c9000da1" => "DAI".to_string(),
        "0x8f3cf7ad23cd3cadbd9735aff958023239c6a063" => "DAI".to_string(),
        "0xd586e7f844cea2f87f50152665bcbc2c279d8d70" => "DAI.e".to_string(),

        // WETH — L2 deployments
        "0x82af49447d8a07e3bd95bd0d56f35241523fbab1" => "WETH".to_string(),
        "0x4200000000000000000000000000000000000006" => "WETH".to_string(),
        "0x7ceb23fd6bc0add59e62ac25578270cff1b9f619" => "WETH".to_string(),
        "0x49d5c2bdffac6ce2bfdb6fd9b3c6573c5b1d790a" => "WETH.e".to_string(),

        _ => contract_address.to_string(),
    }
}

/// Normalize a BigDecimal raw amount by dividing by 10^decimals.
fn normalize_bigdecimal(raw: BigDecimal, decimals: u32) -> BigDecimal {
    let divisor = BigDecimal::new(1.into(), -i64::from(decimals));
    raw / divisor
}

/// Negate a BigDecimal value.
fn negate(val: BigDecimal) -> BigDecimal {
    -val
}

// ---------------------------------------------------------------------------
// Token Transfer extraction (P3-W2)
// ---------------------------------------------------------------------------

use chrono::Utc;
use spectraplex_core::materializer::{
    DatasetDescriptor, DatasetName, DecodedEvent, Materializer, TokenTransfer,
};
use spectraplex_core::v2::ChainFamily;
use uuid::Uuid;

/// Extract ERC-20 token transfers from an EVM raw transaction.
///
/// Decodes Transfer(address,address,uint256) events from raw log topics/data.
/// Does not filter by wallet — emits all transfers found in the transaction.
pub fn extract_evm_token_transfers(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<TokenTransfer> {
    let mut transfers = Vec::new();

    let topics = match raw_metadata.get("topics").and_then(|t| t.as_array()) {
        Some(t) => t,
        None => return vec![],
    };

    let topic0 = match topics.first().and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return vec![],
    };

    if topic0 != ERC20_TRANSFER_TOPIC || topics.len() < 3 {
        return vec![];
    }

    let from = topic_to_address(topics[1].as_str().unwrap_or_default());
    let to = topic_to_address(topics[2].as_str().unwrap_or_default());

    let data_hex = raw_metadata
        .get("data")
        .and_then(|d| d.as_str())
        .unwrap_or("0x0");
    let amount_bd = match hex_to_bigdecimal(data_hex) {
        Ok(v) => v,
        Err(e) => {
            warn!(field = "data", raw = %data_hex, "Skipping EVM token transfer with malformed amount: {e}");
            return vec![];
        }
    };

    let token_address = raw_metadata
        .get("address")
        .and_then(|a| a.as_str())
        .unwrap_or("unknown")
        .to_string();

    let decimals = token_decimals(&token_address);
    let symbol = token_symbol(&token_address);
    let normalized = normalize_bigdecimal(amount_bd, decimals);

    transfers.push(TokenTransfer {
        id: Uuid::new_v4(),
        raw_transaction_id: raw_tx_id,
        network: network.to_string(),
        token_address,
        token_symbol: Some(symbol),
        from_address: from,
        to_address: to,
        amount: normalized,
        decimals: decimals as i32,
        transfer_index: 0,
        dataset_version_id: None,
        created_at: Utc::now(),
    });

    transfers
}

// ---------------------------------------------------------------------------
// Decoded Event extraction (P3-W3)
// ---------------------------------------------------------------------------

/// ERC-20 Approval event signature: keccak256("Approval(address,address,uint256)")
const ERC20_APPROVAL_TOPIC: &str =
    "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925";

/// Extract decoded events from an EVM raw transaction's log metadata.
///
/// Decodes ALL log events, not just ERC-20 Transfer events. For well-known
/// events (Transfer, Approval), provides named decoded_fields. For unknown
/// events, decoded_fields mirrors raw topic/data structure.
pub fn extract_evm_decoded_events(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<DecodedEvent> {
    let mut events = Vec::new();

    // Handle single-log format (topics/data/address at top level)
    if let Some(topics) = raw_metadata.get("topics").and_then(|t| t.as_array()) {
        let address = raw_metadata
            .get("address")
            .and_then(|a| a.as_str())
            .unwrap_or("unknown")
            .to_string();

        let data_hex = raw_metadata
            .get("data")
            .and_then(|d| d.as_str())
            .unwrap_or("0x");

        let log_index = raw_metadata
            .get("logIndex")
            .or_else(|| raw_metadata.get("log_index"))
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(hex_to_i64_opt)))
            .unwrap_or(0) as i32;

        let topic0 = topics.first().and_then(|t| t.as_str());

        let (event_name, decoded_fields) = decode_evm_event_fields(topic0, topics, data_hex);

        let raw_fields = serde_json::json!({
            "topics": topics,
            "data": data_hex,
        });

        events.push(DecodedEvent {
            id: Uuid::new_v4(),
            raw_transaction_id: raw_tx_id,
            network: network.to_string(),
            program_or_contract: address,
            event_signature: topic0.map(|s| s.to_string()),
            event_name,
            log_index,
            decoded_fields,
            raw_fields,
            dataset_version_id: None,
            created_at: Utc::now(),
        });
    }

    // Handle multi-log format (logs array)
    if let Some(logs) = raw_metadata.get("logs").and_then(|l| l.as_array()) {
        for (idx, log) in logs.iter().enumerate() {
            let topics = match log.get("topics").and_then(|t| t.as_array()) {
                Some(t) => t,
                None => continue,
            };

            let address = log
                .get("address")
                .and_then(|a| a.as_str())
                .unwrap_or("unknown")
                .to_string();

            let data_hex = log.get("data").and_then(|d| d.as_str()).unwrap_or("0x");

            let log_index = log
                .get("logIndex")
                .or_else(|| log.get("log_index"))
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(hex_to_i64_opt)))
                .unwrap_or(idx as i64) as i32;

            let topic0 = topics.first().and_then(|t| t.as_str());

            let (event_name, decoded_fields) = decode_evm_event_fields(topic0, topics, data_hex);

            let raw_fields = serde_json::json!({
                "topics": topics,
                "data": data_hex,
            });

            events.push(DecodedEvent {
                id: Uuid::new_v4(),
                raw_transaction_id: raw_tx_id,
                network: network.to_string(),
                program_or_contract: address,
                event_signature: topic0.map(|s| s.to_string()),
                event_name,
                log_index,
                decoded_fields,
                raw_fields,
                dataset_version_id: None,
                created_at: Utc::now(),
            });
        }
    }

    events
}

/// Decode known EVM event fields by event signature.
/// Returns (event_name, decoded_fields).
fn decode_evm_event_fields(
    topic0: Option<&str>,
    topics: &[serde_json::Value],
    data_hex: &str,
) -> (Option<String>, serde_json::Value) {
    match topic0 {
        Some(t) if t == ERC20_TRANSFER_TOPIC && topics.len() >= 3 => {
            let from = topic_to_address(topics[1].as_str().unwrap_or_default());
            let to = topic_to_address(topics[2].as_str().unwrap_or_default());
            let value = match hex_to_bigdecimal(data_hex) {
                Ok(v) => v.to_string(),
                Err(e) => {
                    warn!(field = "data", raw = %data_hex, "Malformed hex in Transfer event: {e}");
                    data_hex.to_string()
                }
            };
            (
                Some("Transfer".to_string()),
                serde_json::json!({
                    "from": from,
                    "to": to,
                    "value": value,
                }),
            )
        }
        Some(t) if t == ERC20_APPROVAL_TOPIC && topics.len() >= 3 => {
            let owner = topic_to_address(topics[1].as_str().unwrap_or_default());
            let spender = topic_to_address(topics[2].as_str().unwrap_or_default());
            let value = match hex_to_bigdecimal(data_hex) {
                Ok(v) => v.to_string(),
                Err(e) => {
                    warn!(field = "data", raw = %data_hex, "Malformed hex in Approval event: {e}");
                    data_hex.to_string()
                }
            };
            (
                Some("Approval".to_string()),
                serde_json::json!({
                    "owner": owner,
                    "spender": spender,
                    "value": value,
                }),
            )
        }
        Some(_) => {
            // Unknown event — provide indexed topics and data
            let indexed: Vec<String> = topics
                .iter()
                .skip(1) // skip topic0 (the signature)
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect();
            (
                None,
                serde_json::json!({
                    "indexed_topics": indexed,
                    "data": data_hex,
                }),
            )
        }
        None => {
            // Anonymous event (no topics)
            (
                None,
                serde_json::json!({
                    "data": data_hex,
                }),
            )
        }
    }
}

/// Parse hex string to i64 (for log index fields that may be hex-encoded).
fn hex_to_i64_opt(hex: &str) -> Option<i64> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    i64::from_str_radix(stripped, 16).ok()
}

// ---------------------------------------------------------------------------
// EVM Native Balance Delta: DEFERRED
// ---------------------------------------------------------------------------
//
// EVM native balance deltas require trace API (debug_traceTransaction or
// trace_transaction) data which is not yet stored in Bronze raw_transactions.
// The current Bronze model stores log-level data and transaction-level
// metadata (value, gas_used, effective_gas_price), but this is insufficient
// to compute accurate native balance deltas for all accounts in a transaction:
//
// - The "value" field only captures the direct ETH transfer.
// - Internal transactions (contract-to-contract ETH transfers) are not
//   visible without trace data.
// - Gas refunds and self-destructs also affect balances.
//
// When Bronze gains trace API support (e.g. raw_evm_traces table), this
// materializer should be implemented. Until then, DatasetName::NativeBalanceDeltas
// is not supported for EVM chain family.

// ---------------------------------------------------------------------------
// Materializer implementations
// ---------------------------------------------------------------------------

/// Materializer wrapper for the EVM ledger parser.
pub struct EvmLedgerMaterializer;

impl Materializer for EvmLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:evm_ledger_v1_b7e4d1f5"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "EVM ledger entries: ERC-20 transfers, native ETH value transfers, and gas fees"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for EVM token transfers (ERC-20 Transfer events).
pub struct EvmTokenTransferMaterializer;

impl Materializer for EvmTokenTransferMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::TokenTransfers
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:evm_token_transfers_v1_a1c5e7b9"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "EVM token transfers: ERC-20 Transfer events decoded from raw log topics and data"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for EVM decoded events (all log events).
pub struct EvmDecodedEventMaterializer;

impl Materializer for EvmDecodedEventMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::DecodedEvents
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:evm_decoded_events_v1_c8d4f2a6"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "EVM decoded events: all log events with ABI decoding for known signatures"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use spectraplex_core::models::Chain;
    use uuid::Uuid;

    fn make_tx(metadata: serde_json::Value) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            wallet_address: "0xabcdef1234567890abcdef1234567890abcdef12".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xdeadbeef".to_string(),
            chain: Chain::Ethereum,
            raw_metadata: metadata,
        }
    }

    #[test]
    fn test_parse_erc20_transfer_incoming() {
        let wallet = "0xabcdef1234567890abcdef1234567890abcdef12";
        // topic1 = from (some other address), topic2 = to (our wallet, padded to 32 bytes)
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!(
            "0x000000000000000000000000{}",
            &wallet[2..] // remove 0x prefix
        );
        let metadata = json!({
            "topics": [
                ERC20_TRANSFER_TOPIC,
                from_padded,
                to_padded,
            ],
            "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000", // 1e18
            "address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
        });

        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();

        assert_eq!(entries.len(), 1);
        // USDT is a known token, so symbol should be resolved
        assert_eq!(entries[0].asset_symbol, "USDT");
        // 1e18 raw with 6 decimals = 1e12 normalized
        assert!(entries[0].amount > BigDecimal::from(0));
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
    }

    #[test]
    fn test_parse_erc20_transfer_outgoing() {
        let wallet = "0xabcdef1234567890abcdef1234567890abcdef12";
        let from_padded = format!("0x000000000000000000000000{}", &wallet[2..]);
        let to_padded = format!(
            "0x000000000000000000000000{}",
            "2222222222222222222222222222222222222222"
        );
        let metadata = json!({
            "topics": [
                ERC20_TRANSFER_TOPIC,
                from_padded,
                to_padded,
            ],
            "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000",
            "address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
        });

        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();

        assert_eq!(entries.len(), 1);
        assert!(entries[0].amount < BigDecimal::from(0)); // outgoing = negative
    }

    #[test]
    fn test_parse_native_eth_transfer() {
        let wallet = "0xabcdef1234567890abcdef1234567890abcdef12";
        let metadata = json!({
            "topics": [],
            "data": "0x",
            "value": "0xde0b6b3a7640000", // 1 ETH in wei
            "from": "0x1111111111111111111111111111111111111111",
            "to": wallet,
        });

        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "ETH");
        assert!(entries[0].amount > BigDecimal::from(0));
    }

    #[test]
    fn test_parse_gas_fee() {
        let metadata = json!({
            "topics": [],
            "data": "0x",
            "gas_used": "0x5208",           // 21000
            "effective_gas_price": "0x3b9aca00", // 1 gwei
        });

        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();

        // Gas fee: 21000 * 1e9 = 21000000000000 wei = 0.000021 ETH
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "ETH");
        assert!(entries[0].amount < BigDecimal::from(0)); // fee is negative
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
    }

    #[test]
    fn test_topic_to_address() {
        let padded = "0x000000000000000000000000abcdef1234567890abcdef1234567890abcdef12";
        let addr = topic_to_address(padded);
        assert_eq!(addr, "0xabcdef1234567890abcdef1234567890abcdef12");
    }

    #[test]
    fn test_hex_to_u128() {
        assert_eq!(hex_to_u128("0x0"), 0);
        assert_eq!(hex_to_u128("0x1"), 1);
        assert_eq!(hex_to_u128("0xde0b6b3a7640000"), 1_000_000_000_000_000_000);
        assert_eq!(hex_to_u128("0x5208"), 21000);
    }

    #[test]
    fn test_empty_metadata() {
        let metadata = json!({});
        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_token_decimals_known() {
        assert_eq!(
            token_decimals("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            6
        ); // USDC
        assert_eq!(
            token_decimals("0xdac17f958d2ee523a2206206994597c13d831ec7"),
            6
        ); // USDT
        assert_eq!(
            token_decimals("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
            8
        ); // WBTC
    }

    #[test]
    fn test_token_decimals_unknown_defaults_to_18() {
        assert_eq!(
            token_decimals("0x0000000000000000000000000000000000000001"),
            18
        );
    }

    #[test]
    fn test_token_symbol_known() {
        assert_eq!(
            token_symbol("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            "USDC"
        );
        assert_eq!(
            token_symbol("0xdac17f958d2ee523a2206206994597c13d831ec7"),
            "USDT"
        );
    }

    #[test]
    fn test_token_symbol_unknown_returns_address() {
        let addr = "0x0000000000000000000000000000000000000001";
        assert_eq!(token_symbol(addr), addr);
    }

    #[test]
    fn test_normalize_bigdecimal() {
        // 1_000_000 raw with 6 decimals = 1.0
        let result = normalize_bigdecimal(BigDecimal::from(1_000_000), 6);
        assert_eq!(result, BigDecimal::from(1));

        // 100_000_000 raw with 8 decimals = 1.0
        let result = normalize_bigdecimal(BigDecimal::from(100_000_000), 8);
        assert_eq!(result, BigDecimal::from(1));
    }

    #[test]
    fn test_gas_fee_not_duplicated_across_logs() {
        let wallet = "0xabcdef1234567890abcdef1234567890abcdef12";
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!("0x000000000000000000000000{}", &wallet[2..]);
        let tx_hash = "0xaaa";
        let user_id = Uuid::nil();
        let tx_id = Uuid::new_v4();

        // First log: has gas fields (adapter attaches to first log only)
        let first_log = Transaction {
            id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: tx_hash.to_string(),
            chain: Chain::Ethereum,
            raw_metadata: json!({
                "topics": [ERC20_TRANSFER_TOPIC, &from_padded, &to_padded],
                "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                "address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                "gas_used": "0x5208",
                "effective_gas_price": "0x3b9aca00",
                "value": "0x0",
                "from": "0x1111111111111111111111111111111111111111",
                "to": wallet,
            }),
        };

        // Second log: same tx hash, no gas fields (adapter omits them)
        let second_log = Transaction {
            id: Uuid::new_v4(),
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: tx_hash.to_string(),
            chain: Chain::Ethereum,
            raw_metadata: json!({
                "topics": [ERC20_TRANSFER_TOPIC, &from_padded, &to_padded],
                "data": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                "address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
            }),
        };

        let entries_1 = parse_evm_transaction(&first_log).unwrap();
        let entries_2 = parse_evm_transaction(&second_log).unwrap();

        let all_entries: Vec<_> = entries_1.iter().chain(entries_2.iter()).collect();

        let fee_count = all_entries
            .iter()
            .filter(|e| matches!(e.entry_type, EntryType::Fee))
            .count();
        assert_eq!(
            fee_count, 1,
            "gas fee should appear exactly once across all logs of the same tx"
        );
    }

    #[test]
    fn test_gas_fields_absent_produces_no_fee() {
        let metadata = json!({
            "topics": [],
            "data": "0x",
        });
        let tx = make_tx(metadata);
        let entries = parse_evm_transaction(&tx).unwrap();
        let fee_count = entries
            .iter()
            .filter(|e| matches!(e.entry_type, EntryType::Fee))
            .count();
        assert_eq!(fee_count, 0);
    }

    #[test]
    fn test_hex_to_bigdecimal() {
        // Small value
        assert_eq!(hex_to_bigdecimal("0xff").unwrap(), BigDecimal::from(255));
        // u128 max = 0xffffffffffffffffffffffffffffffff
        let u128_max = hex_to_bigdecimal("0xffffffffffffffffffffffffffffffff").unwrap();
        assert_eq!(u128_max, BigDecimal::from(u128::MAX));
        // Value larger than u128 (u128::MAX + 1)
        let over_u128 = hex_to_bigdecimal("0x100000000000000000000000000000000").unwrap();
        assert!(over_u128 > BigDecimal::from(u128::MAX));
        // Invalid hex returns error
        assert!(hex_to_bigdecimal("0xGG").is_err());
    }

    #[test]
    fn evm_materializer_contract() {
        let m = EvmLedgerMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::LedgerEntries);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Evm);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::LedgerEntries);
        assert_eq!(desc.chain_families, vec![ChainFamily::Evm]);
    }

    #[test]
    fn evm_token_transfer_materializer_contract() {
        let m = EvmTokenTransferMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::TokenTransfers);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Evm);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(
            m.parser_hash(),
            EvmLedgerMaterializer.parser_hash(),
            "distinct from ledger materializer"
        );
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::TokenTransfers);
    }

    #[test]
    fn evm_decoded_event_materializer_contract() {
        let m = EvmDecodedEventMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::DecodedEvents);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Evm);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(
            m.parser_hash(),
            EvmLedgerMaterializer.parser_hash(),
            "distinct from ledger materializer"
        );
        assert_ne!(
            m.parser_hash(),
            EvmTokenTransferMaterializer.parser_hash(),
            "distinct from token transfer materializer"
        );
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::DecodedEvents);
    }

    #[test]
    fn evm_all_materializers_have_distinct_hashes() {
        use std::collections::HashSet;
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(EvmLedgerMaterializer),
            Box::new(EvmTokenTransferMaterializer),
            Box::new(EvmDecodedEventMaterializer),
        ];
        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            3,
            "all EVM materializers must have distinct hashes"
        );
    }

    #[test]
    fn test_extract_evm_token_transfer_erc20() {
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!(
            "0x000000000000000000000000{}",
            "2222222222222222222222222222222222222222"
        );
        let metadata = json!({
            "topics": [
                ERC20_TRANSFER_TOPIC,
                from_padded,
                to_padded,
            ],
            "data": "0x0000000000000000000000000000000000000000000000000000000005f5e100", // 100_000_000
            "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // USDC
        });

        let transfers =
            extract_evm_token_transfers(Some(Uuid::new_v4()), "ethereum-mainnet", &metadata);

        assert_eq!(transfers.len(), 1);
        let t = &transfers[0];
        assert_eq!(t.from_address, "0x1111111111111111111111111111111111111111");
        assert_eq!(t.to_address, "0x2222222222222222222222222222222222222222");
        assert_eq!(t.token_symbol, Some("USDC".to_string()));
        assert_eq!(t.decimals, 6);
        assert_eq!(t.network, "ethereum-mainnet");
        // 100_000_000 / 10^6 = 100
        assert_eq!(t.amount, BigDecimal::from(100));
    }

    #[test]
    fn test_extract_evm_token_transfer_no_topics() {
        let metadata = json!({});
        let transfers = extract_evm_token_transfers(None, "ethereum-mainnet", &metadata);
        assert!(transfers.is_empty());
    }

    #[test]
    fn test_extract_evm_token_transfer_non_transfer_topic() {
        let metadata = json!({
            "topics": ["0xabcdef"],
            "data": "0x0",
        });
        let transfers = extract_evm_token_transfers(None, "ethereum-mainnet", &metadata);
        assert!(transfers.is_empty());
    }

    #[test]
    fn test_deterministic_ids_are_stable() {
        let tx = make_tx(json!({
            "topics": [],
            "data": "0x",
            "gas_used": "0x5208",
            "effective_gas_price": "0x3b9aca00",
        }));
        let entries1 = parse_evm_transaction(&tx).unwrap();
        let entries2 = parse_evm_transaction(&tx).unwrap();

        assert_eq!(entries1.len(), entries2.len());
        for (a, b) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(a.id, b.id);
        }
    }

    // -- Decoded Event extraction tests (P3-W3) --

    #[test]
    fn test_extract_evm_decoded_events_transfer() {
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!(
            "0x000000000000000000000000{}",
            "2222222222222222222222222222222222222222"
        );
        let metadata = json!({
            "topics": [
                ERC20_TRANSFER_TOPIC,
                from_padded,
                to_padded,
            ],
            "data": "0x0000000000000000000000000000000000000000000000000000000005f5e100",
            "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        });

        let events =
            extract_evm_decoded_events(Some(Uuid::new_v4()), "ethereum-mainnet", &metadata);

        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_name, Some("Transfer".to_string()));
        assert_eq!(e.event_signature, Some(ERC20_TRANSFER_TOPIC.to_string()));
        assert_eq!(
            e.program_or_contract,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert_eq!(e.network, "ethereum-mainnet");
        assert!(e.decoded_fields.get("from").is_some());
        assert!(e.decoded_fields.get("to").is_some());
        assert!(e.decoded_fields.get("value").is_some());
    }

    #[test]
    fn test_extract_evm_decoded_events_approval() {
        let owner_padded = format!(
            "0x000000000000000000000000{}",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        let spender_padded = format!(
            "0x000000000000000000000000{}",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        let metadata = json!({
            "topics": [
                ERC20_APPROVAL_TOPIC,
                owner_padded,
                spender_padded,
            ],
            "data": "0x00000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        });

        let events = extract_evm_decoded_events(None, "ethereum-mainnet", &metadata);

        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_name, Some("Approval".to_string()));
        assert!(e.decoded_fields.get("owner").is_some());
        assert!(e.decoded_fields.get("spender").is_some());
        assert!(e.decoded_fields.get("value").is_some());
    }

    #[test]
    fn test_extract_evm_decoded_events_unknown_event() {
        let unknown_topic = "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let indexed_topic1 = "0x0000000000000000000000001111111111111111111111111111111111111111";
        let metadata = json!({
            "topics": [unknown_topic, indexed_topic1],
            "data": "0xdeadbeef",
            "address": "0x3333333333333333333333333333333333333333",
        });

        let events =
            extract_evm_decoded_events(Some(Uuid::new_v4()), "ethereum-mainnet", &metadata);

        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_name, None);
        assert_eq!(e.event_signature, Some(unknown_topic.to_string()));
        assert!(e.decoded_fields.get("indexed_topics").is_some());
        assert!(e.decoded_fields.get("data").is_some());
    }

    #[test]
    fn test_extract_evm_decoded_events_empty_metadata() {
        let metadata = json!({});
        let events = extract_evm_decoded_events(None, "ethereum-mainnet", &metadata);
        assert!(events.is_empty());
    }

    #[test]
    fn test_extract_evm_decoded_events_multi_log() {
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!(
            "0x000000000000000000000000{}",
            "2222222222222222222222222222222222222222"
        );
        let metadata = json!({
            "logs": [
                {
                    "topics": [ERC20_TRANSFER_TOPIC, &from_padded, &to_padded],
                    "data": "0x0000000000000000000000000000000000000000000000000000000005f5e100",
                    "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "logIndex": 0
                },
                {
                    "topics": ["0xabcdef"],
                    "data": "0xdeadbeef",
                    "address": "0x3333333333333333333333333333333333333333",
                    "logIndex": 1
                }
            ]
        });

        let events =
            extract_evm_decoded_events(Some(Uuid::new_v4()), "ethereum-mainnet", &metadata);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_name, Some("Transfer".to_string()));
        assert_eq!(events[0].log_index, 0);
        assert_eq!(events[1].event_name, None);
        assert_eq!(events[1].log_index, 1);
    }

    #[test]
    fn test_extract_evm_decoded_events_anonymous() {
        let metadata = json!({
            "topics": [],
            "data": "0xdeadbeef",
            "address": "0x4444444444444444444444444444444444444444",
        });

        let events = extract_evm_decoded_events(None, "ethereum-mainnet", &metadata);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_name, None);
        assert_eq!(e.event_signature, None);
        assert_eq!(e.decoded_fields.get("data").unwrap(), "0xdeadbeef");
    }
}
