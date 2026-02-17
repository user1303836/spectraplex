use bigdecimal::BigDecimal;
use spectraplex_core::models::{EntryType, LedgerEntry, Transaction};
use uuid::Uuid;

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
                let amount_bd = hex_to_bigdecimal(data_hex);

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
                        id: Uuid::new_v4(),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: symbol.clone(),
                        amount: negate(normalized.clone()),
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
                }

                if to == wallet {
                    // Incoming transfer
                    entries.push(LedgerEntry {
                        id: Uuid::new_v4(),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: symbol,
                        amount: normalized,
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
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
                    id: Uuid::new_v4(),
                    transaction_id: tx.id,
                    user_id: tx.user_id,
                    wallet_address: tx.wallet_address.clone(),
                    asset_symbol: "ETH".to_string(),
                    amount: negate(eth_amount.clone()),
                    entry_type: EntryType::Transfer,
                    fiat_value: None,
                });
            }

            if to == wallet {
                entries.push(LedgerEntry {
                    id: Uuid::new_v4(),
                    transaction_id: tx.id,
                    user_id: tx.user_id,
                    wallet_address: tx.wallet_address.clone(),
                    asset_symbol: "ETH".to_string(),
                    amount: eth_amount,
                    entry_type: EntryType::Transfer,
                    fiat_value: None,
                });
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
                id: Uuid::new_v4(),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "ETH".to_string(),
                amount: negate(fee_eth),
                entry_type: EntryType::Fee,
                fiat_value: None,
            });
        }
    }

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
fn hex_to_bigdecimal(hex: &str) -> BigDecimal {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    if stripped.is_empty() {
        return BigDecimal::from(0);
    }
    // Parse hex digits in chunks to build a BigDecimal without overflow
    let mut result = BigDecimal::from(0);
    let base = BigDecimal::from(16);
    for ch in stripped.chars() {
        let digit = match ch.to_ascii_lowercase() {
            '0'..='9' => ch as u32 - '0' as u32,
            'a'..='f' => ch as u32 - 'a' as u32 + 10,
            _ => return BigDecimal::from(0),
        };
        result = result * &base + BigDecimal::from(digit);
    }
    result
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
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => 6, // USDC
        "0xdac17f958d2ee523a2206206994597c13d831ec7" => 6, // USDT
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => 8, // WBTC
        "0x6b175474e89094c44da98b954eedeac495271d0f" => 18, // DAI
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => 18, // WETH
        "0x514910771af9ca656af840dff83e8264ecf986ca" => 18, // LINK
        "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984" => 18, // UNI
        "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce" => 18, // SHIB
        _ => 18,
    }
}

/// Lookup a human-readable symbol for well-known ERC-20 tokens.
/// Falls back to the contract address for unknown tokens.
fn token_symbol(contract_address: &str) -> String {
    match contract_address.to_lowercase().as_str() {
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => "USDC".to_string(),
        "0xdac17f958d2ee523a2206206994597c13d831ec7" => "USDT".to_string(),
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => "WBTC".to_string(),
        "0x6b175474e89094c44da98b954eedeac495271d0f" => "DAI".to_string(),
        "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => "WETH".to_string(),
        "0x514910771af9ca656af840dff83e8264ecf986ca" => "LINK".to_string(),
        "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984" => "UNI".to_string(),
        "0x95ad61b0a150d79219dcf64e1e6cc01f0b64c4ce" => "SHIB".to_string(),
        _ => contract_address.to_string(),
    }
}

/// Normalize a raw token amount by dividing by 10^decimals.
/// Normalize a BigDecimal raw amount by dividing by 10^decimals.
fn normalize_bigdecimal(raw: BigDecimal, decimals: u32) -> BigDecimal {
    use std::str::FromStr;
    let divisor = BigDecimal::from_str(&format!("1{}", "0".repeat(decimals as usize)))
        .unwrap_or_else(|_| BigDecimal::from(1));
    raw / divisor
}

/// Negate a BigDecimal value.
fn negate(val: BigDecimal) -> BigDecimal {
    -val
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use spectraplex_core::models::Chain;

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
    fn test_hex_to_bigdecimal() {
        // Small value
        assert_eq!(hex_to_bigdecimal("0xff"), BigDecimal::from(255));
        // u128 max = 0xffffffffffffffffffffffffffffffff
        let u128_max = hex_to_bigdecimal("0xffffffffffffffffffffffffffffffff");
        assert_eq!(u128_max, BigDecimal::from(u128::MAX));
        // Value larger than u128 (u128::MAX + 1)
        let over_u128 = hex_to_bigdecimal("0x100000000000000000000000000000000");
        assert!(over_u128 > BigDecimal::from(u128::MAX));
    }
}
