use spectraplex_core::models::{Chain, EntryType, IndexerCheckpoint, LedgerEntry, Transaction};
use sqlx::{postgres::PgPool, Executor, Postgres, Row};

fn chain_to_str(chain: &Chain) -> &'static str {
    match chain {
        Chain::Solana => "solana",
        Chain::Hyperliquid => "hyperliquid",
        Chain::Ethereum => "ethereum",
    }
}

fn str_to_chain(s: &str) -> anyhow::Result<Chain> {
    match s {
        "solana" => Ok(Chain::Solana),
        "hyperliquid" => Ok(Chain::Hyperliquid),
        "ethereum" => Ok(Chain::Ethereum),
        _ => Err(anyhow::anyhow!("Unknown chain: {}", s)),
    }
}

fn entry_type_to_str(entry_type: &EntryType) -> &'static str {
    match entry_type {
        EntryType::Trade => "trade",
        EntryType::Fee => "fee",
        EntryType::Transfer => "transfer",
        EntryType::Staking => "staking",
        EntryType::Income => "income",
    }
}

fn str_to_entry_type(s: &str) -> anyhow::Result<EntryType> {
    match s {
        "trade" => Ok(EntryType::Trade),
        "fee" => Ok(EntryType::Fee),
        "transfer" => Ok(EntryType::Transfer),
        "staking" => Ok(EntryType::Staking),
        "income" => Ok(EntryType::Income),
        _ => Err(anyhow::anyhow!("Unknown entry type: {}", s)),
    }
}

/// Chain-specific finality buffer (number of slots/blocks to subtract from
/// the latest position when building a checkpoint).  On restart the indexer
/// will re-fetch from this earlier position; duplicate transactions are
/// safely discarded by the `ON CONFLICT DO NOTHING` upsert.
fn finality_buffer(chain: &str) -> i64 {
    match chain {
        "solana" => 32,   // probabilistic slot finality (~13 s)
        "ethereum" => 15, // probabilistic block finality (~3 min)
        _ => 0,           // hyperliquid has instant finality
    }
}

pub fn build_checkpoint(
    chain: &str,
    wallet: &str,
    txs: &[Transaction],
) -> Option<IndexerCheckpoint> {
    if txs.is_empty() {
        return None;
    }

    let chain_enum = str_to_chain(chain).ok()?;
    let latest = txs.iter().max_by_key(|tx| tx.timestamp)?;
    let buffer = finality_buffer(chain);

    let last_slot = match chain {
        "solana" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("slot").and_then(|v| v.as_i64()))
            .max()
            .map(|s| (s - buffer).max(0)),
        _ => None,
    };

    let last_block = match chain {
        "ethereum" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("block_number").and_then(|v| v.as_i64()))
            .max()
            .map(|b| (b - buffer).max(0)),
        _ => None,
    };

    Some(IndexerCheckpoint {
        chain: chain_enum,
        wallet_address: wallet.to_string(),
        last_signature: Some(latest.tx_hash.clone()),
        last_slot,
        last_block,
        last_timestamp: Some(latest.timestamp),
    })
}

pub struct WalletStatsRow {
    pub tx_count: i64,
    pub earliest_timestamp: Option<i64>,
    pub latest_timestamp: Option<i64>,
    pub chain_count: i64,
    pub unique_assets: i64,
    pub per_chain: Vec<(String, i64)>,
}

#[derive(Clone)]
pub struct Repository {
    pool: PgPool,
}

impl Repository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Crate-visible accessor for the connection pool (used by `v2_repo`).
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Batch size for chunked inserts.
    const BATCH_SIZE: usize = 500;

    pub async fn save_transactions(&self, txs: &[Transaction]) -> anyhow::Result<()> {
        for chunk in txs.chunks(Self::BATCH_SIZE) {
            let (query, args) = Self::build_transaction_insert(chunk)?;
            sqlx::query_with(&query, args).execute(&self.pool).await?;
        }
        Ok(())
    }

    fn build_transaction_insert(
        chunk: &[Transaction],
    ) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
        let mut query = String::from(
            "INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata) VALUES ",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        for (i, tx) in chunk.iter().enumerate() {
            let chain_str = chain_to_str(&tx.chain);
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
        query.push_str(" ON CONFLICT (chain, tx_hash, wallet_address) DO NOTHING");
        Ok((query, args))
    }

    pub async fn save_ledger_entries(&self, entries: &[LedgerEntry]) -> anyhow::Result<()> {
        for chunk in entries.chunks(Self::BATCH_SIZE) {
            let (query, args) = Self::build_ledger_insert(chunk)?;
            sqlx::query_with(&query, args).execute(&self.pool).await?;
        }
        Ok(())
    }

    fn build_ledger_insert(
        chunk: &[LedgerEntry],
    ) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
        let mut query = String::from(
            "INSERT INTO ledger_entries (id, transaction_id, user_id, wallet_address, asset_symbol, amount, entry_type, fiat_value) VALUES ",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        for (i, entry) in chunk.iter().enumerate() {
            let entry_type_str = entry_type_to_str(&entry.entry_type);
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
        Ok((query, args))
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
        self.get_transactions_by_wallet_filtered(wallet, limit, offset, None, None)
            .await
    }

    pub async fn get_transactions_by_wallet_filtered(
        &self,
        wallet: &str,
        limit: i64,
        offset: i64,
        from: Option<i64>,
        to: Option<i64>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let mut sql = String::from(
            "SELECT id, user_id, wallet_address, timestamp, tx_hash, chain::text, raw_metadata \
             FROM transactions WHERE wallet_address = $1",
        );
        if from.is_some() {
            sql.push_str(" AND timestamp >= $4");
        }
        if to.is_some() {
            sql.push_str(if from.is_some() {
                " AND timestamp <= $5"
            } else {
                " AND timestamp <= $4"
            });
        }
        sql.push_str(" ORDER BY timestamp ASC LIMIT $2 OFFSET $3");

        let mut query = sqlx::query(&sql).bind(wallet).bind(limit).bind(offset);
        if let Some(f) = from {
            query = query.bind(f);
        }
        if let Some(t) = to {
            query = query.bind(t);
        }
        let rows = query.fetch_all(&self.pool).await?;

        let mut txs = Vec::new();
        for row in rows {
            let chain_str: String = row.try_get("chain")?;
            let chain = str_to_chain(&chain_str)?;

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
        self.get_ledger_entries_by_wallet_filtered(wallet, limit, offset, None, None)
            .await
    }

    pub async fn get_ledger_entries_by_wallet_filtered(
        &self,
        wallet: &str,
        limit: i64,
        offset: i64,
        from: Option<i64>,
        to: Option<i64>,
    ) -> anyhow::Result<Vec<LedgerEntry>> {
        let mut sql = String::from(
            "SELECT l.id, l.transaction_id, l.user_id, l.wallet_address, l.asset_symbol, \
             l.amount, l.entry_type::text, l.fiat_value \
             FROM ledger_entries l",
        );
        let needs_join = from.is_some() || to.is_some();
        if needs_join {
            sql.push_str(" JOIN transactions t ON l.transaction_id = t.id");
        }
        sql.push_str(" WHERE l.wallet_address = $1");
        let mut param_idx = 4_usize;
        if from.is_some() {
            sql.push_str(&format!(" AND t.timestamp >= ${param_idx}"));
            param_idx += 1;
        }
        if to.is_some() {
            sql.push_str(&format!(" AND t.timestamp <= ${param_idx}"));
        }
        sql.push_str(" ORDER BY l.created_at ASC LIMIT $2 OFFSET $3");

        let mut query = sqlx::query(&sql).bind(wallet).bind(limit).bind(offset);
        if let Some(f) = from {
            query = query.bind(f);
        }
        if let Some(t) = to {
            query = query.bind(t);
        }
        let rows = query.fetch_all(&self.pool).await?;

        let mut entries = Vec::new();
        for row in rows {
            let entry_type_str: String = row.try_get("entry_type")?;
            let entry_type = str_to_entry_type(&entry_type_str)?;

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

    pub async fn get_balances(
        &self,
        wallet: &str,
        at: Option<i64>,
    ) -> anyhow::Result<Vec<(String, bigdecimal::BigDecimal)>> {
        let rows = match at {
            Some(ts) => {
                sqlx::query(
                    r#"
                    SELECT le.asset_symbol, SUM(le.amount) as balance
                    FROM ledger_entries le
                    JOIN transactions t ON le.transaction_id = t.id
                    WHERE le.wallet_address = $1 AND t.timestamp <= $2
                    GROUP BY le.asset_symbol
                    HAVING SUM(le.amount) != 0
                    ORDER BY le.asset_symbol
                    "#,
                )
                .bind(wallet)
                .bind(ts)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    r#"
                    SELECT asset_symbol, SUM(amount) as balance
                    FROM ledger_entries
                    WHERE wallet_address = $1
                    GROUP BY asset_symbol
                    HAVING SUM(amount) != 0
                    ORDER BY asset_symbol
                    "#,
                )
                .bind(wallet)
                .fetch_all(&self.pool)
                .await?
            }
        };

        let mut balances = Vec::new();
        for row in rows {
            balances.push((
                row.try_get::<String, _>("asset_symbol")?,
                row.try_get::<bigdecimal::BigDecimal, _>("balance")?,
            ));
        }
        Ok(balances)
    }

    pub async fn get_checkpoint(
        &self,
        chain: &str,
        wallet: &str,
    ) -> anyhow::Result<Option<IndexerCheckpoint>> {
        let row = sqlx::query(
            r#"
            SELECT chain::text, wallet_address, last_signature, last_slot, last_block, last_timestamp
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
                let chain = str_to_chain(&chain_str)?;
                Ok(Some(IndexerCheckpoint {
                    chain,
                    wallet_address: row.try_get("wallet_address")?,
                    last_signature: row.try_get("last_signature")?,
                    last_slot: row.try_get("last_slot")?,
                    last_block: row.try_get("last_block")?,
                    last_timestamp: row.try_get("last_timestamp")?,
                }))
            }
            None => Ok(None),
        }
    }

    pub async fn get_transaction_by_hash(
        &self,
        wallet: &str,
        tx_hash: &str,
    ) -> anyhow::Result<Option<Transaction>> {
        let row = sqlx::query(
            r#"
            SELECT id, user_id, wallet_address, timestamp, tx_hash, chain::text, raw_metadata
            FROM transactions
            WHERE wallet_address = $1 AND tx_hash = $2
            "#,
        )
        .bind(wallet)
        .bind(tx_hash)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            Some(row) => {
                let chain_str: String = row.try_get("chain")?;
                let chain = str_to_chain(&chain_str)?;
                Ok(Some(Transaction {
                    id: row.try_get("id")?,
                    user_id: row.try_get("user_id")?,
                    wallet_address: row.try_get("wallet_address")?,
                    timestamp: row.try_get("timestamp")?,
                    tx_hash: row.try_get("tx_hash")?,
                    chain,
                    raw_metadata: row.try_get("raw_metadata")?,
                }))
            }
            None => Ok(None),
        }
    }

    pub async fn get_wallet_stats(&self, wallet: &str) -> anyhow::Result<WalletStatsRow> {
        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) AS tx_count,
                MIN(timestamp) AS earliest_timestamp,
                MAX(timestamp) AS latest_timestamp,
                COUNT(DISTINCT chain::text) AS chain_count
            FROM transactions
            WHERE wallet_address = $1
            "#,
        )
        .bind(wallet)
        .fetch_one(&self.pool)
        .await?;

        let tx_count: i64 = row.try_get("tx_count")?;
        let earliest_timestamp: Option<i64> = row.try_get("earliest_timestamp")?;
        let latest_timestamp: Option<i64> = row.try_get("latest_timestamp")?;
        let chain_count: i64 = row.try_get("chain_count")?;

        let unique_assets: i64 = sqlx::query(
            r#"
            SELECT COUNT(DISTINCT asset_symbol) AS unique_assets
            FROM ledger_entries
            WHERE wallet_address = $1
            "#,
        )
        .bind(wallet)
        .fetch_one(&self.pool)
        .await?
        .try_get("unique_assets")?;

        let chain_tx_counts = sqlx::query(
            r#"
            SELECT chain::text AS chain, COUNT(*) AS count
            FROM transactions
            WHERE wallet_address = $1
            GROUP BY chain
            ORDER BY chain
            "#,
        )
        .bind(wallet)
        .fetch_all(&self.pool)
        .await?;

        let mut per_chain: Vec<(String, i64)> = Vec::new();
        for r in chain_tx_counts {
            let chain: String = r.try_get("chain")?;
            let count: i64 = r.try_get("count")?;
            per_chain.push((chain, count));
        }

        Ok(WalletStatsRow {
            tx_count,
            earliest_timestamp,
            latest_timestamp,
            chain_count,
            unique_assets,
            per_chain,
        })
    }

    pub async fn save_checkpoint(&self, checkpoint: &IndexerCheckpoint) -> anyhow::Result<()> {
        Self::save_checkpoint_with(&self.pool, checkpoint).await
    }

    pub async fn save_checkpoint_with<'e, E>(
        executor: E,
        checkpoint: &IndexerCheckpoint,
    ) -> anyhow::Result<()>
    where
        E: Executor<'e, Database = Postgres>,
    {
        let chain_str = chain_to_str(&checkpoint.chain);

        sqlx::query(
            r#"
            INSERT INTO indexer_checkpoints (chain, wallet_address, last_signature, last_slot, last_block, last_timestamp, updated_at)
            VALUES ($1::chain_enum, $2, $3, $4, $5, $6, NOW())
            ON CONFLICT (chain, wallet_address)
            DO UPDATE SET
                last_signature = EXCLUDED.last_signature,
                last_slot = EXCLUDED.last_slot,
                last_block = EXCLUDED.last_block,
                last_timestamp = EXCLUDED.last_timestamp,
                updated_at = NOW()
            "#,
        )
        .bind(chain_str)
        .bind(&checkpoint.wallet_address)
        .bind(&checkpoint.last_signature)
        .bind(checkpoint.last_slot)
        .bind(checkpoint.last_block)
        .bind(checkpoint.last_timestamp)
        .execute(executor)
        .await?;

        Ok(())
    }

    pub async fn save_transactions_with<'e, E>(
        executor: E,
        txs: &[Transaction],
    ) -> anyhow::Result<()>
    where
        E: Executor<'e, Database = Postgres>,
    {
        let mut query = String::from(
            "INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata) VALUES ",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        for (i, tx) in txs.iter().enumerate() {
            let chain_str = chain_to_str(&tx.chain);
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
        query.push_str(" ON CONFLICT (chain, tx_hash, wallet_address) DO NOTHING");
        sqlx::query_with(&query, args).execute(executor).await?;
        Ok(())
    }

    pub async fn save_transactions_and_checkpoint(
        &self,
        txs: &[Transaction],
        checkpoint: &IndexerCheckpoint,
    ) -> anyhow::Result<()> {
        let mut db_tx = self.pool.begin().await?;

        for chunk in txs.chunks(Self::BATCH_SIZE) {
            Self::save_transactions_with(&mut *db_tx, chunk).await?;
        }
        Self::save_checkpoint_with(&mut *db_tx, checkpoint).await?;

        db_tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bigdecimal::BigDecimal;
    use std::str::FromStr;
    use uuid::Uuid;

    fn make_tx(chain: Chain) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "test_wallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xdeadbeef".to_string(),
            chain,
            raw_metadata: serde_json::json!({}),
        }
    }

    fn make_ledger_entry(entry_type: EntryType) -> LedgerEntry {
        LedgerEntry {
            id: Uuid::new_v4(),
            transaction_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "test_wallet".to_string(),
            asset_symbol: "SOL".to_string(),
            amount: BigDecimal::from_str("1.5").unwrap(),
            entry_type,
            fiat_value: None,
        }
    }

    #[test]
    fn test_chain_to_str() {
        assert_eq!(chain_to_str(&Chain::Solana), "solana");
        assert_eq!(chain_to_str(&Chain::Hyperliquid), "hyperliquid");
        assert_eq!(chain_to_str(&Chain::Ethereum), "ethereum");
    }

    #[test]
    fn test_str_to_chain() {
        assert!(matches!(str_to_chain("solana").unwrap(), Chain::Solana));
        assert!(matches!(
            str_to_chain("hyperliquid").unwrap(),
            Chain::Hyperliquid
        ));
        assert!(matches!(str_to_chain("ethereum").unwrap(), Chain::Ethereum));
        assert!(str_to_chain("bitcoin").is_err());
    }

    #[test]
    fn test_entry_type_to_str() {
        assert_eq!(entry_type_to_str(&EntryType::Trade), "trade");
        assert_eq!(entry_type_to_str(&EntryType::Fee), "fee");
        assert_eq!(entry_type_to_str(&EntryType::Transfer), "transfer");
        assert_eq!(entry_type_to_str(&EntryType::Staking), "staking");
        assert_eq!(entry_type_to_str(&EntryType::Income), "income");
    }

    #[test]
    fn test_str_to_entry_type() {
        assert!(matches!(
            str_to_entry_type("trade").unwrap(),
            EntryType::Trade
        ));
        assert!(matches!(str_to_entry_type("fee").unwrap(), EntryType::Fee));
        assert!(matches!(
            str_to_entry_type("transfer").unwrap(),
            EntryType::Transfer
        ));
        assert!(matches!(
            str_to_entry_type("staking").unwrap(),
            EntryType::Staking
        ));
        assert!(matches!(
            str_to_entry_type("income").unwrap(),
            EntryType::Income
        ));
        assert!(str_to_entry_type("unknown").is_err());
    }

    #[test]
    fn test_build_transaction_insert_single() {
        let tx = make_tx(Chain::Solana);
        let (query, _args) = Repository::build_transaction_insert(&[tx]).unwrap();

        assert!(query.starts_with("INSERT INTO transactions"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6::chain_enum, $7)"));
        assert!(query.ends_with("ON CONFLICT (chain, tx_hash, wallet_address) DO NOTHING"));
    }

    #[test]
    fn test_build_transaction_insert_multiple() {
        let txs: Vec<Transaction> = (0..3).map(|_| make_tx(Chain::Ethereum)).collect();
        let (query, _args) = Repository::build_transaction_insert(&txs).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6::chain_enum, $7)"));
        assert!(query.contains("($8, $9, $10, $11, $12, $13::chain_enum, $14)"));
        assert!(query.contains("($15, $16, $17, $18, $19, $20::chain_enum, $21)"));
        assert!(query.ends_with("ON CONFLICT (chain, tx_hash, wallet_address) DO NOTHING"));
    }

    #[test]
    fn test_build_ledger_insert_single() {
        let entry = make_ledger_entry(EntryType::Trade);
        let (query, _args) = Repository::build_ledger_insert(&[entry]).unwrap();

        assert!(query.starts_with("INSERT INTO ledger_entries"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7::entry_type_enum, $8)"));
        assert!(query.ends_with("ON CONFLICT (id) DO NOTHING"));
    }

    #[test]
    fn test_build_ledger_insert_multiple() {
        let entries: Vec<LedgerEntry> = vec![
            make_ledger_entry(EntryType::Trade),
            make_ledger_entry(EntryType::Fee),
        ];
        let (query, _args) = Repository::build_ledger_insert(&entries).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7::entry_type_enum, $8)"));
        assert!(query.contains("($9, $10, $11, $12, $13, $14, $15::entry_type_enum, $16)"));
    }

    #[test]
    fn test_batch_size_constant() {
        assert_eq!(Repository::BATCH_SIZE, 500);
    }

    #[test]
    fn test_chain_roundtrip() {
        for chain in [Chain::Solana, Chain::Hyperliquid, Chain::Ethereum] {
            let s = chain_to_str(&chain);
            let recovered = str_to_chain(s).unwrap();
            assert_eq!(chain_to_str(&recovered), s);
        }
    }

    #[test]
    fn test_entry_type_roundtrip() {
        for et in [
            EntryType::Trade,
            EntryType::Fee,
            EntryType::Transfer,
            EntryType::Staking,
            EntryType::Income,
        ] {
            let s = entry_type_to_str(&et);
            let recovered = str_to_entry_type(s).unwrap();
            assert_eq!(entry_type_to_str(&recovered), s);
        }
    }

    #[test]
    fn test_finality_buffer_values() {
        assert_eq!(finality_buffer("solana"), 32);
        assert_eq!(finality_buffer("ethereum"), 15);
        assert_eq!(finality_buffer("hyperliquid"), 0);
        assert_eq!(finality_buffer("unknown"), 0);
    }

    #[test]
    fn test_checkpoint_solana_applies_finality_buffer() {
        let slot: i64 = 200_000_000;
        let txs = vec![Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "sig1".to_string(),
            chain: Chain::Solana,
            raw_metadata: serde_json::json!({ "slot": slot }),
        }];
        let cp = build_checkpoint("solana", "wallet", &txs).unwrap();
        assert_eq!(cp.last_slot, Some(slot - 32));
        assert_eq!(cp.last_block, None);
    }

    #[test]
    fn test_checkpoint_ethereum_applies_finality_buffer() {
        let block: i64 = 19_000_000;
        let txs = vec![Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xabc".to_string(),
            chain: Chain::Ethereum,
            raw_metadata: serde_json::json!({ "block_number": block }),
        }];
        let cp = build_checkpoint("ethereum", "wallet", &txs).unwrap();
        assert_eq!(cp.last_block, Some(block - 15));
        assert_eq!(cp.last_slot, None);
    }

    #[test]
    fn test_checkpoint_hyperliquid_no_finality_buffer() {
        let txs = vec![Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "h1".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: serde_json::json!({}),
        }];
        let cp = build_checkpoint("hyperliquid", "wallet", &txs).unwrap();
        assert_eq!(cp.last_slot, None);
        assert_eq!(cp.last_block, None);
        assert_eq!(cp.last_timestamp, Some(1700000000));
    }

    #[test]
    fn test_checkpoint_solana_slot_saturating_sub() {
        let txs = vec![Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "sig_early".to_string(),
            chain: Chain::Solana,
            raw_metadata: serde_json::json!({ "slot": 10 }),
        }];
        let cp = build_checkpoint("solana", "wallet", &txs).unwrap();
        // slot 10 - buffer 32 saturates to 0
        assert_eq!(cp.last_slot, Some(0));
    }

    #[test]
    fn test_build_transaction_insert_multi_wallet_same_hash() {
        // Two wallets share the same on-chain transaction. With the updated
        // UNIQUE(chain, tx_hash, wallet_address) constraint, both rows can
        // coexist in the same batch insert.
        let shared_hash = "0xshared";
        let tx_a = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet_a".to_string(),
            timestamp: 1700000000,
            tx_hash: shared_hash.to_string(),
            chain: Chain::Ethereum,
            raw_metadata: serde_json::json!({}),
        };
        let tx_b = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet_b".to_string(),
            timestamp: 1700000000,
            tx_hash: shared_hash.to_string(),
            chain: Chain::Ethereum,
            raw_metadata: serde_json::json!({}),
        };

        let (query, _args) = Repository::build_transaction_insert(&[tx_a, tx_b]).unwrap();

        // Both rows should be present in the VALUES clause
        assert!(query.contains("($1, $2, $3, $4, $5, $6::chain_enum, $7)"));
        assert!(query.contains("($8, $9, $10, $11, $12, $13::chain_enum, $14)"));
        // The constraint includes wallet_address so both can succeed
        assert!(query.ends_with("ON CONFLICT (chain, tx_hash, wallet_address) DO NOTHING"));
    }
}
