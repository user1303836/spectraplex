use crate::repo::Repository;
use crate::v2_repo::{
    build_ingestion_run_insert, build_raw_transaction_upsert_returning, build_target_match_insert,
};
use spectraplex_core::v2::{IndexTarget, IngestionRun, RawTransaction, TargetKind, TargetMatch};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

impl Repository {
    pub(crate) async fn upsert_raw_batch_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        records: &[RawTransaction],
    ) -> anyhow::Result<Vec<Uuid>> {
        let (query, args) = build_raw_transaction_upsert_returning(records)?;
        let rows = sqlx::query_with(&query, args).fetch_all(&mut **tx).await?;
        Self::link_run_inputs(tx, records).await?;
        rows.iter().map(|r| Ok(r.try_get("id")?)).collect()
    }

    pub(crate) async fn link_run_inputs(
        tx: &mut Transaction<'_, Postgres>,
        records: &[RawTransaction],
    ) -> anyhow::Result<()> {
        let with_run: Vec<_> = records
            .iter()
            .filter(|r| r.ingestion_run_id.is_some())
            .collect();
        if with_run.is_empty() {
            return Ok(());
        }
        let networks: Vec<_> = with_run.iter().map(|r| r.network.as_str()).collect();
        let hashes: Vec<_> = with_run.iter().map(|r| r.tx_hash.as_str()).collect();
        let runs: Vec<_> = with_run.iter().filter_map(|r| r.ingestion_run_id).collect();
        sqlx::query("INSERT INTO ingestion_run_transactions (ingestion_run_id, raw_transaction_id)
            SELECT i.run, r.id FROM unnest($1::text[], $2::text[], $3::uuid[]) AS i(network, hash, run)
            JOIN raw_transactions r ON r.network = i.network AND r.tx_hash = i.hash
            ON CONFLICT DO NOTHING")
            .bind(networks).bind(hashes).bind(runs).execute(&mut **tx).await?;
        Ok(())
    }

    /// Raw rows, run membership, target lineage and queued ETL commit together.
    pub async fn import_raw_records(
        &self,
        target: &IndexTarget,
        run: &IngestionRun,
        records: &[RawTransaction],
    ) -> anyhow::Result<Option<Uuid>> {
        let mut tx = self.pool().begin().await?;
        let (query, args) = build_ingestion_run_insert(run)?;
        sqlx::query_with(&query, args).execute(&mut *tx).await?;
        for chunk in records.chunks(500) {
            let ids = Self::upsert_raw_batch_in_tx(&mut tx, chunk).await?;
            let matches: Vec<_> = ids
                .iter()
                .map(|id| TargetMatch {
                    id: Uuid::new_v4(),
                    target_id: target.id,
                    raw_transaction_id: *id,
                    match_reason: Some("import".to_string()),
                    matched_at: chrono::Utc::now(),
                })
                .collect();
            let (query, args) = build_target_match_insert(&matches)?;
            sqlx::query_with(&query, args).execute(&mut *tx).await?;
        }
        let materialization_id = if target.kind == TargetKind::Wallet {
            let id = Uuid::new_v4();
            let scope = serde_json::json!({"wallet": target.address, "network": target.network,
                "target_id": target.id, "ingestion_run_id": run.id});
            sqlx::query("INSERT INTO materialization_runs (id, dataset_name, scope) VALUES ($1, 'normalize', $2)")
                .bind(id).bind(scope).execute(&mut *tx).await?;
            Some(id)
        } else {
            None
        };
        tx.commit().await?;
        Ok(materialization_id)
    }

    /// Lease-fenced commit: inputs, lineage, checkpoint and downstream work are
    /// durable together. A crash cannot advance a cursor past unmaterialized data.
    pub async fn commit_connector_batch(
        &self,
        target: &IndexTarget,
        run_id: Uuid,
        records: &[RawTransaction],
        checkpoint: Option<&spectraplex_core::v2::Checkpoint>,
        lease: (Uuid, &str),
    ) -> anyhow::Result<()> {
        let mut tx = self.pool().begin().await?;
        let valid = sqlx::query_scalar::<_, Uuid>("SELECT j.id FROM ingestion_jobs j WHERE j.id = $1 AND j.status IN ('claimed', 'running') AND EXISTS (SELECT 1 FROM ingestion_job_attempts a WHERE a.job_id = j.id AND a.worker_id = $2 AND a.finished_at IS NULL) FOR UPDATE OF j")
            .bind(lease.0).bind(lease.1).fetch_optional(&mut *tx).await?;
        anyhow::ensure!(valid.is_some(), "Ingestion lease lost before commit");
        for chunk in records.chunks(500) {
            let ids = Self::upsert_raw_batch_in_tx(&mut tx, chunk).await?;
            let matches: Vec<_> = ids
                .into_iter()
                .map(|id| TargetMatch {
                    id: Uuid::new_v4(),
                    target_id: target.id,
                    raw_transaction_id: id,
                    match_reason: Some("provider".into()),
                    matched_at: chrono::Utc::now(),
                })
                .collect();
            let (query, args) = build_target_match_insert(&matches)?;
            sqlx::query_with(&query, args).execute(&mut *tx).await?;
        }
        if let Some(checkpoint) = checkpoint {
            let (query, args) = crate::v2_repo::build_checkpoint_upsert(checkpoint)?;
            sqlx::query_with(&query, args).execute(&mut *tx).await?;
        }
        if target.kind == TargetKind::Wallet && !records.is_empty() {
            let scope = serde_json::json!({"wallet": target.address, "network": target.network, "target_id": target.id, "ingestion_run_id": run_id});
            sqlx::query("INSERT INTO materialization_runs (id, dataset_name, scope) VALUES ($1, 'normalize', $2)")
                .bind(Uuid::new_v4()).bind(scope).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE ingestion_runs SET status = 'completed', finished_at = NOW(), records_written = $2 WHERE id = $1")
            .bind(run_id).bind(records.len() as i64).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_workbench_jobs(
        &self,
        owner: Option<Uuid>,
        target: Option<Uuid>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "WITH jobs AS (
            SELECT j.id, 'ingest' AS kind, j.status AS state, j.network, j.target_id,
                   NULL::text AS dataset, NULL::text AS format, j.error_message AS message,
                   j.created_at, j.updated_at, NULL::bigint AS record_count,
                   NULL::uuid AS ingestion_run_id, t.address AS wallet, j.mode::text AS mode
            FROM ingestion_jobs j JOIN index_targets t ON t.id = j.target_id
            WHERE ($1::uuid IS NULL OR t.owner_id = $1)
            UNION ALL
            SELECT m.id, 'materialize', m.status, r.network, r.target_id, m.dataset_name, NULL,
                   m.error_message, m.created_at, m.updated_at, m.output_record_count, r.id, t.address, NULL::text
            FROM materialization_runs m
            JOIN ingestion_runs r ON r.id::text = m.scope->>'ingestion_run_id'
            JOIN index_targets t ON t.id = r.target_id
            WHERE ($1::uuid IS NULL OR t.owner_id = $1)
            UNION ALL
            SELECT e.id, 'export', e.status, e.filters->>'network', t.id, e.dataset, e.format,
                   e.error_message, e.created_at, e.updated_at, e.record_count::bigint, NULL::uuid, t.address, NULL::text
            FROM export_jobs e LEFT JOIN index_targets t ON t.id::text = e.filters->>'target_id'
            WHERE ($1::uuid IS NULL OR (e.owner_id = $1 AND t.owner_id = $1))
        ) SELECT to_jsonb(jobs) AS job FROM jobs WHERE ($2::uuid IS NULL OR target_id = $2)
        ORDER BY created_at DESC, id DESC LIMIT $3 OFFSET $4",
        )
        .bind(owner)
        .bind(target)
        .bind(limit.clamp(1, 1000))
        .bind(offset.max(0))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(|r| Ok(r.try_get("job")?)).collect()
    }
}
