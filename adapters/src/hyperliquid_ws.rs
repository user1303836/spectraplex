use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const HL_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";

#[derive(Serialize)]
struct WsSubscription<'a> {
    method: &'static str,
    subscription: WsSubType<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
#[allow(clippy::enum_variant_names)]
enum WsSubType<'a> {
    #[serde(rename = "userFills")]
    UserFills { user: &'a str },
    #[serde(rename = "userFundings")]
    UserFundings { user: &'a str },
    #[serde(rename = "userNonFundingLedgerUpdates")]
    UserLedgerUpdates { user: &'a str },
}

#[derive(Debug, Clone, Deserialize)]
pub struct WsMessage {
    pub channel: Option<String>,
    pub data: Option<serde_json::Value>,
}

pub struct HyperliquidWsClient {
    url: String,
}

impl Default for HyperliquidWsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperliquidWsClient {
    pub fn new() -> Self {
        Self {
            url: HL_WS_URL.to_string(),
        }
    }

    pub fn with_url(url: &str) -> Self {
        Self {
            url: url.to_string(),
        }
    }

    /// Subscribe to a user's fills, funding, and ledger updates.
    /// Calls `on_message` for each incoming WebSocket message.
    /// Returns when the connection is closed or an error occurs.
    pub async fn subscribe_user<F>(
        &self,
        wallet: &str,
        mut on_message: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(WsMessage) + Send,
    {
        let (ws_stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Subscribe to all user channels
        let subscriptions = [
            WsSubscription {
                method: "subscribe",
                subscription: WsSubType::UserFills { user: wallet },
            },
            WsSubscription {
                method: "subscribe",
                subscription: WsSubType::UserFundings { user: wallet },
            },
            WsSubscription {
                method: "subscribe",
                subscription: WsSubType::UserLedgerUpdates { user: wallet },
            },
        ];

        for sub in &subscriptions {
            let msg = serde_json::to_string(sub)?;
            write.send(Message::Text(msg)).await?;
        }

        // Read messages
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(ws_msg) = serde_json::from_str::<WsMessage>(&text) {
                        on_message(ws_msg);
                    }
                }
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await?;
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    eprintln!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }
}
