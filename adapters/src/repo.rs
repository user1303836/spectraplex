use spectraplex_core::models::{Chain, IndexerCheckpoint, LedgerEntry, Transaction};
use sqlx::{postgres::PgPool, Row};

pub struct Repository {
    pool: PgPool,
}

impl Repository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Batch size for chunked inserts.
    const BATCH_SIZE: usize = 500;

    pub async fn save_transactions(&self, txs: &[Transaction]) -> anyhow::Result<()> {
        for chunk in txs.chunks(Self::BATCH_SIZE) {
            let mut query = String::from(
                "INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata) VALUES ",
            );
            let mut args = sqlx::postgres::PgArguments::default();
            for (i, tx) in chunk.iter().enumerate() {
                let chain_str = match tx.chain {
                    spectraplex_core::models::Chain::Solana => "solana",
                    spectraplex_core::models::Chain::Hyperliquid => "hyperliquid",
                    spectraplex_core::models::Chain::Ethereum => "ethereum",
                };
                let base = i * 7;
                if i > 0 {
                    query.push_str(", ");
                }
                query.push_str(&format!(
                    "(${}, ${}, ${}, ${}, ${}, ${}::chain_enum, ${})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7
                ));
                use sqlx::Arguments;
                args.add(tx.id).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(tx.user_id).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&tx.wallet_address)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(tx.timestamp).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&tx.tx_hash).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(chain_str).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&tx.raw_metadata)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            query.push_str(" ON CONFLICT (chain, tx_hash) DO NOTHING");
            sqlx::query_with(&query, args).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn save_ledger_entries(&self, entries: &[LedgerEntry]) -> anyhow::Result<()> {
        for chunk in entries.chunks(Self::BATCH_SIZE) {
            let mut query = String::from(
                "INSERT INTO ledger_entries (id, transaction_id, user_id, wallet_address, asset_symbol, amount, entry_type, fiat_value) VALUES ",
            );
            let mut args = sqlx::postgres::PgArguments::default();
            for (i, entry) in chunk.iter().enumerate() {
                let entry_type_str = match entry.entry_type {
                    spectraplex_core::models::EntryType::Trade => "trade",
                    spectraplex_core::models::EntryType::Fee => "fee",
                    spectraplex_core::models::EntryType::Transfer => "transfer",
                    spectraplex_core::models::EntryType::Staking => "staking",
                    spectraplex_core::models::EntryType::Income => "income",
                };
                let base = i * 8;
                if i > 0 {
                    query.push_str(", ");
                }
                query.push_str(&format!(
                    "(${}, ${}, ${}, ${}, ${}, ${}, ${}::entry_type_enum, ${})",
                    base + 1,
                    base + 2,
                    base + 3,
                    base + 4,
                    base + 5,
                    base + 6,
                    base + 7,
                    base + 8
                ));
                use sqlx::Arguments;
                args.add(entry.id).map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(entry.transaction_id)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(entry.user_id)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&entry.wallet_address)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&entry.asset_symbol)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&entry.amount)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(entry_type_str)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                args.add(&entry.fiat_value)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            query.push_str(" ON CONFLICT (id) DO NOTHING");
            sqlx::query_with(&query, args).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn get_transactions_by_wallet(
        &self,
        wallet: &str,
    ) -> anyhow::Result<Vec<Transaction>> {
        self.get_transactions_by_wallet_paginated(wallet, 1000, 0)
            .await
    }

    pub async fn get_transactions_by_wallet_paginated(
        &self,
        wallet: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Transaction>> {
        let rows = sqlx::query(
            r#"
            SELECT id, user_id, wallet_address, timestamp, tx_hash, chain::text, raw_metadata
            FROM transactions
            WHERE wallet_address = $1
            ORDER BY timestamp ASC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(wallet)
        .bind(limit)
        .bind(offset)
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

    pub async fn get_ledger_entries_by_wallet(
        &self,
        wallet: &str,
    ) -> anyhow::Result<Vec<LedgerEntry>> {
        self.get_ledger_entries_by_wallet_paginated(wallet, 1000, 0)
            .await
    }

    pub async fn get_ledger_entries_by_wallet_paginated(
        &self,
        wallet: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<LedgerEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT
                id, transaction_id, user_id, wallet_address, asset_symbol, amount,
                entry_type::text, fiat_value
            FROM ledger_entries
            WHERE wallet_address = $1
            ORDER BY created_at ASC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(wallet)
        .bind(limit)
        .bind(offset)
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

    pub async fn get_checkpoint(
        &self,
        chain: &str,
        wallet: &str,
    ) -> anyhow::Result<Option<IndexerCheckpoint>> {
        let row = sqlx::query(
            r#"
            SELECT chain::text, wallet_address, last_signature, last_slot, last_timestamp
            FROM indexer_checkpoints
            WHERE chain = $1::chain_enum AND wallet_address = $2
            "#,
        )
        .bind(chain)
        .bind(wallet)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let chain_str: String = row.try_get("chain")?;
                let chain = match chain_str.as_str() {
                    "solana" => Chain::Solana,
                    "hyperliquid" => Chain::Hyperliquid,
                    "ethereum" => Chain::Ethereum,
                    _ => return Err(anyhow::anyhow!("Unknown chain: {}", chain_str)),
                };
                Ok(Some(IndexerCheckpoint {
                    chain,
                    wallet_address: row.try_get("wallet_address")?,
                    last_signature: row.try_get("last_signature")?,
                    last_slot: row.try_get("last_slot")?,
                    last_timestamp: row.try_get("last_timestamp")?,
                }))
            }
            None => Ok(None),
        }
    }

    pub async fn save_checkpoint(&self, checkpoint: &IndexerCheckpoint) -> anyhow::Result<()> {
        let chain_str = match checkpoint.chain {
            Chain::Solana => "solana",
            Chain::Hyperliquid => "hyperliquid",
            Chain::Ethereum => "ethereum",
        };

        sqlx::query(
            r#"
            INSERT INTO indexer_checkpoints (chain, wallet_address, last_signature, last_slot, last_timestamp, updated_at)
            VALUES ($1::chain_enum, $2, $3, $4, $5, NOW())
            ON CONFLICT (chain, wallet_address)
            DO UPDATE SET
                last_signature = EXCLUDED.last_signature,
                last_slot = EXCLUDED.last_slot,
                last_timestamp = EXCLUDED.last_timestamp,
                updated_at = NOW()
            "#,
        )
        .bind(chain_str)
        .bind(&checkpoint.wallet_address)
        .bind(&checkpoint.last_signature)
        .bind(checkpoint.last_slot)
        .bind(checkpoint.last_timestamp)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
