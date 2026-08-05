//! End-to-end tests for signing in through an OpenID Connect provider.
//!
//! A real provider is stood up on a loopback port for each test rather than
//! mocked out behind a trait. The interesting failures in this flow are about
//! what actually crosses the wire — the client authentication on the token
//! request, the PKCE verifier, the shape of the discovery document — and a
//! mock built from our own assumptions would agree with those assumptions.
//! `tensorchat` talks to this over plain HTTP because it is loopback, which is
//! the exception `Config::require_web_url` makes.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tensorchat_server::{AppState, Config, build_router};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// A provider, on a real port.
// ---------------------------------------------------------------------------

/// What the fake provider was asked for, so a test can assert on the request
/// rather than only on the outcome.
#[derive(Default)]
struct Seen {
    /// Decoded form fields of the last token request.
    token_form: Vec<(String, String)>,
    /// The `Authorization` header of the last token request.
    token_auth: Option<String>,
    /// The bearer token presented to the userinfo endpoint.
    userinfo_bearer: Option<String>,
}

#[derive(Clone)]
struct Idp {
    base: String,
    /// Overrides the `issuer` the discovery document claims. Defaults to
    /// `base`.
    issuer_claim: Option<String>,
    /// The subject and names handed out by the userinfo endpoint.
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
    seen: Arc<Mutex<Seen>>,
}

async fn discovery(State(idp): State<Idp>) -> Json<Value> {
    let b = &idp.base;
    Json(json!({
        "issuer": idp.issuer_claim.clone().unwrap_or_else(|| b.clone()),
        "authorization_endpoint": format!("{b}/login/oauth/authorize"),
        "token_endpoint": format!("{b}/login/oauth/access_token"),
        "userinfo_endpoint": format!("{b}/login/oauth/userinfo"),
        "jwks_uri": format!("{b}/login/oauth/keys"),
    }))
}

async fn token(State(idp): State<Idp>, req: Request<Body>) -> Response {
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = to_bytes(req.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&body).to_string();
    let form: Vec<(String, String)> = body
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (percent_decode(k), percent_decode(v)))
        .collect();

    {
        let mut seen = idp.seen.lock().unwrap();
        seen.token_form = form.clone();
        seen.token_auth = auth;
    }

    let code = form
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.clone());
    if code.as_deref() != Some("the-code") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response();
    }
    Json(json!({
        "access_token": "the-access-token",
        "token_type": "bearer",
        "expires_in": 3600,
        // Present and deliberately unparseable: nothing reads it, and a test
        // that passed only because this was a valid JWT would be testing the
        // wrong thing.
        "id_token": "not.a.jwt",
    }))
    .into_response()
}

async fn userinfo(State(idp): State<Idp>, req: Request<Body>) -> Response {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    idp.seen.lock().unwrap().userinfo_bearer = bearer.clone();
    if bearer.as_deref() != Some("the-access-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut claims = json!({ "sub": idp.sub });
    if let Some(u) = &idp.preferred_username {
        claims["preferred_username"] = json!(u);
    }
    if let Some(n) = &idp.name {
        claims["name"] = json!(n);
    }
    Json(claims).into_response()
}

/// Reject anything that reaches the authorization endpoint: the browser goes
/// there, this server never should.
async fn authorize(Query(_): Query<std::collections::HashMap<String, String>>) -> StatusCode {
    StatusCode::OK
}

impl Idp {
    /// Start a provider on an ephemeral port. It lives until the test ends.
    async fn start() -> Idp {
        Idp::start_with(|_| {}).await
    }

    async fn start_with(adjust: impl FnOnce(&mut Idp)) -> Idp {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut idp = Idp {
            base: format!("http://127.0.0.1:{}", addr.port()),
            issuer_claim: None,
            sub: "provider-subject-1".into(),
            preferred_username: Some("alice".into()),
            name: Some("Alice Example".into()),
            seen: Arc::new(Mutex::new(Seen::default())),
        };
        adjust(&mut idp);

        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/login/oauth/authorize", get(authorize))
            .route("/login/oauth/access_token", post(token))
            .route("/login/oauth/userinfo", get(userinfo))
            .with_state(idp.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        idp
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// The application under test.
// ---------------------------------------------------------------------------

struct App {
    router: Router,
}

impl App {
    fn with_provider(idp: &Idp) -> App {
        App::configured(|c| {
            c.oidc = Some(tensorchat_server::config::OidcConfig {
                issuer: idp.base.clone(),
                client_id: "the-client".into(),
                client_secret: "the-secret".into(),
                redirect_url: "http://127.0.0.1:8080/api/oauth/callback".into(),
                scopes: "openid profile".into(),
                label: "Example".into(),
            });
        })
    }

    fn configured(adjust: impl FnOnce(&mut Config)) -> App {
        let mut cfg = Config::default();
        adjust(&mut cfg);
        let store = tensorchat_store::Store::open_in_memory().expect("in-memory store");
        let st = Arc::new(AppState::new(cfg, store));
        App {
            router: build_router(st),
        }
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut builder = Request::builder().method("GET").uri(path);
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        self.router.clone().oneshot(request).await.unwrap()
    }

    /// Walk the whole flow the way a browser would: start, follow the redirect
    /// to the provider, come back to the callback with a code.
    ///
    /// Returns the callback's response.
    async fn sign_in(&self, sub_code: &str) -> Response {
        let started = self.get("/api/oauth/start", None).await;
        assert_eq!(
            started.status(),
            StatusCode::SEE_OTHER,
            "start should redirect to the provider"
        );
        let location = header_str(&started, header::LOCATION).unwrap();
        let state = query_param(&location, "state").expect("no state in the authorize URL");
        let cookie = flow_cookie(&started).expect("no flow cookie set");

        self.get(
            &format!("/api/oauth/callback?code={sub_code}&state={state}"),
            Some(&cookie),
        )
        .await
    }
}

fn header_str(r: &Response, name: header::HeaderName) -> Option<String> {
    r.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The `tc_oauth=...` pair from a `Set-Cookie`, ready to send back.
fn flow_cookie(r: &Response) -> Option<String> {
    set_cookie(r, "tc_oauth")
}

fn set_cookie(r: &Response, name: &str) -> Option<String> {
    r.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| {
            let pair = v.split(';').next()?;
            let (k, value) = pair.split_once('=')?;
            // A cleared cookie has an empty value; that is not a credential.
            (k == name && !value.is_empty()).then(|| pair.to_string())
        })
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

async fn body_json(r: Response) -> Value {
    let bytes = to_bytes(r.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_first_sign_in_provisions_an_account_and_leaves_a_usable_session() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    let done = app.sign_in("the-code").await;
    assert_eq!(
        done.status(),
        StatusCode::SEE_OTHER,
        "the callback should send the browser back to the app"
    );
    assert_eq!(header_str(&done, header::LOCATION).as_deref(), Some("/"));

    let session = set_cookie(&done, "tc_session").expect("no session cookie issued");
    // The flow cookie is cleared on the way out, so it cannot be replayed.
    assert!(
        flow_cookie(&done).is_none(),
        "the flow cookie should be cleared once spent"
    );

    // The cookie alone is a working credential — the client never sees a token.
    let me = app.get("/api/me", Some(&session)).await;
    assert_eq!(me.status(), StatusCode::OK);
    let me = body_json(me).await;
    assert_eq!(me["h"], "alice", "the handle comes from preferred_username");
    assert_eq!(me["n"], "Alice Example");
    assert_eq!(me["adm"], true, "the first account here administers it");
}

#[tokio::test]
async fn the_token_request_authenticates_the_client_and_carries_the_pkce_verifier() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);
    app.sign_in("the-code").await;

    let seen = idp.seen.lock().unwrap();
    let field = |k: &str| {
        seen.token_form
            .iter()
            .find(|(f, _)| f == k)
            .map(|(_, v)| v.clone())
    };

    assert_eq!(field("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(field("code").as_deref(), Some("the-code"));
    assert_eq!(
        field("redirect_uri").as_deref(),
        Some("http://127.0.0.1:8080/api/oauth/callback"),
        "the redirect URI must be repeated exactly, or a conforming provider refuses"
    );
    // PKCE: the verifier whose challenge went out with the authorize request.
    let verifier = field("code_verifier").expect("no code_verifier sent");
    assert_eq!(verifier.len(), 43, "expected 256 bits, got {verifier:?}");

    // client_secret_basic, and the secret never appears in the body.
    let auth = seen.token_auth.clone().expect("no client authentication");
    assert!(auth.starts_with("Basic "), "got {auth:?}");
    assert!(
        !seen.token_form.iter().any(|(k, _)| k == "client_secret"),
        "the secret should authenticate the request, not ride in the form"
    );

    assert_eq!(seen.userinfo_bearer.as_deref(), Some("the-access-token"));
}

#[tokio::test]
async fn the_same_subject_returns_to_the_same_account() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    let first = app.sign_in("the-code").await;
    let first_session = set_cookie(&first, "tc_session").unwrap();
    let first_me = body_json(app.get("/api/me", Some(&first_session)).await).await;

    // A second sign-in, as if from another browser.
    let second = app.sign_in("the-code").await;
    let second_session = set_cookie(&second, "tc_session").unwrap();
    let second_me = body_json(app.get("/api/me", Some(&second_session)).await).await;

    assert_eq!(first_me["id"], second_me["id"], "same person, same account");
    assert_ne!(first_session, second_session, "but a distinct session");

    let users = body_json(app.get("/api/users", Some(&first_session)).await).await;
    assert_eq!(
        users.as_array().map(Vec::len),
        Some(1),
        "a second sign-in must not provision a second account: {users}"
    );
}

#[tokio::test]
async fn a_callback_that_did_not_start_here_is_refused() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    // No flow cookie at all: someone pasted a callback URL.
    let bare = app
        .get("/api/oauth/callback?code=the-code&state=anything", None)
        .await;
    assert_eq!(bare.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&bare, "tc_session").is_none(), "no session");

    // A real flow, but the state does not match the one this browser was
    // given — which is what an attacker feeding their own code would look like.
    let started = app.get("/api/oauth/start", None).await;
    let cookie = flow_cookie(&started).unwrap();
    let forged = app
        .get(
            "/api/oauth/callback?code=the-code&state=not-the-state",
            Some(&cookie),
        )
        .await;
    assert_eq!(
        forged.status(),
        StatusCode::BAD_REQUEST,
        "a mismatched state must not sign anyone in"
    );
    assert!(set_cookie(&forged, "tc_session").is_none());
}

#[tokio::test]
async fn a_callback_cannot_be_replayed() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    let started = app.get("/api/oauth/start", None).await;
    let location = header_str(&started, header::LOCATION).unwrap();
    let state = query_param(&location, "state").unwrap();
    let cookie = flow_cookie(&started).unwrap();
    let url = format!("/api/oauth/callback?code=the-code&state={state}");

    let first = app.get(&url, Some(&cookie)).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);

    // The very same URL and cookie again. The flow was consumed, so there is
    // nothing left to match against.
    let again = app.get(&url, Some(&cookie)).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&again, "tc_session").is_none());
}

#[tokio::test]
async fn a_provider_that_refuses_sends_the_browser_back_without_a_session() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    let started = app.get("/api/oauth/start", None).await;
    let cookie = flow_cookie(&started).unwrap();
    // What a provider sends when the user presses cancel.
    let denied = app
        .get(
            "/api/oauth/callback?error=access_denied&error_description=user%20refused",
            Some(&cookie),
        )
        .await;

    assert_eq!(denied.status(), StatusCode::SEE_OTHER);
    assert!(
        set_cookie(&denied, "tc_session").is_none(),
        "a refusal must not mint a session"
    );
}

#[tokio::test]
async fn a_bad_code_fails_without_leaking_the_provider_s_complaint() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    let response = app.sign_in("wrong-code").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(response).await;
    assert_eq!(
        body["message"], "internal error",
        "the token endpoint's response is logged, not forwarded: {body}"
    );
}

#[tokio::test]
async fn a_discovery_document_naming_a_different_issuer_is_refused() {
    // The issuer is half the primary key of every identity row. If the document
    // disagrees with the configured issuer, the two would file the same subject
    // under different keys.
    let idp = Idp::start_with(|i| i.issuer_claim = Some("https://somewhere.else".into())).await;
    let app = App::with_provider(&idp);

    let started = app.get("/api/oauth/start", None).await;
    assert_eq!(started.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(flow_cookie(&started).is_none());
}

#[tokio::test]
async fn a_provider_name_that_is_not_a_handle_still_yields_an_account() {
    let idp = Idp::start_with(|i| {
        i.preferred_username = Some("Ünüsable ✨".into());
        i.name = None;
    })
    .await;
    let app = App::with_provider(&idp);

    let done = app.sign_in("the-code").await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    let session = set_cookie(&done, "tc_session").unwrap();
    let me = body_json(app.get("/api/me", Some(&session)).await).await;
    assert_eq!(
        me["h"], "nsable",
        "what survives the character class, rather than a refusal: {me}"
    );
}

#[tokio::test]
async fn a_provider_handle_that_is_taken_is_numbered() {
    let idp = Idp::start().await;
    let app = App::with_provider(&idp);

    // Somebody already registered `alice` with a password.
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/register")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"handle":"alice","password":"correct horse battery"}))
                .unwrap(),
        ))
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
    ));
    let registered = app.router.clone().oneshot(req).await.unwrap();
    assert_eq!(registered.status(), StatusCode::OK);

    let done = app.sign_in("the-code").await;
    let session = set_cookie(&done, "tc_session").unwrap();
    let me = body_json(app.get("/api/me", Some(&session)).await).await;
    assert_eq!(me["h"], "alice2");
    // `adm` is omitted when false, so its absence is the assertion.
    assert!(
        me["adm"].is_null(),
        "the password account got there first, so it administers: {me}"
    );
}

#[tokio::test]
async fn the_login_screen_is_told_which_providers_exist() {
    let idp = Idp::start().await;

    let configured = App::with_provider(&idp);
    let body = body_json(configured.get("/api/auth/providers", None).await).await;
    assert_eq!(body["oidc"]["label"], "Example");

    // With nothing configured the button has nothing to draw, and the routes
    // are not merely disabled — they are absent.
    let plain = App::configured(|_| {});
    let body = body_json(plain.get("/api/auth/providers", None).await).await;
    assert_eq!(body["oidc"], Value::Null);
    assert_eq!(
        plain.get("/api/oauth/start", None).await.status(),
        StatusCode::NOT_FOUND
    );
}
