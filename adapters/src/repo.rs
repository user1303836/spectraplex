use spectraplex_core::models::{Transaction, LedgerEntry};
use sqlx::{postgres::PgPool, Row};

pub struct Repository {
    pool: PgPool,
}

impl Repository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn save_transactions(&self, txs: &[Transaction]) -> anyhow::Result<()> {
        for tx in txs {
            let chain_str = match tx.chain {
                spectraplex_core::models::Chain::Solana => "solana",
                spectraplex_core::models::Chain::Hyperliquid => "hyperliquid",
                spectraplex_core::models::Chain::Ethereum => "ethereum",
            };

            // Using unchecked query to avoid needing a running DB during compilation
            sqlx::query(
                r#"
                INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata)
                VALUES ($1, $2, $3, $4, $5, $6::chain_enum, $7)
                ON CONFLICT (id) DO NOTHING
                "#
            )
            .bind(tx.id)
            .bind(tx.user_id)
            .bind(&tx.wallet_address)
            .bind(tx.timestamp)
            .bind(&tx.tx_hash)
            .bind(chain_str)
            .bind(&tx.raw_metadata)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn save_ledger_entries(&self, entries: &[LedgerEntry]) -> anyhow::Result<()> {
        for entry in entries {
            let entry_type_str = match entry.entry_type {
                spectraplex_core::models::EntryType::Trade => "trade",
                spectraplex_core::models::EntryType::Fee => "fee",
                spectraplex_core::models::EntryType::Transfer => "transfer",
                spectraplex_core::models::EntryType::Staking => "staking",
                spectraplex_core::models::EntryType::Income => "income",
            };
            
            sqlx::query(
                r#"
                INSERT INTO ledger_entries (id, transaction_id, user_id, wallet_address, asset_symbol, amount, entry_type, fiat_value)
                VALUES ($1, $2, $3, $4, $5, $6, $7::entry_type_enum, $8)
                ON CONFLICT (id) DO NOTHING
                "#
            )
            .bind(entry.id)
            .bind(entry.transaction_id)
            .bind(entry.user_id)
            .bind(&entry.wallet_address)
            .bind(&entry.asset_symbol)
            .bind(&entry.amount)
            .bind(entry_type_str)
            .bind(&entry.fiat_value)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
    
    pub async fn get_transactions_by_wallet(&self, wallet: &str) -> anyhow::Result<Vec<Transaction>> {
        let rows = sqlx::query(
            r#"
            SELECT id, user_id, wallet_address, timestamp, tx_hash, chain::text, raw_metadata
            FROM transactions
            WHERE wallet_address = $1
            ORDER BY timestamp ASC
            "#
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?;

        let mut txs = Vec::new();
        for row in rows {
            let chain_str: String = row.try_get("chain")?;
            let chain = match chain_str.as_str() {
                "solana" => spectraplex_core::models::Chain::Solana,
                "hyperliquid" => spectraplex_core::models::Chain::Hyperliquid,
                "ethereum" => spectraplex_core::models::Chain::Ethereum,
                _ => return Err(anyhow::anyhow!("Unknown chain: {}", chain_str)),
            };
            
            txs.push(Transaction {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                wallet_address: row.try_get("wallet_address")?,
                timestamp: row.try_get("timestamp")?,
                tx_hash: row.try_get("tx_hash")?,
                chain,
                raw_metadata: row.try_get("raw_metadata")?,
            });
        }
        Ok(txs)
    }

    pub async fn get_ledger_entries_by_wallet(&self, wallet: &str) -> anyhow::Result<Vec<LedgerEntry>> {
        // Optimized: Query directly on indexed wallet_address column
        let rows = sqlx::query(
            r#"
            SELECT 
                id, transaction_id, user_id, wallet_address, asset_symbol, amount, 
                entry_type::text, fiat_value
            FROM ledger_entries
            WHERE wallet_address = $1
            ORDER BY created_at ASC
            "#
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?;

        let mut entries = Vec::new();
        for row in rows {
            let entry_type_str: String = row.try_get("entry_type")?;
            let entry_type = match entry_type_str.as_str() {
                "trade" => spectraplex_core::models::EntryType::Trade,
                "fee" => spectraplex_core::models::EntryType::Fee,
                "transfer" => spectraplex_core::models::EntryType::Transfer,
                "staking" => spectraplex_core::models::EntryType::Staking,
                "income" => spectraplex_core::models::EntryType::Income,
                _ => spectraplex_core::models::EntryType::Transfer,
            };

            entries.push(LedgerEntry {
                id: row.try_get("id")?,
                transaction_id: row.try_get("transaction_id")?,
                user_id: row.try_get("user_id")?,
                wallet_address: row.try_get("wallet_address")?,
                asset_symbol: row.try_get("asset_symbol")?,
                amount: row.try_get("amount")?,
                entry_type,
                fiat_value: row.try_get("fiat_value")?,
            });
        }
        Ok(entries)
    }
}
