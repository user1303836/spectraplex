use super::*;
use spectraplex_core::v2::{IngestionJobMode, IngestionRun, RawTransaction};

pub(super) async fn session(
    State(state): State<Arc<AppState>>,
    Extension(owner): Extension<AuthenticatedOwner>,
) -> Json<serde_json::Value> {
    let networks: Vec<_> = state
        .provider_registry
        .enabled_networks()
        .map(|id| id.to_string())
        .collect();
    Json(
        serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "admin": owner.0.is_none(),
        "owner_id": owner.0, "configured_networks": networks}),
    )
}

#[derive(Deserialize)]
pub(super) struct JobListParams {
    target_id: Option<Uuid>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub(super) async fn jobs(
    State(state): State<Arc<AppState>>,
    Extension(owner): Extension<AuthenticatedOwner>,
    Query(params): Query<JobListParams>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    if let Some(target) = params.target_id {
        check_target_owner(&state.repo, target, &owner).await?;
    }
    Ok(Json(
        state
            .repo
            .list_workbench_jobs(
                owner.0,
                params.target_id,
                clamp_limit(params.limit),
                clamp_offset(params.offset),
            )
            .await
            .map_err(AppError::internal)?,
    ))
}

#[derive(Deserialize)]
pub(super) struct ImportRequest {
    records: Vec<ImportRecord>,
}

#[derive(Deserialize)]
struct ImportRecord {
    tx_hash: String,
    timestamp: i64,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    block_number: Option<i64>,
    raw_metadata: serde_json::Value,
}

fn validate_records(records: &[ImportRecord], target: &IndexTarget) -> Result<(), AppError> {
    if records.is_empty() || records.len() > 500 {
        return Err(AppError::bad_request(
            "Import requires 1–500 records (maximum request size 1 MiB)",
        ));
    }
    let mut hashes = HashSet::new();
    for (index, record) in records.iter().enumerate() {
        let invalid =
            |message: &str| AppError::bad_request(format!("Record {}: {message}", index + 1));
        if record.tx_hash.is_empty()
            || record.tx_hash.len() > 256
            || record.tx_hash.chars().any(char::is_whitespace)
        {
            return Err(invalid(
                "tx_hash must contain 1–256 non-whitespace characters",
            ));
        }
        if !hashes.insert(&record.tx_hash) {
            return Err(invalid("duplicate tx_hash in import"));
        }
        if record.timestamp < 0
            || record.timestamp > 32_503_680_000
            || record.block_number.is_some_and(|b| b < 0)
        {
            return Err(invalid("invalid timestamp (Unix seconds) or block_number"));
        }
        if record
            .network
            .as_ref()
            .is_some_and(|n| n != &target.network)
        {
            return Err(invalid("network does not match target"));
        }
        spectraplex_adapters::validate_import_payload(target.chain_family, &record.raw_metadata)
            .map_err(|e| invalid(&e.to_string()))?;
    }
    Ok(())
}

async fn import(
    state: &AppState,
    target: &IndexTarget,
    records: Vec<ImportRecord>,
    source: &str,
) -> Result<Json<serde_json::Value>, AppError> {
    if let Some(wallet) = target.address.as_deref() {
        check_wallet_allowed(wallet, &state.allowed_wallets)?;
    }
    validate_records(&records, target)?;
    let now = chrono::Utc::now();
    let run = IngestionRun {
        id: Uuid::new_v4(),
        target_id: Some(target.id),
        network: target.network.clone(),
        source: source.into(),
        mode: IngestionJobMode::Backfill,
        status: IngestionJobStatus::Completed,
        started_at: now,
        finished_at: Some(now),
        records_written: records.len() as i64,
        error_message: None,
        cursor_state: Some(serde_json::json!({"coverage": "imported records only"})),
    };
    let raw: Vec<_> = records
        .into_iter()
        .map(|r| RawTransaction {
            id: Uuid::new_v4(),
            network: target.network.clone(),
            tx_hash: r.tx_hash,
            timestamp: r.timestamp,
            block_number: r.block_number,
            raw_metadata: r.raw_metadata,
            source: source.into(),
            ingestion_run_id: Some(run.id),
            ingested_at: now,
        })
        .collect();
    let job = state
        .repo
        .import_raw_records(target, &run, &raw)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(
        serde_json::json!({"target_id": target.id, "ingestion_run_id": run.id,
        "materialization_job_id": job, "record_count": raw.len()}),
    ))
}

pub(super) async fn import_records(
    State(state): State<Arc<AppState>>,
    Extension(owner): Extension<AuthenticatedOwner>,
    Path(id): Path<Uuid>,
    Json(request): Json<ImportRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Unverified payloads must not let a tenant poison canonical chain data for others.
    if owner.0.is_some() {
        return Err(AppError::forbidden(
            "File import requires an admin key; tenants can use sample data or provider ingestion",
        ));
    }
    let target = state
        .repo
        .get_index_target(id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found("Target not found"))?;
    if target.network.ends_with("-demo") {
        return Err(AppError::bad_request(
            "Sample networks accept only bundled sample data",
        ));
    }
    import(&state, &target, request.records, "trusted-import").await
}

pub(super) async fn demo(
    State(state): State<Arc<AppState>>,
    Extension(owner): Extension<AuthenticatedOwner>,
    Path(chain): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    #[derive(Deserialize)]
    struct Sample {
        network: String,
        wallet: String,
        label: String,
        records: Vec<ImportRecord>,
    }
    let mut samples: HashMap<String, Sample> =
        serde_json::from_str(include_str!("../static/samples.json")).map_err(AppError::internal)?;
    let sample = samples
        .remove(&chain)
        .ok_or_else(|| AppError::bad_request("Choose solana, ethereum or hyperliquid"))?;
    check_wallet_allowed(&sample.wallet, &state.allowed_wallets)?;
    let existing = state
        .repo
        .get_index_target_by_address(TargetKind::Wallet, &sample.network, &sample.wallet, owner.0)
        .await
        .map_err(AppError::internal)?;
    let target = if let Some(target) = existing {
        target
    } else {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: sample.network.clone(),
            chain_family: DatasetRegistry::chain_family_for_network(&sample.network)
                .expect("bundled sample family"),
            address: Some(sample.wallet),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: Some(sample.label),
            owner_id: owner.0,
            created_at: now,
            updated_at: now,
        };
        match state.repo.create_index_target(&target).await {
            Ok(target) => target,
            Err(e) => state
                .repo
                .get_index_target_by_address(
                    TargetKind::Wallet,
                    &target.network,
                    target.address.as_deref().unwrap(),
                    owner.0,
                )
                .await
                .map_err(AppError::internal)?
                .ok_or_else(|| AppError::internal(e))?,
        }
    };
    import(&state, &target, sample.records, "sample-data").await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundled_samples_are_valid() {
        let samples: serde_json::Value =
            serde_json::from_str(include_str!("../static/samples.json")).unwrap();
        for sample in samples.as_object().unwrap().values() {
            let network = sample["network"].as_str().unwrap();
            let now = chrono::Utc::now();
            let target = IndexTarget {
                id: Uuid::new_v4(),
                kind: TargetKind::Wallet,
                network: network.into(),
                chain_family: DatasetRegistry::chain_family_for_network(network).unwrap(),
                address: Some(sample["wallet"].as_str().unwrap().into()),
                filter_spec: None,
                mode: TargetMode::Backfill,
                label: None,
                owner_id: None,
                created_at: now,
                updated_at: now,
            };
            let records: Vec<ImportRecord> =
                serde_json::from_value(sample["records"].clone()).unwrap();
            validate_records(&records, &target).unwrap();
            assert!(validate_records(&[], &target).is_err());
        }
    }
}
