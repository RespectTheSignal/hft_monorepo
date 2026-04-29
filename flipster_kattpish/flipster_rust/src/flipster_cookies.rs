// Extract Flipster cookies via Chrome DevTools Protocol (localhost:{cdp_port}).

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub async fn fetch_flipster_cookies(cdp_port: u16) -> Result<Vec<(String, String)>> {
    let ver_url = format!("http://localhost:{}/json/version", cdp_port);
    let ver: Value = reqwest::get(&ver_url)
        .await
        .with_context(|| format!("GET {}", ver_url))?
        .json()
        .await
        .context("parse CDP version json")?;
    let ws_url = ver
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("CDP: no webSocketDebuggerUrl"))?;

    let (mut ws, _) = connect_async(ws_url).await.context("connect CDP ws")?;
    ws.send(Message::Text(
        json!({ "id": 1, "method": "Storage.getCookies" }).to_string().into(),
    ))
    .await
    .context("send getCookies")?;

    let reply = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(anyhow!("CDP recv: {}", e)),
            None => return Err(anyhow!("CDP closed")),
        }
    };
    let _ = ws.close(None).await;

    let v: Value = serde_json::from_str(&reply).context("parse CDP reply")?;
    let cookies = v
        .get("result")
        .and_then(|r| r.get("cookies"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("CDP: no cookies"))?;
    let mut out = Vec::new();
    for c in cookies {
        let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        if !domain.contains("flipster") {
            continue;
        }
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if !name.is_empty() {
            out.push((name.to_string(), value.to_string()));
        }
    }
    if out.is_empty() {
        return Err(anyhow!("CDP: no flipster cookies found"));
    }
    Ok(out)
}
