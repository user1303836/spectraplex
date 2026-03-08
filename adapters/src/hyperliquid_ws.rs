use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, warn};

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
    #[serde(rename = "trades")]
    Trades { coin: &'a str },
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
    pub async fn subscribe_user<F>(&self, wallet: &str, mut on_message: F) -> anyhow::Result<()>
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
                Ok(Message::Text(text)) => match serde_json::from_str::<WsMessage>(&text) {
                    Ok(ws_msg) => on_message(ws_msg),
                    Err(e) => warn!(error = %e, "Failed to parse WebSocket message"),
                },
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await?;
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    error!(error = %e, "WebSocket error");
                    break;
                }
                _ => {}
            }
        }

        let _ = write.send(Message::Close(None)).await;

        Ok(())
    }

    /// Subscribe to market trades for a specific coin.
    ///
    /// Uses the Hyperliquid WS `trades` channel which emits real-time
    /// trade events for the given coin. Calls `on_message` for each
    /// incoming message. Returns when the connection is closed or an
    /// error occurs.
    pub async fn subscribe_trades<F>(&self, coin: &str, mut on_message: F) -> anyhow::Result<()>
    where
        F: FnMut(WsMessage) + Send,
    {
        let (ws_stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = ws_stream.split();

        let sub = WsSubscription {
            method: "subscribe",
            subscription: WsSubType::Trades { coin },
        };
        let msg = serde_json::to_string(&sub)?;
        write.send(Message::Text(msg)).await?;

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => match serde_json::from_str::<WsMessage>(&text) {
                    Ok(ws_msg) => on_message(ws_msg),
                    Err(e) => warn!(error = %e, "Failed to parse WebSocket trade message"),
                },
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await?;
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    error!(error = %e, "WebSocket error in trades stream");
                    break;
                }
                _ => {}
            }
        }

        let _ = write.send(Message::Close(None)).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trades_subscription_serialization() {
        let sub = WsSubscription {
            method: "subscribe",
            subscription: WsSubType::Trades { coin: "SOL" },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["method"], "subscribe");
        assert_eq!(parsed["subscription"]["type"], "trades");
        assert_eq!(parsed["subscription"]["coin"], "SOL");
    }

    #[test]
    fn test_user_fills_subscription_serialization() {
        let sub = WsSubscription {
            method: "subscribe",
            subscription: WsSubType::UserFills { user: "0xabc" },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["method"], "subscribe");
        assert_eq!(parsed["subscription"]["type"], "userFills");
        assert_eq!(parsed["subscription"]["user"], "0xabc");
    }

    #[test]
    fn test_user_fundings_subscription_serialization() {
        let sub = WsSubscription {
            method: "subscribe",
            subscription: WsSubType::UserFundings { user: "0xdef" },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["subscription"]["type"], "userFundings");
    }

    #[test]
    fn test_user_ledger_updates_subscription_serialization() {
        let sub = WsSubscription {
            method: "subscribe",
            subscription: WsSubType::UserLedgerUpdates { user: "0xghi" },
        };
        let json = serde_json::to_string(&sub).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["subscription"]["type"],
            "userNonFundingLedgerUpdates"
        );
    }

    #[test]
    fn test_ws_message_deserialization() {
        let json = r#"{"channel":"trades","data":[{"coin":"ETH","side":"B","px":"3500.0","sz":"1.0","time":1700000000000}]}"#;
        let msg: WsMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.channel.as_deref(), Some("trades"));
        assert!(msg.data.is_some());
    }

    #[test]
    fn test_ws_message_deserialization_subscription_response() {
        let json = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"SOL"}}}"#;
        let msg: WsMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.channel.as_deref(), Some("subscriptionResponse"));
    }
}
