//! Signing in through an external OpenID Connect provider.
//!
//! An authorization code flow with PKCE. Two endpoints: `/api/oauth/start`
//! sends the browser to the provider, `/api/oauth/callback` receives it back,
//! turns the code into a subject, and mints an ordinary local session. Nothing
//! downstream of that knows the session came from here — the `Auth` extractor,
//! the WebSocket handshake and logout are all unchanged.
//!
//! # The ID token's signature is deliberately not checked
//!
//! The provider returns an ID token signed with RS256 (or ES256, or one of a
//! dozen others it may choose). Verifying it means fetching a JWKS, caching it,
//! handling key rotation, and pulling in an RSA implementation — a meaningful
//! amount of new security-critical code.
//!
//! It also is not necessary here. The code is exchanged over a direct
//! server-to-server TLS connection to the token endpoint, and the subject is
//! then read from the userinfo endpoint over the same kind of connection.
//! OIDC Core §3.1.3.7 says exactly this: when the ID token is received by
//! direct communication with the token endpoint, TLS server authentication may
//! be used to validate the issuer instead of checking the token's signature.
//! The ID token is therefore never parsed at all, and the trust is placed in
//! one mechanism (TLS, which the whole flow already depends on) rather than
//! two.
//!
//! The cost is one extra round trip per sign-in. A login is not a hot path.
//!
//! This is also why `config::require_web_url` refuses plain `http` to anything
//! but a loopback address: without TLS the argument above evaporates.
//!
//! # Why a `nonce` is not sent
//!
//! A nonce binds an ID token to the request that asked for it. Since no ID
//! token is consumed, there would be nothing to check it against — sending one
//! would be decoration. The replay protection that matters here is PKCE, which
//! binds the *code* to this server, and the `state`/cookie pair, which binds
//! the round trip to this browser.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tensorchat_core::{now_ms, text};
use tensorchat_store::OidcLogin;

use crate::auth;
use crate::config::OidcConfig;
use crate::error::{ApiError, ApiResult};
use crate::state::Shared;

/// Name of the cookie holding the in-flight sign-in's secret.
const FLOW_COOKIE: &str = "tc_oauth";

/// How long a started sign-in stays valid.
///
/// Long enough to type a password and answer a second factor at the provider,
/// short enough that an abandoned flow is not a lasting entry in the map.
const FLOW_TTL: Duration = Duration::from_secs(10 * 60);

/// Ceiling on flows in flight, after expiry pruning.
///
/// `/api/oauth/start` is unauthenticated by necessity, so without a cap it is
/// an invitation to allocate memory a few hundred bytes at a time.
const MAX_FLOWS: usize = 4096;

/// How long a discovery document is reused before being fetched again.
const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);

/// A sign-in that has been started and not yet completed.
struct Pending {
    /// The `state` handed to the provider, to be compared with what comes back.
    state: String,
    /// The PKCE code verifier, whose challenge went out with the request.
    verifier: String,
    started: Instant,
}

/// The endpoints read out of the provider's discovery document.
#[derive(Clone, Debug, Deserialize)]
struct Endpoints {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    userinfo_endpoint: String,
}

/// A configured provider, with its caches.
pub struct Oidc {
    pub cfg: OidcConfig,
    /// Discovery is lazy and cached rather than done at startup: a provider
    /// that is briefly unreachable should not stop this server from booting and
    /// serving everyone whose session is already live.
    discovery: tokio::sync::RwLock<Option<(Arc<Endpoints>, Instant)>>,
    flows: DashMap<String, Pending>,
}

impl Oidc {
    pub fn new(cfg: OidcConfig) -> Oidc {
        Oidc {
            cfg,
            discovery: tokio::sync::RwLock::new(None),
            flows: DashMap::new(),
        }
    }

    /// The provider's endpoints, from cache when it is fresh.
    async fn endpoints(&self, http: &reqwest::Client) -> ApiResult<Arc<Endpoints>> {
        if let Some((cached, at)) = self.discovery.read().await.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(cached.clone());
        }

        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer);
        let response = http
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::Internal(format!("OIDC discovery request to {url}: {e}")))?;
        if !response.status().is_success() {
            return Err(ApiError::Internal(format!(
                "OIDC discovery at {url} answered {}",
                response.status()
            )));
        }
        let found: Endpoints = response.json().await.map_err(|e| {
            ApiError::Internal(format!("OIDC discovery at {url} is not usable: {e}"))
        })?;

        // The issuer identifies the provider, and it is half the primary key of
        // every identity row. A document served from our configured issuer that
        // *names* a different one means the two would disagree about who a
        // subject belongs to, so refuse rather than pick one.
        if found.issuer.trim_end_matches('/') != self.cfg.issuer {
            return Err(ApiError::Internal(format!(
                "OIDC discovery at {url} claims issuer {:?}, expected {:?}",
                found.issuer, self.cfg.issuer
            )));
        }

        let found = Arc::new(found);
        *self.discovery.write().await = Some((found.clone(), Instant::now()));
        Ok(found)
    }

    /// Record a started sign-in, returning the secret to put in the cookie.
    fn begin(&self, state: String, verifier: String) -> String {
        self.flows.retain(|_, f| f.started.elapsed() < FLOW_TTL);
        if self.flows.len() >= MAX_FLOWS {
            // Full of flows that are not yet expired: drop the oldest rather
            // than refuse a legitimate sign-in.
            let oldest = self
                .flows
                .iter()
                .min_by_key(|f| f.started)
                .map(|f| f.key().clone());
            if let Some(k) = oldest {
                self.flows.remove(&k);
            }
        }

        let secret = auth::new_session_token().secret;
        self.flows.insert(
            digest(&secret),
            Pending {
                state,
                verifier,
                started: Instant::now(),
            },
        );
        secret
    }

    /// Consume a started sign-in. Removing it is what makes a callback
    /// single-use, so a replayed callback URL finds nothing.
    fn take(&self, secret: &str) -> Option<Pending> {
        let (_, flow) = self.flows.remove(&digest(secret))?;
        (flow.started.elapsed() < FLOW_TTL).then_some(flow)
    }
}

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/auth/providers", get(providers))
        .route("/api/oauth/start", get(start))
        .route("/api/oauth/callback", get(callback))
}

#[derive(Serialize)]
struct ProviderInfo {
    label: String,
}

#[derive(Serialize)]
struct ProvidersRes {
    /// `None` when no provider is configured, which is how the client decides
    /// whether to draw the button.
    oidc: Option<ProviderInfo>,
}

/// What sign-in methods this server offers. Unauthenticated by necessity — it
/// is read by the login screen, before anyone has a session.
async fn providers(State(st): State<Shared>) -> axum::Json<ProvidersRes> {
    axum::Json(ProvidersRes {
        oidc: st.oidc.as_ref().map(|o| ProviderInfo {
            label: o.cfg.label.clone(),
        }),
    })
}

/// Send the browser to the provider.
async fn start(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> ApiResult<Response> {
    // Rate-limited with the other unauthenticated endpoints. Each start costs a
    // discovery lookup and a map entry, both on behalf of someone who has not
    // identified themselves.
    if !st.login_limiter.allow(peer.ip()) {
        return Err(ApiError::RateLimited);
    }
    let oidc = st.oidc.as_ref().ok_or(ApiError::NotFound)?;
    let endpoints = oidc.endpoints(&st.http).await?;

    // `state` guards the round trip; the verifier guards the code. Both are
    // fresh 256-bit secrets from the same generator sessions use.
    let state = auth::new_session_token().secret;
    let verifier = auth::new_session_token().secret;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let secret = oidc.begin(state.clone(), verifier);

    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        endpoints.authorization_endpoint,
        urlencode(&oidc.cfg.client_id),
        urlencode(&oidc.cfg.redirect_url),
        urlencode(&oidc.cfg.scopes),
        urlencode(&state),
        urlencode(&challenge),
    );

    let mut headers = HeaderMap::new();
    if let Ok(v) = header::HeaderValue::from_str(&flow_cookie(&secret, is_secure(&st))) {
        headers.insert(header::SET_COOKIE, v);
    }
    Ok((headers, Redirect::to(&url)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// The provider refused, e.g. because the user pressed cancel. Carried so
    /// the browser can be sent back to a login screen that says so, rather than
    /// to a bare error.
    error: Option<String>,
    error_description: Option<String>,
    /// Fields real providers append that this endpoint does not act on.
    ///
    /// `deny_unknown_fields` is the house rule, and it is right — but a query
    /// type on a URL somebody *else* builds has to name the extras or the flow
    /// breaks against a conforming provider. `iss` is RFC 9207, `session_state`
    /// is OIDC session management, and Gitea and Keycloak both send at least
    /// one of them.
    #[allow(dead_code)]
    iss: Option<String>,
    #[allow(dead_code)]
    session_state: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

/// Receive the browser back from the provider and sign the person in.
async fn callback(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    if !st.login_limiter.allow(peer.ip()) {
        return Err(ApiError::RateLimited);
    }
    let oidc = st.oidc.as_ref().ok_or(ApiError::NotFound)?;

    // Whatever happens from here, the flow cookie has served its purpose and
    // should not survive to be replayed against a later sign-in.
    let clear = clear_flow_cookie(is_secure(&st));

    if let Some(error) = q.error {
        tracing::info!(
            error,
            detail = q.error_description.unwrap_or_default(),
            "the provider refused a sign-in"
        );
        return Ok(finish("/#/oauth-error", &[&clear]));
    }

    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| crate::state::cookie_value(c, FLOW_COOKIE))
        .ok_or_else(|| ApiError::BadRequest("this sign-in did not start here".into()))?;
    let flow = oidc
        .take(&cookie)
        .ok_or_else(|| ApiError::BadRequest("this sign-in expired; please try again".into()))?;

    // The returned `state` must match the one this browser was given. Without
    // it, an attacker who completes a flow at the provider can feed their own
    // authorization code to somebody else's browser and silently sign that
    // person into the attacker's account.
    let returned = q.state.unwrap_or_default();
    if !constant_time_eq(returned.as_bytes(), flow.state.as_bytes()) {
        return Err(ApiError::BadRequest(
            "this sign-in did not start here".into(),
        ));
    }
    let code = q
        .code
        .ok_or_else(|| ApiError::BadRequest("the provider returned no code".into()))?;

    let endpoints = oidc.endpoints(&st.http).await?;
    let access_token = exchange_code(&st, oidc, &endpoints, &code, &flow.verifier).await?;
    let claims = fetch_userinfo(&st, &endpoints, &access_token).await?;

    if claims.sub.trim().is_empty() {
        return Err(ApiError::Internal(
            "the provider's userinfo response has no subject".into(),
        ));
    }

    // The provider's name for someone is a starting point, not a handle. The
    // store numbers it if it is taken.
    let hint = claims
        .preferred_username
        .as_deref()
        .and_then(text::handle_from_external)
        .or_else(|| claims.name.as_deref().and_then(text::handle_from_external))
        .unwrap_or_else(|| "user".to_string());
    let display = claims
        .name
        .filter(|n| !n.trim().is_empty() && n.chars().count() <= text::MAX_DISPLAY_NAME_LEN)
        .unwrap_or_else(|| hint.clone());

    let (issuer, subject) = (endpoints.issuer.clone(), claims.sub);
    let id = st.next_id();
    let now = now_ms();
    let outcome = st
        .db(move |s| s.user_for_oidc_identity(&issuer, &subject, id, &hint, &display, now))
        .await?;

    let user = match outcome {
        OidcLogin::Existing(u) => u,
        OidcLogin::Created(u) => {
            tracing::info!(handle = %u.handle, "provisioned an account from a provider sign-in");
            u
        }
        // Deliberately the same answer a deactivated password login gets.
        OidcLogin::Deactivated => return Err(ApiError::Unauthorized),
    };

    let token = auth::new_session_token();
    let (hash, uid) = (token.hash, user.id);
    st.db(move |s| s.create_session(&hash, uid, now, now + auth::SESSION_TTL_MS))
        .await?;

    // The session rides home in the cookie alone. Putting the token in the URL
    // fragment — as the invite flow does, for a credential that has to survive
    // a page the server never sees — would write a live session token into the
    // browser's history for no benefit: the client can read `/api/me` with the
    // cookie it already has.
    let session = crate::api::session_cookie(&token.secret, is_secure(&st));
    Ok(finish("/", &[&session, &clear]))
}

/// Send the browser back to the app, setting cookies on the way.
fn finish(target: &str, cookies: &[&str]) -> Response {
    let mut response = Redirect::to(target).into_response();
    for c in cookies {
        if let Ok(v) = header::HeaderValue::from_str(c) {
            response.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    response
}

#[derive(Deserialize)]
struct TokenRes {
    access_token: String,
}

/// Trade the authorization code for an access token.
async fn exchange_code(
    st: &Shared,
    oidc: &Oidc,
    endpoints: &Endpoints,
    code: &str,
    verifier: &str,
) -> ApiResult<String> {
    // Encoded by hand rather than with reqwest's `form` feature, which would
    // add `serde_urlencoded` to the tree to join five known pairs with `&`.
    let body = form_urlencode(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", oidc.cfg.redirect_url.as_str()),
        // Sent alongside the Basic header because some providers identify the
        // client from the body regardless of how it authenticated.
        ("client_id", oidc.cfg.client_id.as_str()),
        ("code_verifier", verifier),
    ]);
    let response = st
        .http
        .post(&endpoints.token_endpoint)
        // `client_secret_basic`: RFC 6749 §2.3.1 requires servers to support
        // it, where accepting the secret as a form field is only optional.
        .basic_auth(&oidc.cfg.client_id, Some(&oidc.cfg.client_secret))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("OIDC token request: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        // The body names the reason (`invalid_grant`, a redirect URI that does
        // not match what was registered) and is the difference between a
        // five-minute fix and an afternoon. `Internal`'s detail is logged and
        // never sent to the client.
        let body = response.text().await.unwrap_or_default();
        return Err(ApiError::Internal(format!(
            "OIDC token endpoint answered {status}: {}",
            body.chars().take(500).collect::<String>()
        )));
    }
    let token: TokenRes = response
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("OIDC token response is not usable: {e}")))?;
    Ok(token.access_token)
}

/// The claims this application reads. Everything else the provider sends is
/// ignored rather than stored.
#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
}

async fn fetch_userinfo(
    st: &Shared,
    endpoints: &Endpoints,
    access_token: &str,
) -> ApiResult<UserInfo> {
    let response = st
        .http
        .get(&endpoints.userinfo_endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("OIDC userinfo request: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "OIDC userinfo endpoint answered {status}"
        )));
    }
    response
        .json()
        .await
        .map_err(|e| ApiError::Internal(format!("OIDC userinfo response is not usable: {e}")))
}

/// Percent-encode a value for a query string.
///
/// Hand-rolled rather than pulled in: the set of characters that may appear
/// unescaped in a query value is small and fixed, and this is the only place
/// the server builds a URL for somebody else to parse.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Join key/value pairs into an `application/x-www-form-urlencoded` body.
fn form_urlencode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn digest(secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

/// Compare two secrets without letting the time taken reveal a prefix match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Digest first, so the comparison is fixed-length: a plain loop returns
    // early when the lengths differ and hands over the length for free.
    let (a, b) = (Sha256::digest(a), Sha256::digest(b));
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn is_secure(st: &Shared) -> bool {
    !st.cfg.bind.ip().is_loopback()
}

/// `SameSite=Lax` because the provider returns the browser by a top-level GET,
/// which `Strict` would strip the cookie from — the sign-in would then always
/// fail on the last hop.
fn flow_cookie(secret: &str, secure: bool) -> String {
    let mut c = format!(
        "{FLOW_COOKIE}={secret}; HttpOnly; SameSite=Lax; Path=/api/oauth; Max-Age={}",
        FLOW_TTL.as_secs()
    );
    if secure {
        c.push_str("; Secure");
    }
    c
}

fn clear_flow_cookie(secure: bool) -> String {
    let mut c = format!("{FLOW_COOKIE}=; HttpOnly; SameSite=Lax; Path=/api/oauth; Max-Age=0");
    if secure {
        c.push_str("; Secure");
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> Oidc {
        Oidc::new(OidcConfig {
            issuer: "https://idp.example.com".into(),
            client_id: "id".into(),
            client_secret: "secret".into(),
            redirect_url: "https://chat.example.com/api/oauth/callback".into(),
            scopes: "openid profile".into(),
            label: "idp.example.com".into(),
        })
    }

    #[test]
    fn a_flow_is_single_use() {
        let o = provider();
        let secret = o.begin("state".into(), "verifier".into());
        assert!(o.take(&secret).is_some());
        // A replayed callback URL finds nothing, so the code cannot be
        // presented twice.
        assert!(o.take(&secret).is_none());
    }

    #[test]
    fn only_the_digest_of_the_flow_secret_is_kept() {
        let o = provider();
        let secret = o.begin("state".into(), "verifier".into());
        assert!(
            !o.flows.iter().any(|f| f.key() == &secret),
            "the cookie's value must not be usable as a map key on its own"
        );
        assert!(o.flows.contains_key(&digest(&secret)));
    }

    #[test]
    fn flows_in_flight_are_capped() {
        let o = provider();
        for _ in 0..MAX_FLOWS + 50 {
            o.begin("state".into(), "verifier".into());
        }
        assert!(
            o.flows.len() <= MAX_FLOWS,
            "an unauthenticated endpoint must not grow the map without bound, got {}",
            o.flows.len()
        );
    }

    #[test]
    fn the_pkce_challenge_is_the_sha256_of_the_verifier() {
        // The value RFC 7636's appendix B fixes, so a mistake in the encoding
        // shows up here rather than as a provider rejecting every sign-in.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn urlencoding_escapes_what_would_break_the_query() {
        assert_eq!(urlencode("openid profile"), "openid%20profile");
        assert_eq!(
            urlencode("https://a.example.com/cb?x=1&y=2"),
            "https%3A%2F%2Fa.example.com%2Fcb%3Fx%3D1%26y%3D2"
        );
        // Unreserved characters survive untouched.
        assert_eq!(urlencode("aZ0-_.~"), "aZ0-_.~");
    }

    #[test]
    fn secrets_compare_without_leaking_their_length_or_prefix() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn the_flow_cookie_is_scoped_and_not_readable_from_script() {
        let c = flow_cookie("s", true);
        assert!(c.contains("HttpOnly"));
        assert!(
            c.contains("SameSite=Lax"),
            "Strict would break the callback"
        );
        // Scoped to the only path that uses it, so it is not attached to every
        // request the app makes.
        assert!(c.contains("Path=/api/oauth"));
        assert!(c.contains("; Secure"));
        assert!(!flow_cookie("s", false).contains("; Secure"));
        assert!(clear_flow_cookie(false).contains("Max-Age=0"));
    }
}
