pub mod compat;
pub mod dual_write;
pub mod evm;
pub mod evm_parser;
pub mod hl_analytics;
pub mod hyperliquid;
pub mod hyperliquid_parser;
pub mod hyperliquid_ws;
pub mod ledger_derivation;
pub mod protocol_analytics;
pub mod repo;
pub mod solana;
pub mod solana_grpc;
mod solana_history;
pub mod solana_parser;
pub mod v2_repo;
mod workbench_repo;

use uuid::Uuid;

/// Validate supported payload shapes and amounts before import or materialization.
pub fn validate_import_payload(
    family: spectraplex_core::v2::ChainFamily,
    raw: &serde_json::Value,
) -> anyhow::Result<()> {
    use spectraplex_core::v2::ChainFamily;
    anyhow::ensure!(raw.is_object(), "raw_metadata must be an object");
    let decimal = |text: &str| -> anyhow::Result<()> {
        anyhow::ensure!(text.len() <= 256, "Decimal amount is too long");
        let _: bigdecimal::BigDecimal = text.parse()?;
        Ok(())
    };
    match family {
        ChainFamily::Solana => {
            let tx: solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta =
                serde_json::from_value(raw.clone())?;
            let meta = tx
                .transaction
                .meta
                .ok_or_else(|| anyhow::anyhow!("Solana metadata is required"))?;
            anyhow::ensure!(
                meta.pre_balances.len() == meta.post_balances.len(),
                "Solana balance array length mismatch"
            );
            for balances in [&meta.pre_token_balances, &meta.post_token_balances] {
                if let solana_transaction_status::option_serializer::OptionSerializer::Some(
                    balances,
                ) = balances
                {
                    for balance in balances {
                        let _: u64 = balance.ui_token_amount.amount.parse()?;
                    }
                }
            }
        }
        ChainFamily::Evm => {
            anyhow::ensure!(
                (raw["topics"].is_array() && raw["data"].is_string() && raw["address"].is_string())
                    || (raw["from"].is_string() && raw["value"].is_string())
                    || raw["logs"].is_array(),
                "EVM payload must be an RPC log or transaction"
            );
            fn check_evm_fields(raw: &serde_json::Value) -> anyhow::Result<()> {
                anyhow::ensure!(raw.is_object(), "EVM log must be an object");
                for field in ["value", "gas_used", "effective_gas_price", "data"] {
                    if let Some(value) = raw.get(field) {
                        let text = value
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("{field} must be a hex string"))?;
                        anyhow::ensure!(
                            text.starts_with("0x")
                                && text[2..].bytes().all(|b| b.is_ascii_hexdigit()),
                            "Invalid hex in {field}"
                        );
                        if field != "data" {
                            anyhow::ensure!(
                                text.len() <= 66 && text.len() > 2,
                                "Invalid uint256 in {field}"
                            );
                        }
                    }
                }
                if let Some(topics) = raw.get("topics") {
                    let topics = topics
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("topics must be an array"))?;
                    anyhow::ensure!(
                        topics.len() <= 4
                            && topics
                                .iter()
                                .all(|t| t.as_str().is_some_and(|s| s.len() == 66
                                    && s.starts_with("0x")
                                    && s[2..].bytes().all(|b| b.is_ascii_hexdigit()))),
                        "Invalid EVM topics"
                    );
                }
                Ok(())
            }
            check_evm_fields(raw)?;
            if let Some(logs) = raw["logs"].as_array() {
                for log in logs {
                    check_evm_fields(log)?;
                }
            }
            if let Some(hints) = raw["token_decimals"].as_object() {
                anyhow::ensure!(
                    hints.values().all(|v| v.as_u64().is_some_and(|n| n <= 255)),
                    "Token decimals must be integers 0–255"
                );
            }
        }
        ChainFamily::Hyperliquid => match raw["type"].as_str() {
            Some("fill") => {
                let fill: hyperliquid::HlFill = serde_json::from_value(raw["data"].clone())?;
                for text in [
                    Some(fill.px.as_str()),
                    Some(fill.sz.as_str()),
                    fill.fee.as_deref(),
                    fill.closed_pnl.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    decimal(text)?;
                }
            }
            Some("funding") => {
                let funding: hyperliquid::HlFundingEntry =
                    serde_json::from_value(raw["data"].clone())?;
                decimal(&funding.usdc)?;
                if let Some(rate) = funding.funding_rate {
                    decimal(&rate)?;
                }
            }
            Some("ledger_update") => {
                let update: hyperliquid::HlLedgerUpdate =
                    serde_json::from_value(raw["data"].clone())?;
                if matches!(update.delta["type"].as_str(), Some("deposit" | "withdraw")) {
                    decimal(
                        update.delta["usdc"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("Ledger usdc amount is required"))?,
                    )?;
                }
            }
            Some("funding_rate") => {
                let rate: hyperliquid::HlFundingRate = serde_json::from_value(raw["data"].clone())?;
                decimal(&rate.funding_rate)?;
                decimal(&rate.premium)?;
            }
            _ => anyhow::bail!("Unknown Hyperliquid payload type"),
        },
    }
    Ok(())
}

const LEDGER_ENTRY_NS: Uuid = Uuid::from_bytes([
    0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

pub fn deterministic_id(transaction_id: Uuid, entry_index: u32) -> Uuid {
    let name = format!("{}:{}", transaction_id, entry_index);
    Uuid::new_v5(&LEDGER_ENTRY_NS, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_id_is_stable() {
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let id1 = deterministic_id(tx_id, 0);
        let id2 = deterministic_id(tx_id, 0);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_deterministic_id_varies_by_index() {
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let id0 = deterministic_id(tx_id, 0);
        let id1 = deterministic_id(tx_id, 1);
        assert_ne!(id0, id1);
    }

    #[test]
    fn test_deterministic_id_varies_by_tx() {
        let tx_a = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let tx_b = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001").unwrap();
        let id_a = deterministic_id(tx_a, 0);
        let id_b = deterministic_id(tx_b, 0);
        assert_ne!(id_a, id_b);
    }

    #[test]
    fn materializer_impls_all_produce_ledger_entries() {
        use spectraplex_core::materializer::{DatasetName, Materializer};

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaLedgerMaterializer),
            Box::new(evm_parser::EvmLedgerMaterializer),
            Box::new(hyperliquid_parser::HyperliquidLedgerMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.dataset_name(),
                DatasetName::LedgerEntries,
                "materializer for {:?} should produce ledger_entries",
                m.chain_family()
            );
        }
    }

    #[test]
    fn materializer_impls_have_distinct_hashes() {
        use spectraplex_core::materializer::Materializer;
        use std::collections::HashSet;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaLedgerMaterializer),
            Box::new(evm_parser::EvmLedgerMaterializer),
            Box::new(hyperliquid_parser::HyperliquidLedgerMaterializer),
        ];

        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            materializers.len(),
            "each materializer must have a distinct parser_hash"
        );
    }

    #[test]
    fn materializer_impls_have_distinct_chain_families() {
        use spectraplex_core::materializer::Materializer;
        use spectraplex_core::v2::ChainFamily;
        use std::collections::HashSet;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaLedgerMaterializer),
            Box::new(evm_parser::EvmLedgerMaterializer),
            Box::new(hyperliquid_parser::HyperliquidLedgerMaterializer),
        ];

        let families: HashSet<ChainFamily> =
            materializers.iter().map(|m| m.chain_family()).collect();
        assert_eq!(families.len(), 3);
        assert!(families.contains(&ChainFamily::Solana));
        assert!(families.contains(&ChainFamily::Evm));
        assert!(families.contains(&ChainFamily::Hyperliquid));
    }

    #[test]
    fn materializer_descriptors_are_valid() {
        use spectraplex_core::materializer::Materializer;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaLedgerMaterializer),
            Box::new(evm_parser::EvmLedgerMaterializer),
            Box::new(hyperliquid_parser::HyperliquidLedgerMaterializer),
        ];

        for m in &materializers {
            let desc = m.descriptor();
            assert!(
                desc.validate().is_ok(),
                "descriptor for {:?} should be valid",
                m.chain_family()
            );
            assert_eq!(desc.name, m.dataset_name());
        }
    }

    // -- P3-W2: Token Transfer and Native Balance Delta materializer tests --

    #[test]
    fn all_token_transfer_materializers_produce_correct_dataset() {
        use spectraplex_core::materializer::{DatasetName, Materializer};

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaTokenTransferMaterializer),
            Box::new(evm_parser::EvmTokenTransferMaterializer),
            Box::new(hyperliquid_parser::HyperliquidTokenTransferMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.dataset_name(),
                DatasetName::TokenTransfers,
                "token transfer materializer for {:?} should produce token_transfers",
                m.chain_family()
            );
        }
    }

    #[test]
    fn all_native_balance_delta_materializers_produce_correct_dataset() {
        use spectraplex_core::materializer::{DatasetName, Materializer};

        // EVM native balance deltas are deferred — only Solana and Hyperliquid
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaNativeBalanceDeltaMaterializer),
            Box::new(hyperliquid_parser::HyperliquidNativeBalanceDeltaMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.dataset_name(),
                DatasetName::NativeBalanceDeltas,
                "native balance delta materializer for {:?} should produce native_balance_deltas",
                m.chain_family()
            );
        }
    }

    #[test]
    fn all_decoded_event_materializers_produce_correct_dataset() {
        use spectraplex_core::materializer::{DatasetName, Materializer};

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(evm_parser::EvmDecodedEventMaterializer),
            Box::new(solana_parser::SolanaDecodedEventMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.dataset_name(),
                DatasetName::DecodedEvents,
                "decoded event materializer for {:?} should produce decoded_events",
                m.chain_family()
            );
        }
    }

    #[test]
    fn all_materializers_have_globally_distinct_hashes() {
        use spectraplex_core::materializer::Materializer;
        use std::collections::HashSet;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            // Ledger materializers (existing)
            Box::new(solana_parser::SolanaLedgerMaterializer),
            Box::new(evm_parser::EvmLedgerMaterializer),
            Box::new(hyperliquid_parser::HyperliquidLedgerMaterializer),
            // Token transfer materializers (P3-W2)
            Box::new(solana_parser::SolanaTokenTransferMaterializer),
            Box::new(evm_parser::EvmTokenTransferMaterializer),
            Box::new(hyperliquid_parser::HyperliquidTokenTransferMaterializer),
            // Native balance delta materializers (P3-W2, EVM deferred)
            Box::new(solana_parser::SolanaNativeBalanceDeltaMaterializer),
            Box::new(hyperliquid_parser::HyperliquidNativeBalanceDeltaMaterializer),
            // Decoded event materializers (P3-W3)
            Box::new(evm_parser::EvmDecodedEventMaterializer),
            Box::new(solana_parser::SolanaDecodedEventMaterializer),
            // Hyperliquid Silver dataset materializers (P3-W4)
            Box::new(hyperliquid_parser::HlFillMaterializer),
            Box::new(hyperliquid_parser::HlFundingPaymentMaterializer),
            Box::new(hyperliquid_parser::HlPositionChangeMaterializer),
            // Derived ledger materializers (P3-W5)
            Box::new(ledger_derivation::SolanaDerivedLedgerMaterializer),
            Box::new(ledger_derivation::EvmDerivedLedgerMaterializer),
            Box::new(ledger_derivation::HyperliquidDerivedLedgerMaterializer),
        ];

        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            materializers.len(),
            "all 16 materializers must have globally distinct parser_hash values"
        );
    }

    #[test]
    fn all_silver_materializer_descriptors_are_valid() {
        use spectraplex_core::materializer::Materializer;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(solana_parser::SolanaTokenTransferMaterializer),
            Box::new(evm_parser::EvmTokenTransferMaterializer),
            Box::new(hyperliquid_parser::HyperliquidTokenTransferMaterializer),
            Box::new(solana_parser::SolanaNativeBalanceDeltaMaterializer),
            Box::new(hyperliquid_parser::HyperliquidNativeBalanceDeltaMaterializer),
            Box::new(evm_parser::EvmDecodedEventMaterializer),
            Box::new(solana_parser::SolanaDecodedEventMaterializer),
            // P3-W4 materializers
            Box::new(hyperliquid_parser::HlFillMaterializer),
            Box::new(hyperliquid_parser::HlFundingPaymentMaterializer),
            Box::new(hyperliquid_parser::HlPositionChangeMaterializer),
            // P3-W5 derived ledger materializers
            Box::new(ledger_derivation::SolanaDerivedLedgerMaterializer),
            Box::new(ledger_derivation::EvmDerivedLedgerMaterializer),
            Box::new(ledger_derivation::HyperliquidDerivedLedgerMaterializer),
        ];

        for m in &materializers {
            let desc = m.descriptor();
            assert!(
                desc.validate().is_ok(),
                "descriptor for {:?} / {:?} should be valid",
                m.chain_family(),
                m.dataset_name()
            );
            assert_eq!(desc.name, m.dataset_name());
        }
    }
}
