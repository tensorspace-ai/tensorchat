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
    /// How people actually reach this server, e.g. `https://chat.example.com`.
    /// The server never needs it — it sits behind whatever proxy the operator
    /// put there — but `tensorchat invite` has to print a link somebody can
    /// click, and the bind address is usually not it.
    pub public_url: Option<String>,
    /// Contact address embedded in every VAPID token, as a `mailto:` or URL.
    /// Push services want somewhere to complain to; setting it to an empty
    /// string switches Web Push off entirely.
    pub push_contact: String,
    /// An external OpenID Connect provider people may sign in through.
    /// `None` — the default — leaves the server exactly as it was, with
    /// passwords the only way in.
    pub oidc: Option<OidcConfig>,
}

/// Settings for signing in through an external OpenID Connect provider.
///
/// Either all of it is configured or none of it is. A half-configured provider
/// is a button that only fails once the user has already been bounced to
/// somebody else's login page, so the missing pieces are a startup error
/// instead.
#[derive(Clone)]
pub struct OidcConfig {
    /// Issuer URL, without a trailing slash. Endpoints are discovered from
    /// `{issuer}/.well-known/openid-configuration` rather than configured
    /// one by one — that document is the part of OIDC every provider
    /// implements, and it keeps this vendor-neutral.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// Where the provider sends the browser back to. Configured rather than
    /// derived from the request, because a redirect URI assembled from the
    /// `Host` header is one proxy misconfiguration away from sending
    /// authorization codes somewhere else — and because this has to match what
    /// was registered with the provider exactly, so it should be the same
    /// string in both places.
    pub redirect_url: String,
    /// Space-separated scopes. `openid` is what makes it an OIDC request at
    /// all; `profile` is what carries a username worth naming an account after.
    pub scopes: String,
    /// What the sign-in button calls this provider.
    pub label: String,
}

/// Redact the secret. `Config` is `Debug` and gets logged at startup; a client
/// secret in the log is a client secret in the log aggregator.
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_url", &self.redirect_url)
            .field("scopes", &self.scopes)
            .field("label", &self.label)
            .finish()
    }
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
            public_url: None,
            // On by default with a placeholder contact: push is useless without
            // it, and a self-hosted instance should not need configuration to
            // get notifications working.
            push_contact: "mailto:admin@localhost".to_string(),
            oidc: None,
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
        if let Ok(v) = std::env::var("TC_PUBLIC_URL") {
            let v = v.trim().to_string();
            c.public_url = (!v.is_empty()).then_some(v);
        }
        if let Ok(v) = std::env::var("TC_PUSH_CONTACT") {
            let v = v.trim().to_string();
            if !v.is_empty() && !v.starts_with("mailto:") && !v.starts_with("https://") {
                return Err("TC_PUSH_CONTACT: expected a mailto: or https: address".into());
            }
            c.push_contact = v;
        }
        c.oidc = OidcConfig::from_env()?;
        Ok(c)
    }
}

impl OidcConfig {
    /// Read the provider settings, or `None` when `TC_OIDC_ISSUER` is unset.
    fn from_env() -> Result<Option<OidcConfig>, String> {
        let Some(issuer) = non_empty("TC_OIDC_ISSUER") else {
            return Ok(None);
        };
        // A trailing slash here becomes a double slash in the discovery URL,
        // which some providers 404.
        let issuer = issuer.trim_end_matches('/').to_string();
        require_web_url("TC_OIDC_ISSUER", &issuer)?;

        let client_id =
            non_empty("TC_OIDC_CLIENT_ID").ok_or("TC_OIDC_CLIENT_ID: required when OIDC is on")?;
        let client_secret = non_empty("TC_OIDC_CLIENT_SECRET")
            .ok_or("TC_OIDC_CLIENT_SECRET: required when OIDC is on")?;
        let redirect_url = non_empty("TC_OIDC_REDIRECT_URL")
            .ok_or("TC_OIDC_REDIRECT_URL: required when OIDC is on")?;
        require_web_url("TC_OIDC_REDIRECT_URL", &redirect_url)?;

        let scopes = non_empty("TC_OIDC_SCOPES").unwrap_or_else(|| "openid profile".to_string());
        if !scopes.split_whitespace().any(|s| s == "openid") {
            return Err("TC_OIDC_SCOPES: must include `openid`".into());
        }
        // Naming the host is a better default than naming no one: "Sign in with
        // git.example.com" tells a user where they are about to be sent.
        let label = non_empty("TC_OIDC_LABEL")
            .unwrap_or_else(|| host_of(&issuer).unwrap_or("your provider").to_string());

        Ok(Some(OidcConfig {
            issuer,
            client_id,
            client_secret,
            redirect_url,
            scopes,
            label,
        }))
    }
}

/// A set, non-blank environment variable, trimmed.
///
/// Unset and empty mean the same thing throughout: `TC_OIDC_ISSUER=` in a
/// compose file is how an operator turns the feature off without deleting the
/// line.
fn non_empty(key: &str) -> Option<String> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// Require an absolute `http`/`https` URL, and require `https` unless it points
/// at this machine.
///
/// The whole flow rests on TLS: the authorization code, the client secret and
/// the access token all cross this connection, and the ID token's signature is
/// deliberately not checked because TLS is doing that job (OIDC Core §3.1.3.7).
/// Plain `http` to a loopback address is the exception every OAuth
/// implementation makes, because that traffic never leaves the host — it is how
/// anyone develops against a provider running next to them.
fn require_web_url(key: &str, url: &str) -> Result<(), String> {
    let (scheme, host) = scheme_and_host(url).ok_or_else(|| {
        format!("{key}: expected an absolute http:// or https:// URL, got {url:?}")
    })?;
    if scheme == "http" && !is_loopback_host(host) {
        return Err(format!(
            "{key}: refusing plain http to {host:?} — use https, or a loopback address for local development"
        ));
    }
    Ok(())
}

/// Split an absolute web URL into its scheme and bare host.
fn scheme_and_host(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    // Userinfo first, or `user@host` would be read as a host of `user`.
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal's colons are not a port separator.
        Some(rest) => rest.split_once(']')?.0,
        None => authority.split(':').next()?,
    };
    (!host.is_empty()).then_some((scheme, host))
}

fn host_of(url: &str) -> Option<&str> {
    scheme_and_host(url).map(|(_, host)| host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    fn splits_urls_into_scheme_and_host() {
        assert_eq!(
            scheme_and_host("https://git.example.com/path?q=1#f"),
            Some(("https", "git.example.com"))
        );
        assert_eq!(
            scheme_and_host("http://127.0.0.1:3000"),
            Some(("http", "127.0.0.1"))
        );
        // The colons inside an IPv6 literal are not a port separator, and
        // userinfo is not a host.
        assert_eq!(
            scheme_and_host("http://[::1]:3000/x"),
            Some(("http", "::1"))
        );
        assert_eq!(
            scheme_and_host("https://user@example.com/"),
            Some(("https", "example.com"))
        );

        for bad in [
            "example.com",
            "ftp://example.com",
            // `javascript:` and friends must not survive as a redirect target.
            "javascript://example.com",
            "https://",
        ] {
            assert_eq!(scheme_and_host(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn plain_http_is_allowed_only_to_this_machine() {
        // The flow's confidentiality rests on TLS, so http off-box is refused
        // rather than warned about.
        require_web_url("X", "https://git.example.com").unwrap();
        require_web_url("X", "http://localhost:3000").unwrap();
        require_web_url("X", "http://127.0.0.1:3000").unwrap();
        require_web_url("X", "http://[::1]:3000").unwrap();

        let err = require_web_url("X", "http://git.example.com").unwrap_err();
        assert!(err.contains("refusing plain http"), "{err}");
        assert!(require_web_url("X", "not a url").is_err());
    }

    #[test]
    fn a_loopback_lookalike_is_not_loopback() {
        // `localhost.example.com` is somebody else's domain, and `127.0.0.1.evil`
        // is not an address at all.
        assert!(is_loopback_host("localhost") && is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("127.0.0.1") && is_loopback_host("127.9.9.9"));
        assert!(!is_loopback_host("localhost.example.com"));
        assert!(!is_loopback_host("127.0.0.1.evil.net"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn the_client_secret_stays_out_of_the_debug_output() {
        let cfg = OidcConfig {
            issuer: "https://git.example.com".into(),
            client_id: "id".into(),
            client_secret: "hunter2-the-real-secret".into(),
            redirect_url: "https://chat.example.com/api/oauth/callback".into(),
            scopes: "openid profile".into(),
            label: "git.example.com".into(),
        };
        let shown = format!("{cfg:?}");
        assert!(
            !shown.contains("hunter2"),
            "the secret reached a log line: {shown}"
        );
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn defaults_bind_to_loopback() {
        // Binding 0.0.0.0 by default would expose an unconfigured instance to
        // the network on first run.
        assert!(Config::default().bind.ip().is_loopback());
    }
}
