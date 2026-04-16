//! Extract Flipster session cookies from a running Chrome instance via CDP WebSocket.

use std::collections::HashMap;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::error::FlipsterError;

/// Connect to Chrome DevTools Protocol and extract all cookies for flipster domain.
/// Returns a map of cookie_name -> cookie_value.
pub async fn extract_cookies(cdp_ws_url: &str) -> Result<HashMap<String, String>, FlipsterError> {
    let (mut ws, _) = connect_async(cdp_ws_url)
        .await
        .map_err(|e| FlipsterError::Api {
            status: 0,
            message: format!("CDP WebSocket connect failed: {e}"),
        })?;

    let req = serde_json::json!({
        "id": 1,
        "method": "Storage.getCookies"
    });

    ws.send(Message::Text(req.to_string()))
        .await
        .map_err(|e| FlipsterError::Api {
            status: 0,
            message: format!("CDP send failed: {e}"),
        })?;

    let msg = ws.next().await.ok_or(FlipsterError::Api {
        status: 0,
        message: "CDP: no response".into(),
    })?.map_err(|e| FlipsterError::Api {
        status: 0,
        message: format!("CDP recv failed: {e}"),
    })?;

    let text = msg.into_text().map_err(|e| FlipsterError::Api {
        status: 0,
        message: format!("CDP message not text: {e}"),
    })?;

    let resp: serde_json::Value = serde_json::from_str(&text).map_err(|e| FlipsterError::Api {
        status: 0,
        message: format!("CDP JSON parse failed: {e}"),
    })?;

    let cookies = resp["result"]["cookies"]
        .as_array()
        .ok_or(FlipsterError::Api {
            status: 0,
            message: "CDP: no cookies in response".into(),
        })?;

    let mut map = HashMap::new();
    for c in cookies {
        let domain = c["domain"].as_str().unwrap_or("");
        if domain.contains("flipster") {
            if let (Some(name), Some(value)) = (c["name"].as_str(), c["value"].as_str()) {
                map.insert(name.to_string(), value.to_string());
            }
        }
    }

    if !map.contains_key("session_id_bolts") {
        return Err(FlipsterError::Auth);
    }

    let _ = ws.close(None).await;
    Ok(map)
}
