//! Shared application state and the authentication extractor.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use tensorchat_core::{Id, IdGen, User};
use tensorchat_store::Store;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::hub::Hub;
use crate::ratelimit::IpLimiter;

pub struct AppState {
    pub cfg: Config,
    pub store: Store,
    pub hub: Hub,
    pub ids: IdGen,
    /// Guards the unauthenticated endpoints against credential stuffing.
    pub login_limiter: IpLimiter,
    /// VAPID identity for Web Push. `None` when push is switched off, which is
    /// what makes every push code path a no-op rather than a special case.
    pub vapid: Option<crate::push::Vapid>,
    /// The external identity provider, when one is configured. `None` leaves
    /// every OIDC route answering 404, so an unconfigured server has no extra
    /// surface rather than a disabled one.
    pub oidc: Option<crate::oidc::Oidc>,
    /// Outbound client, for pushes and for the OIDC token and userinfo calls.
    /// Held rather than built per request: it is the connection pool, and the
    /// TLS session cache with it.
    pub http: reqwest::Client,
    pub started: std::time::Instant,
}

pub type Shared = Arc<AppState>;

/// Install the TLS backend, once, before the first outbound client is built.
///
/// `reqwest` is compiled with `rustls-no-provider`, which makes this choice
/// explicit rather than implicit — and the choice is deliberate. The default,
/// `aws-lc-rs`, needs CMake at build time; `ring` does not. This project already
/// compiles C for the bundled SQLite, so `ring` adds no new class of build
/// requirement, while CMake would be one more thing every cross-compiled release
/// target has to provide.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Errors only if a provider is already installed, which is not a
        // problem — something has to win, and either one works.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl AppState {
    pub fn new(cfg: Config, store: Store) -> AppState {
        install_crypto_provider();
        let node = cfg.node_id;
        let (burst, rate) = (cfg.auth_burst, cfg.auth_per_second);
        let oidc = cfg.oidc.clone().map(crate::oidc::Oidc::new);
        AppState {
            cfg,
            oidc,
            store,
            hub: Hub::new(),
            ids: IdGen::new(node),
            vapid: None,
            // A short timeout: a push service that has not answered in ten
            // seconds is not going to, and the task is holding a subscription
            // row's worth of retry state while it waits.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .user_agent(concat!("tensorchat/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            // Defaults to ten attempts refilling at one per two seconds:
            // generous for a person who mistyped, useless for a script. See
            // `Config::auth_burst` for when raising it is legitimate.
            login_limiter: IpLimiter::new(burst, rate),
            started: std::time::Instant::now(),
        }
    }

    /// Attach a VAPID identity, enabling Web Push.
    ///
    /// Separate from `new` because loading it touches the database, which the
    /// constructor deliberately does not.
    pub fn with_push(mut self, vapid: Option<crate::push::Vapid>) -> Self {
        self.vapid = vapid;
        self
    }

    #[inline]
    pub fn next_id(&self) -> Id {
        self.ids.next()
    }

    /// Run a blocking store operation on the blocking pool.
    ///
    /// This is the **only** place `tensorchat_store` is called from, so the "never
    /// block a reactor thread" rule is enforced in one spot rather than
    /// remembered at every call site.
    pub async fn db<T, F>(&self, f: F) -> ApiResult<T>
    where
        F: FnOnce(&Store) -> tensorchat_store::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            // A join error means the closure panicked; that is our bug.
            .map_err(|e| ApiError::Internal(format!("blocking task failed: {e}")))?
            .map_err(Into::into)
    }
}

/// An authenticated request. Extracting this proves the caller holds a live
/// session; handlers that take it can never accidentally run unauthenticated.
pub struct Auth(pub User);

impl FromRequestParts<Shared> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(&parts.headers).ok_or(ApiError::Unauthorized)?;
        let hash = crate::auth::token_hash(&token);
        let now = tensorchat_core::now_ms();

        // A session first, since interactive clients are the overwhelming
        // majority of requests; then a long-lived API token, so a bot presents
        // its credential exactly like a browser does and every downstream
        // authorization rule applies to it unchanged.
        let user = match state.db(move |s| s.session_user(&hash, now)).await {
            Ok(user) => user,
            Err(_) => state
                .db(move |s| s.api_token_user(&hash, now))
                .await
                // Any lookup failure is "not authenticated"; distinguishing
                // expired from forged tells an attacker which tokens are real.
                .map_err(|_| ApiError::Unauthorized)?,
        };
        Ok(Auth(user))
    }
}

/// An authenticated request from a workspace administrator.
///
/// A separate extractor rather than a check inside each handler, for the same
/// reason [`Auth`] exists: a route that takes this cannot accidentally run
/// without the privilege, and the check cannot be forgotten in a new handler.
pub struct AdminAuth(pub User);

impl FromRequestParts<Shared> for AdminAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        let Auth(user) = Auth::from_request_parts(parts, state).await?;
        if !user.admin {
            return Err(ApiError::Forbidden);
        }
        Ok(AdminAuth(user))
    }
}

/// Pull a session token from `Authorization: Bearer`, falling back to a cookie.
///
/// The cookie form exists for one reason: browsers cannot set headers on a
/// WebSocket handshake or on a plain `<img src>` for an attachment, so those
/// two paths need a credential the browser will attach on its own.
///
/// Takes headers rather than `Parts` so handlers that need to identify *their
/// own* session — logging out, or sparing the calling device when revoking the
/// rest — can reuse it instead of re-implementing the fallback chain.
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION)
        && let Ok(s) = v.to_str()
        && let Some(rest) = s.strip_prefix("Bearer ")
    {
        let t = rest.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookie_value(cookies, "tc_session")
}

/// Extract one cookie by name from a `Cookie:` header value.
///
/// Written out rather than pulled in as a dependency: the format is two
/// separators, and a cookie jar crate would be more code than this.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, Request, header};

    fn parts_with(headers: &[(header::HeaderName, &str)]) -> axum::http::HeaderMap {
        let mut req = Request::new(());
        for (k, v) in headers {
            req.headers_mut()
                .insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        req.into_parts().0.headers
    }

    #[test]
    fn prefers_the_authorization_header() {
        let p = parts_with(&[
            (header::AUTHORIZATION, "Bearer header-token"),
            (header::COOKIE, "tc_session=cookie-token"),
        ]);
        assert_eq!(bearer_token(&p).as_deref(), Some("header-token"));
    }

    #[test]
    fn falls_back_to_the_cookie() {
        let p = parts_with(&[(header::COOKIE, "other=1; tc_session=abc123; x=2")]);
        assert_eq!(bearer_token(&p).as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_malformed_authorization_values() {
        for bad in ["", "Bearer", "Bearer ", "Basic abc", "bearer lowercase"] {
            let p = parts_with(&[(header::AUTHORIZATION, bad)]);
            assert_eq!(bearer_token(&p), None, "{bad:?} should not authenticate");
        }
    }

    #[test]
    fn no_credentials_at_all_yields_none() {
        assert_eq!(bearer_token(&parts_with(&[])), None);
    }

    #[test]
    fn parses_cookies_with_awkward_spacing() {
        assert_eq!(
            cookie_value("a=1;tc_session=t", "tc_session").as_deref(),
            Some("t")
        );
        assert_eq!(
            cookie_value("  tc_session = t  ", "tc_session").as_deref(),
            Some("t")
        );
        assert_eq!(cookie_value("tc_session_other=t", "tc_session"), None);
        assert_eq!(cookie_value("novalue", "tc_session"), None);
    }
}
