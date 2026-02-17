use bigdecimal::BigDecimal;
use serde_json::json;
use spectraplex_adapters::solana_parser;
use spectraplex_core::models::{Chain, EntryType, Transaction};
use std::str::FromStr;
use uuid::Uuid;

/// Helper to build a Transaction from JSON metadata.
fn make_tx(wallet: &str, raw_metadata: serde_json::Value) -> Transaction {
    Transaction {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        wallet_address: wallet.to_string(),
        timestamp: 1672531200,
        tx_hash: "sig123".to_string(),
        chain: Chain::Solana,
        raw_metadata,
    }
}

fn bd(s: &str) -> BigDecimal {
    BigDecimal::from_str(s).unwrap()
}

// -----------------------------------------------------------------------
// 1. Native SOL transfer (sender / fee payer)
// -----------------------------------------------------------------------
#[test]
fn test_native_sol_send_with_fee_separation() {
    let wallet = "WalletAddress111111111111111111111111111111";

    // Sender sends 0.5 SOL. Fee is 5000 lamports.
    // pre=10 SOL, post=9.499995 SOL  (balance change = -500_005_000 lamports)
    let meta = json!({
        "slot": 123456,
        "transaction": {
            "signatures": ["sig123"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true },
                    { "pubkey": "Receiver11111111111111111111111111111111", "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [10_000_000_000u64, 0],
            "postBalances": [9_499_995_000u64, 500_000_000u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    assert_eq!(entries.len(), 2, "Should produce fee + transfer entries");

    // First entry should be the fee
    let fee_entry = &entries[0];
    assert_eq!(fee_entry.asset_symbol, "SOL");
    assert!(matches!(fee_entry.entry_type, EntryType::Fee));
    assert_eq!(fee_entry.amount, bd("-0.000005"));

    // Second entry should be the transfer
    let transfer_entry = &entries[1];
    assert_eq!(transfer_entry.asset_symbol, "SOL");
    assert!(matches!(transfer_entry.entry_type, EntryType::Transfer));
    assert_eq!(transfer_entry.amount, bd("-0.5"));
}

// -----------------------------------------------------------------------
// 2. Native SOL receive (non-fee-payer)
// -----------------------------------------------------------------------
#[test]
fn test_native_sol_receive() {
    let receiver = "Receiver11111111111111111111111111111111";

    let meta = json!({
        "slot": 123456,
        "transaction": {
            "signatures": ["sig456"],
            "message": {
                "accountKeys": [
                    { "pubkey": "SenderXX111111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": receiver, "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [10_000_000_000u64, 1_000_000_000u64],
            "postBalances": [8_999_995_000u64, 2_000_000_000u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(receiver, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    assert_eq!(entries.len(), 1, "Receiver gets one transfer entry, no fee");
    let entry = &entries[0];
    assert_eq!(entry.asset_symbol, "SOL");
    assert!(matches!(entry.entry_type, EntryType::Transfer));
    assert_eq!(entry.amount, bd("1"));
}

// -----------------------------------------------------------------------
// 3. Fee-only transaction (e.g. a failed vote or no-op where only fee is deducted)
// -----------------------------------------------------------------------
#[test]
fn test_fee_only_transaction() {
    let wallet = "WalletAddress111111111111111111111111111111";

    // Balance changes by exactly -5000 lamports (the fee), no transfer component.
    let meta = json!({
        "slot": 200000,
        "transaction": {
            "signatures": ["sigFeeOnly"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [1_000_000_000u64],
            "postBalances": [999_995_000u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    // Only a fee entry, no transfer (transfer_lamports = -5000 + 5000 = 0)
    assert_eq!(entries.len(), 1, "Fee-only tx should produce one Fee entry");
    assert!(matches!(entries[0].entry_type, EntryType::Fee));
    assert_eq!(entries[0].amount, bd("-0.000005"));
}

// -----------------------------------------------------------------------
// 4. Failed transaction (should be skipped)
// -----------------------------------------------------------------------
#[test]
fn test_failed_transaction_skipped() {
    let wallet = "WalletAddress111111111111111111111111111111";

    let meta = json!({
        "slot": 300000,
        "transaction": {
            "signatures": ["sigFailed"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": { "InstructionError": [0, "InvalidAccountData"] },
            "status": { "Err": { "InstructionError": [0, "InvalidAccountData"] } },
            "fee": 5000,
            "preBalances": [1_000_000_000u64],
            "postBalances": [999_995_000u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    assert_eq!(
        entries.len(),
        0,
        "Failed transactions should produce no entries"
    );
}

// -----------------------------------------------------------------------
// 5. SPL Token transfer
// -----------------------------------------------------------------------
#[test]
fn test_spl_token_transfer() {
    let wallet = "WalletAddress111111111111111111111111111111";
    let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // USDC

    let meta = json!({
        "slot": 400000,
        "transaction": {
            "signatures": ["sigSPL"],
            "message": {
                "accountKeys": [
                    { "pubkey": "OtherWallet1111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": wallet, "signer": false, "writable": true },
                    { "pubkey": "TokenProgram111111111111111111111111111111", "signer": false, "writable": false }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [1_000_000_000u64, 0, 0],
            "postBalances": [999_995_000u64, 0, 0],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [
                {
                    "accountIndex": 1,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 100.0,
                        "decimals": 6,
                        "amount": "100000000",
                        "uiAmountString": "100.0"
                    },
                    "owner": wallet
                }
            ],
            "postTokenBalances": [
                {
                    "accountIndex": 1,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 150.5,
                        "decimals": 6,
                        "amount": "150500000",
                        "uiAmountString": "150.5"
                    },
                    "owner": wallet
                }
            ],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    // Wallet is not fee payer (index 1), no SOL change for wallet, but has SPL token change
    // USDC mint resolves to "USDC" symbol
    let token_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.asset_symbol == "USDC")
        .collect();
    assert_eq!(token_entries.len(), 1, "Should have one SPL token entry");
    assert_eq!(token_entries[0].amount, bd("50.5"));
    assert!(matches!(token_entries[0].entry_type, EntryType::Transfer));
}

// -----------------------------------------------------------------------
// 6. SPL Token send (negative delta)
// -----------------------------------------------------------------------
#[test]
fn test_spl_token_send() {
    let wallet = "WalletAddress111111111111111111111111111111";
    let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    let meta = json!({
        "slot": 400001,
        "transaction": {
            "signatures": ["sigSPLsend"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true },
                    { "pubkey": "Receiver11111111111111111111111111111111", "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [1_000_000_000u64, 0],
            "postBalances": [999_995_000u64, 0],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [
                {
                    "accountIndex": 0,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 200.0,
                        "decimals": 6,
                        "amount": "200000000",
                        "uiAmountString": "200.0"
                    },
                    "owner": wallet
                }
            ],
            "postTokenBalances": [
                {
                    "accountIndex": 0,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 125.0,
                        "decimals": 6,
                        "amount": "125000000",
                        "uiAmountString": "125.0"
                    },
                    "owner": wallet
                }
            ],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    // USDC mint resolves to "USDC" symbol
    let token_entries: Vec<_> = entries
        .iter()
        .filter(|e| e.asset_symbol == "USDC")
        .collect();
    assert_eq!(token_entries.len(), 1);
    assert_eq!(token_entries[0].amount, bd("-75"));
}

// -----------------------------------------------------------------------
// 7. Multi-instruction tx: SOL + SPL changes together
// -----------------------------------------------------------------------
#[test]
fn test_multi_asset_transaction() {
    let wallet = "WalletAddress111111111111111111111111111111";
    let mint = "So11111111111111111111111111111111111111112"; // wrapped SOL mint (example)

    let meta = json!({
        "slot": 500000,
        "transaction": {
            "signatures": ["sigMulti"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true },
                    { "pubkey": "DEX111111111111111111111111111111111111111", "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 10000,
            "preBalances": [5_000_000_000u64, 0],
            "postBalances": [4_899_990_000u64, 0],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [
                {
                    "accountIndex": 0,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 0.0,
                        "decimals": 9,
                        "amount": "0",
                        "uiAmountString": "0.0"
                    },
                    "owner": wallet
                }
            ],
            "postTokenBalances": [
                {
                    "accountIndex": 0,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 1.0,
                        "decimals": 9,
                        "amount": "1000000000",
                        "uiAmountString": "1.0"
                    },
                    "owner": wallet
                }
            ],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    // Should have: 1 Fee entry, 1 SOL Transfer entry, 1 SPL token entry
    let fee_entries: Vec<_> = entries
        .iter()
        .filter(|e| matches!(e.entry_type, EntryType::Fee))
        .collect();
    // Wrapped SOL mint resolves to "SOL" symbol, so all SOL-related transfer
    // entries share the same asset_symbol. Filter by amount to distinguish.
    let sol_transfers: Vec<_> = entries
        .iter()
        .filter(|e| e.asset_symbol == "SOL" && matches!(e.entry_type, EntryType::Transfer))
        .collect();

    assert_eq!(fee_entries.len(), 1, "One fee entry");
    assert_eq!(fee_entries[0].amount, bd("-0.00001"));

    // Should have 2 SOL transfer entries: native SOL (-0.1) + wrapped SOL SPL token (+1)
    assert_eq!(sol_transfers.len(), 2, "Native SOL + wrapped SOL transfers");
    let native_sol: Vec<_> = sol_transfers
        .iter()
        .filter(|e| e.amount == bd("-0.1"))
        .collect();
    let wrapped_sol: Vec<_> = sol_transfers
        .iter()
        .filter(|e| e.amount == bd("1"))
        .collect();
    assert_eq!(native_sol.len(), 1, "One native SOL transfer");
    assert_eq!(wrapped_sol.len(), 1, "One wrapped SOL SPL transfer");
}

// -----------------------------------------------------------------------
// 8. Zero balance change (wallet not involved)
// -----------------------------------------------------------------------
#[test]
fn test_zero_balance_change_no_entries() {
    let wallet = "WalletAddress111111111111111111111111111111";

    let meta = json!({
        "slot": 600000,
        "transaction": {
            "signatures": ["sigZero"],
            "message": {
                "accountKeys": [
                    { "pubkey": "OtherSigner1111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": wallet, "signer": false, "writable": false }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [1_000_000_000u64, 2_000_000_000u64],
            "postBalances": [999_995_000u64, 2_000_000_000u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    assert_eq!(
        entries.len(),
        0,
        "No entries when wallet balance doesn't change"
    );
}

// -----------------------------------------------------------------------
// 9. No meta (should return empty)
// -----------------------------------------------------------------------
#[test]
fn test_no_meta_returns_empty() {
    let wallet = "WalletAddress111111111111111111111111111111";

    let meta = json!({
        "slot": 700000,
        "transaction": {
            "signatures": ["sigNoMeta"],
            "message": {
                "accountKeys": [
                    { "pubkey": wallet, "signer": true, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": null,
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");
    assert_eq!(entries.len(), 0, "No meta should produce no entries");
}

// -----------------------------------------------------------------------
// 10. Precision: verify no floating-point drift
// -----------------------------------------------------------------------
#[test]
fn test_precision_no_floating_point_drift() {
    let wallet = "WalletAddress111111111111111111111111111111";

    // Use values that are problematic in f64: 0.1 + 0.2 != 0.3
    // 123456789 lamports = 0.123456789 SOL (exact in base-10)
    let meta = json!({
        "slot": 800000,
        "transaction": {
            "signatures": ["sigPrecision"],
            "message": {
                "accountKeys": [
                    { "pubkey": "OtherSigner1111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": wallet, "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [10_000_000_000u64, 1_000_000_000u64],
            "postBalances": [9_876_538_211u64, 1_123_456_789u64],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    assert_eq!(entries.len(), 1);
    // 1_123_456_789 - 1_000_000_000 = 123_456_789 lamports = 0.123456789 SOL
    assert_eq!(entries[0].amount, bd("0.123456789"));
}

// -----------------------------------------------------------------------
// 11. New token account (no pre-balance entry for the account)
// -----------------------------------------------------------------------
#[test]
fn test_spl_new_token_account() {
    let wallet = "WalletAddress111111111111111111111111111111";
    let mint = "TokenMint111111111111111111111111111111111";

    let meta = json!({
        "slot": 900000,
        "transaction": {
            "signatures": ["sigNewAccount"],
            "message": {
                "accountKeys": [
                    { "pubkey": "OtherSigner1111111111111111111111111111111", "signer": true, "writable": true },
                    { "pubkey": wallet, "signer": false, "writable": true }
                ],
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": null,
            "status": { "Ok": null },
            "fee": 5000,
            "preBalances": [10_000_000_000u64, 0],
            "postBalances": [9_999_995_000u64, 0],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [
                {
                    "accountIndex": 1,
                    "mint": mint,
                    "uiTokenAmount": {
                        "uiAmount": 1000.0,
                        "decimals": 9,
                        "amount": "1000000000000",
                        "uiAmountString": "1000.0"
                    },
                    "owner": wallet
                }
            ],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = make_tx(wallet, meta);
    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");

    let token_entries: Vec<_> = entries.iter().filter(|e| e.asset_symbol == mint).collect();
    assert_eq!(
        token_entries.len(),
        1,
        "New token account should produce one entry"
    );
    assert_eq!(token_entries[0].amount, bd("1000"));
}
