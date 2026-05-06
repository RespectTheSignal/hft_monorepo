//! Loader for the cookies.json produced by `scripts/dump_cookies.py`.
//!
//! Layout:
//! ```json
//! { "ts": "...Z", "flipster": {...}, "gate": {...} }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    #[error("read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("missing or non-object key '{key}' in {path}")]
    MissingKey { key: String, path: String },
}

pub struct CookieBundle {
    pub flipster: HashMap<String, String>,
    pub gate: HashMap<String, String>,
    pub bingx: HashMap<String, String>,
    pub ts: String,
}

pub fn load(path: &Path) -> Result<CookieBundle, CookieError> {
    let path_disp = path.display().to_string();
    let raw = std::fs::read_to_string(path).map_err(|e| CookieError::Read {
        path: path_disp.clone(),
        source: e,
    })?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| CookieError::Parse {
        path: path_disp.clone(),
        source: e,
    })?;
    let flipster = pluck(&v, "flipster", &path_disp)?;
    let gate = pluck(&v, "gate", &path_disp)?;
    // BingX is optional — older cookies.json files predate the BingX integration.
    let bingx = pluck(&v, "bingx", &path_disp).unwrap_or_default();
    let ts = v
        .get("ts")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    Ok(CookieBundle {
        flipster,
        gate,
        bingx,
        ts,
    })
}

fn pluck(
    v: &serde_json::Value,
    key: &str,
    path: &str,
) -> Result<HashMap<String, String>, CookieError> {
    let obj = v
        .get(key)
        .and_then(|x| x.as_object())
        .ok_or_else(|| CookieError::MissingKey {
            key: key.to_string(),
            path: path.to_string(),
        })?;
    Ok(obj
        .iter()
        .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
        .collect())
}

/// Resolve a path that may start with `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}
