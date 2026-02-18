pub mod evm;
pub mod evm_parser;
pub mod hyperliquid;
pub mod hyperliquid_parser;
pub mod hyperliquid_ws;
pub mod repo;
pub mod solana;
pub mod solana_grpc;
pub mod solana_parser;

use uuid::Uuid;

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
}
