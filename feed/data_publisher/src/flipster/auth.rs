use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

/// Generate HMAC-SHA256 signature for Flipster API authentication.
///
/// Canonical message: METHOD + path (with query) + expires + body (optional)
pub fn sign(secret: &str, method: &str, path: &str, expires: i64, body: Option<&[u8]>) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(method.to_uppercase().as_bytes());
    message.extend_from_slice(path.as_bytes());
    message.extend_from_slice(expires.to_string().as_bytes());
    if let Some(body) = body {
        message.extend_from_slice(body);
    }

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(&message);

    hex::encode(mac.finalize().into_bytes())
}

/// Generate an expiry timestamp (current UTC epoch seconds + offset).
pub fn make_expires(offset_secs: i64) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + offset_secs
}
