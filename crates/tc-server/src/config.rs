//! Runtime configuration, from environment variables.
//!
//! Environment over a config file: it is what container platforms, systemd
//! units, and `docker run` all speak natively, and it keeps secrets out of the
//! repository by construction.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// SQLite database path. Its parent directory must exist.
    pub db_path: PathBuf,
    /// Directory for uploaded file bytes.
    pub blob_dir: PathBuf,
    /// Directory of built frontend assets to serve at `/`.
    pub web_dir: PathBuf,
    /// Distinguishes ID generators when several instances share a database.
    pub node_id: u16,
    pub max_upload_bytes: usize,
    /// When false, `/api/register` is closed and accounts are provisioned by
    /// an operator. Open by default so a fresh install is usable immediately.
    pub open_registration: bool,
    /// Serve `Access-Control-Allow-Origin: *`. Off by default — the frontend
    /// is served from the same origin, so CORS is only needed for development
    /// against a separate dev server.
    pub permissive_cors: bool,
    /// Burst allowance for `/api/login` and `/api/register`, per client
    /// address.
    ///
    /// Configurable because the default is calibrated for real users arriving
    /// from distinct addresses. Two situations legitimately need it raised:
    /// a load test hammering from one host, and a deployment behind a proxy
    /// that presents every client as the same peer address.
    pub auth_burst: f32,
    /// Sustained refill rate for the same limiter, in attempts per second.
    pub auth_per_second: f32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind: "127.0.0.1:8080".parse().expect("valid default addr"),
            db_path: PathBuf::from("tensorchat.db"),
            blob_dir: PathBuf::from("blobs"),
            web_dir: PathBuf::from("web/dist"),
            node_id: 0,
            max_upload_bytes: 25 * 1024 * 1024,
            open_registration: true,
            permissive_cors: false,
            auth_burst: 10.0,
            auth_per_second: 0.5,
        }
    }
}

impl Config {
    /// Read configuration from the environment, falling back to defaults.
    ///
    /// Malformed values are a hard error: silently falling back to a default
    /// port when `TC_BIND` is a typo is the kind of thing that gets discovered
    /// in production.
    pub fn from_env() -> Result<Config, String> {
        let mut c = Config::default();

        if let Ok(v) = std::env::var("TC_BIND") {
            c.bind = v.parse().map_err(|e| format!("TC_BIND: {e}"))?;
        }
        if let Ok(v) = std::env::var("TC_DB") {
            c.db_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("TC_BLOBS") {
            c.blob_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("TC_WEB") {
            c.web_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("TC_NODE_ID") {
            c.node_id = v.parse().map_err(|e| format!("TC_NODE_ID: {e}"))?;
        }
        if let Ok(v) = std::env::var("TC_MAX_UPLOAD") {
            c.max_upload_bytes = v.parse().map_err(|e| format!("TC_MAX_UPLOAD: {e}"))?;
        }
        if let Ok(v) = std::env::var("TC_OPEN_REGISTRATION") {
            c.open_registration =
                parse_bool(&v).ok_or("TC_OPEN_REGISTRATION: expected true/false")?;
        }
        if let Ok(v) = std::env::var("TC_PERMISSIVE_CORS") {
            c.permissive_cors = parse_bool(&v).ok_or("TC_PERMISSIVE_CORS: expected true/false")?;
        }
        if let Ok(v) = std::env::var("TC_AUTH_BURST") {
            c.auth_burst = v.parse().map_err(|e| format!("TC_AUTH_BURST: {e}"))?;
        }
        if let Ok(v) = std::env::var("TC_AUTH_PER_SECOND") {
            c.auth_per_second = v.parse().map_err(|e| format!("TC_AUTH_PER_SECOND: {e}"))?;
        }
        Ok(c)
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_booleans_people_actually_type() {
        for t in ["1", "true", "TRUE", "yes", " on "] {
            assert_eq!(parse_bool(t), Some(true), "{t:?}");
        }
        for f in ["0", "false", "no", "OFF"] {
            assert_eq!(parse_bool(f), Some(false), "{f:?}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn defaults_bind_to_loopback() {
        // Binding 0.0.0.0 by default would expose an unconfigured instance to
        // the network on first run.
        assert!(Config::default().bind.ip().is_loopback());
    }
}
