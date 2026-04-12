use crate::config::Config;
use crate::error::Result;
use crate::flipster::auth;
use futures_util::{SinkExt, StreamExt};
use http::header::HeaderValue;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{client_async_tls, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct FlipsterWsClient {
    ws: WsStream,
}

impl FlipsterWsClient {
    /// Connect to the Flipster WebSocket endpoint with HMAC authentication.
    pub async fn connect(config: &Config) -> Result<Self> {
        let expires = auth::make_expires(60);
        let signature = auth::sign(&config.api_secret, "GET", "/api/v1/stream", expires, None);

        let mut request = config.ws_url.as_str().into_client_request()?;
        let headers = request.headers_mut();
        headers.insert(
            "api-key",
            HeaderValue::from_str(&config.api_key).expect("API key must be valid ASCII"),
        );
        headers.insert(
            "api-expires",
            HeaderValue::from_str(&expires.to_string()).unwrap(),
        );
        headers.insert(
            "api-signature",
            HeaderValue::from_str(&signature).unwrap(),
        );

        // Manual TCP connection to set TCP_NODELAY before TLS handshake
        let uri: http::Uri = config.ws_url.parse().map_err(|e: http::uri::InvalidUri| {
            crate::error::Error::Other(format!("invalid ws_url: {e}"))
        })?;
        let host = uri
            .host()
            .ok_or_else(|| crate::error::Error::Other("ws_url missing host".into()))?;
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });

        info!("connecting to {} (TCP_NODELAY=true)", config.ws_url);
        let tcp = TcpStream::connect(format!("{host}:{port}")).await.map_err(|e| {
            crate::error::Error::Other(format!("TCP connect failed: {e}"))
        })?;
        tcp.set_nodelay(true)?;

        let (ws, _resp) = client_async_tls(request, tcp).await?;
        info!("websocket connected (TCP_NODELAY=true)");

        Ok(Self { ws })
    }

    /// Subscribe to one or more topics (e.g. `["ticker.BTCUSDT.PERP"]`).
    /// Topics are sent in batches to avoid server-side disconnection.
    pub async fn subscribe(&mut self, topics: &[String]) -> Result<()> {
        const BATCH_SIZE: usize = 50;
        for (i, chunk) in topics.chunks(BATCH_SIZE).enumerate() {
            let payload = serde_json::json!({
                "op": "subscribe",
                "args": chunk,
            });
            debug!("subscribe batch {}: {} topics", i + 1, chunk.len());
            self.ws.send(Message::Text(payload.to_string())).await?;
            if i > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
        Ok(())
    }

    /// Unsubscribe from topics.
    pub async fn unsubscribe(&mut self, topics: &[String]) -> Result<()> {
        let payload = serde_json::json!({
            "op": "unsubscribe",
            "args": topics,
        });
        self.ws.send(Message::Text(payload.to_string())).await?;
        Ok(())
    }

    /// Read the next WebSocket message. Returns `None` when the stream ends.
    pub async fn next_message(&mut self) -> Option<std::result::Result<Message, crate::error::Error>> {
        self.ws.next().await.map(|r| r.map_err(Into::into))
    }
}
