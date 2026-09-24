use anyhow::Context;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_client::GetConfirmedSignaturesForAddress2Config,
    rpc_config::RpcTransactionConfig, rpc_response::RpcConfirmedTransactionStatusWithSignature,
};
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::UiTransactionEncoding;
use spectraplex_core::v2::{Checkpoint, IngestionBatch, RawTransaction};
use std::{str::FromStr, sync::Arc};
use uuid::Uuid;

async fn signatures<F, Fut>(
    limit: usize,
    mut before: Option<Signature>,
    until: Option<Signature>,
    reject_overflow: bool,
    mut fetch: F,
) -> anyhow::Result<(Vec<RpcConfirmedTransactionStatusWithSignature>, bool)>
where
    F: FnMut(GetConfirmedSignaturesForAddress2Config) -> Fut + Send,
    Fut: std::future::Future<
            Output = anyhow::Result<Vec<RpcConfirmedTransactionStatusWithSignature>>,
        > + Send,
{
    anyhow::ensure!(
        limit > 0 && limit <= 100_000,
        "Solana limit must be 1–100000"
    );
    let mut result = Vec::new();
    loop {
        let page_limit = (limit - result.len()).min(1000);
        let page = fetch(GetConfirmedSignaturesForAddress2Config {
            before,
            until,
            limit: Some(page_limit),
            ..Default::default()
        })
        .await?;
        anyhow::ensure!(
            page.len() <= page_limit,
            "Solana provider exceeded requested page size"
        );
        let count = page.len();
        if let Some(last) = page.last() {
            let next = Signature::from_str(&last.signature)?;
            anyhow::ensure!(
                Some(next) != before,
                "Solana provider repeated pagination cursor"
            );
            before = Some(next);
        }
        result.extend(page);
        if count < page_limit {
            return Ok((result, true));
        }
        if result.len() == limit {
            if reject_overflow {
                let extra = fetch(GetConfirmedSignaturesForAddress2Config {
                    before,
                    until,
                    limit: Some(1),
                    ..Default::default()
                })
                .await?;
                anyhow::ensure!(extra.is_empty(), "More new Solana activity than ingest_limit; increase ingest_limit and retry (checkpoint was not advanced)");
            }
            return Ok((result, false));
        }
    }
}

pub(crate) async fn fetch(
    client: Arc<RpcClient>,
    wallet: String,
    network: String,
    cursor: Option<serde_json::Value>,
    limit: usize,
    target_id: Uuid,
) -> anyhow::Result<IngestionBatch> {
    let pubkey = Pubkey::from_str(&wallet)?;
    let cursor = cursor.unwrap_or_default();
    let incremental = cursor["incremental"].as_bool().unwrap_or(false);
    let parse = |name: &str| -> anyhow::Result<Option<Signature>> {
        cursor[name]
            .as_str()
            .map(Signature::from_str)
            .transpose()
            .map_err(Into::into)
    };
    let head = parse("last_signature")?;
    let before = if incremental {
        None
    } else {
        parse("before_signature")?
    };
    let until = if incremental { head } else { None };
    let (signatures, exhausted) = signatures(
        limit,
        before,
        until,
        incremental && head.is_some(),
        |config| {
            let client = client.clone();
            async move {
                Ok(client
                    .get_signatures_for_address_with_config(&pubkey, config)
                    .await?)
            }
        },
    )
    .await?;
    let mut records = Vec::with_capacity(signatures.len());
    for info in &signatures {
        let signature = Signature::from_str(&info.signature)?;
        let tx = client
            .get_transaction_with_config(
                &signature,
                RpcTransactionConfig {
                    encoding: Some(UiTransactionEncoding::Json),
                    max_supported_transaction_version: Some(0),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| {
                format!(
                    "Failed to fetch {}; checkpoint not advanced",
                    info.signature
                )
            })?;
        let timestamp = tx
            .block_time
            .ok_or_else(|| anyhow::anyhow!("Missing blockTime for {}", info.signature))?;
        records.push(RawTransaction {
            id: Uuid::new_v4(),
            network: network.clone(),
            tx_hash: info.signature.clone(),
            timestamp,
            block_number: Some(i64::try_from(tx.slot)?),
            raw_metadata: serde_json::to_value(tx)?,
            source: "solana-rpc-wallet-backfill".into(),
            ingestion_run_id: None,
            ingested_at: chrono::Utc::now(),
        });
    }
    let new_head = if incremental || head.is_none() {
        signatures
            .first()
            .map(|s| s.signature.clone())
            .or_else(|| head.map(|h| h.to_string()))
    } else {
        head.map(|h| h.to_string())
    };
    let new_before = if incremental && cursor["before_signature"].is_string() {
        cursor["before_signature"].as_str().map(str::to_owned)
    } else {
        signatures
            .last()
            .map(|s| s.signature.clone())
            .or_else(|| before.map(|s| s.to_string()))
    };
    let checkpoint = Checkpoint {
        id: Uuid::new_v4(),
        target_id,
        network,
        source: "rpc".into(),
        updated_at: chrono::Utc::now(),
        cursor: serde_json::json!({"last_signature": new_head, "before_signature": new_before,
            "last_slot": signatures.first().map(|s| s.slot), "history_exhausted": if incremental { cursor["history_exhausted"].as_bool().unwrap_or(false) } else { exhausted }}),
    };
    Ok(IngestionBatch {
        records,
        checkpoint: Some(checkpoint),
        run_metadata: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(index: usize) -> RpcConfirmedTransactionStatusWithSignature {
        let mut bytes = [0u8; 64];
        bytes[..8].copy_from_slice(&index.to_le_bytes());
        serde_json::from_value(serde_json::json!({"signature": bs58::encode(bytes).into_string(), "slot": index, "err": null, "memo": null, "blockTime": 1700000000, "confirmationStatus": "finalized"})).unwrap()
    }
    #[tokio::test]
    async fn paginates_2500_signatures_and_preserves_boundaries() {
        let mut offset = 0;
        let mut calls = 0;
        let (rows, exhausted) = signatures(3000, None, None, false, |config| {
            calls += 1;
            assert_eq!(config.limit, Some(1000));
            if offset > 0 {
                assert_eq!(
                    config.before.unwrap().to_string(),
                    info(offset - 1).signature
                );
            }
            let end = (offset + 1000).min(2500);
            let page = (offset..end).map(info).collect();
            offset = end;
            std::future::ready(Ok(page))
        })
        .await
        .unwrap();
        assert_eq!(calls, 3);
        assert_eq!(rows.len(), 2500);
        assert!(exhausted);
    }
    #[tokio::test]
    async fn incremental_overflow_fails_instead_of_skipping_history() {
        let result = signatures(2, None, Some(Signature::default()), true, |config| {
            std::future::ready(Ok((1..=config.limit.unwrap()).map(info).collect()))
        })
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("increase ingest_limit"));
    }
    #[tokio::test]
    async fn rejects_a_provider_that_repeats_a_page() {
        let result = signatures(2000, None, None, false, |_| {
            std::future::ready(Ok((1..=1000).map(info).collect()))
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("repeated"));
    }
}
