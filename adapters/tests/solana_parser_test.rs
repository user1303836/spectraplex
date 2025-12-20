use spectraplex_adapters::solana_parser;
use spectraplex_core::models::{Chain, Transaction};
use serde_json::json;
use uuid::Uuid;
use bigdecimal::{BigDecimal, FromPrimitive};

#[test]
fn test_parse_solana_native_transfer() {
    let wallet = "WalletAddress111111111111111111111111111111";
    
    let full_tx_json = json!({
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
            "postBalances": [9_500_000_000u64, 500_000_000],
            "innerInstructions": [],
            "logMessages": [],
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": []
        },
        "blockTime": 1672531200
    });

    let tx = Transaction {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        wallet_address: wallet.to_string(),
        timestamp: 1672531200,
        tx_hash: "sig123".to_string(),
        chain: Chain::Solana,
        raw_metadata: full_tx_json,
    };

    let entries = solana_parser::parse_solana_transaction(&tx).expect("Parser failed");
    
    assert_eq!(entries.len(), 1, "Should produce 1 entry for native SOL change");
    
    let entry = &entries[0];
    assert_eq!(entry.wallet_address, wallet);
    assert_eq!(entry.asset_symbol, "SOL");
    
    let expected_amount = BigDecimal::from_f64(-0.5).unwrap();
    assert_eq!(entry.amount, expected_amount);
}
