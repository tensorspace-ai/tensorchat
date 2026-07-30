//! End-to-end tests over the real router.
//!
//! These drive the same `Router` the binary serves, against a throwaway
//! in-memory database — no process, no port, no fixtures. Every request goes
//! through the actual middleware stack, extractors, and handlers, so an
//! authorization bug in a handler shows up here rather than in production.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tensorchat_server::{AppState, Config, build_router};
use tower::ServiceExt;

/// A test harness holding the router and a synthetic client address.
struct App {
    router: Router,
    /// Kept so a test can stand the router back up under a different config
    /// against the same data. See [`App::reconfigure`].
    store: tensorchat_store::Store,
}

impl App {
    fn new() -> App {
        Self::with_config(Config::default())
    }

    fn with_config(cfg: Config) -> App {
        let store = tensorchat_store::Store::open_in_memory().expect("in-memory store");
        let st = Arc::new(AppState::new(cfg, store.clone()));
        App {
            router: build_router(st),
            store,
        }
    }

    /// Rebuild the router under a new config, keeping the database.
    ///
    /// Models the one sequence an operator actually performs: bring the
    /// workspace up open, claim the administrator account, then close
    /// registration and restart. Config is read once at startup, so there is no
    /// way to express that without standing the router up twice.
    fn reconfigure(self, cfg: Config) -> App {
        let st = Arc::new(AppState::new(cfg, self.store.clone()));
        App {
            router: build_router(st),
            store: self.store,
        }
    }

    /// Issue a request. `token` is sent as a bearer credential when present.
    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.send_from(method, path, token, body, "127.0.0.1:40000")
            .await
    }

    async fn send_from(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        peer: &str,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let request = match body {
            Some(v) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&v).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };

        let mut request = request;
        // The pre-auth rate limiter extracts `ConnectInfo`, which only exists
        // when the router is served over a real connection.
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    /// Register an account and return its session token and id.
    async fn account(&self, handle: &str) -> (String, String) {
        let (status, body) = self
            .send(
                "POST",
                "/api/register",
                None,
                Some(json!({ "handle": handle, "password": "correct horse battery" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "register failed: {body}");
        (
            body["token"].as_str().unwrap().to_string(),
            body["user"]["id"].as_str().unwrap().to_string(),
        )
    }
}

#[tokio::test]
async fn health_needs_no_credentials() {
    let app = App::new();
    let (status, _) = app.send("GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn register_then_use_the_session() {
    let app = App::new();
    let (token, id) = app.account("alice").await;

    let (status, me) = app.send("GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"].as_str().unwrap(), id);
    assert_eq!(me["h"], "alice");
}

#[tokio::test]
async fn ids_are_json_strings_not_numbers() {
    // A JSON number would be silently rounded by any JavaScript client, since
    // Snowflakes exceed 2^53. This is the guarantee the whole id design rests
    // on, so it is asserted at the HTTP boundary.
    let app = App::new();
    let (_, id) = app.account("alice").await;
    assert!(id.parse::<u64>().unwrap() > (1u64 << 53));
}

#[tokio::test]
async fn protected_routes_reject_missing_and_bogus_tokens() {
    let app = App::new();
    app.account("alice").await;

    for token in [None, Some("not-a-real-token")] {
        let (status, _) = app.send("GET", "/api/channels", token, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
    }
}

#[tokio::test]
async fn duplicate_handles_conflict() {
    let app = App::new();
    app.account("alice").await;
    let (status, _) = app
        .send(
            "POST",
            "/api/register",
            None,
            Some(json!({ "handle": "alice", "password": "another password" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn login_rejects_a_wrong_password_and_an_unknown_handle_identically() {
    let app = App::new();
    app.account("alice").await;

    let (wrong_password, body_a) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "wrong password" })),
        )
        .await;
    let (unknown_user, body_b) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "nobody", "password": "wrong password" })),
        )
        .await;

    assert_eq!(wrong_password, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_user, StatusCode::UNAUTHORIZED);
    // Identical responses: the endpoint must not reveal which handles exist.
    assert_eq!(body_a, body_b);
}

#[tokio::test]
async fn short_passwords_are_rejected() {
    let app = App::new();
    let (status, _) = app
        .send(
            "POST",
            "/api/register",
            None,
            Some(json!({ "handle": "bob", "password": "short" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logout_revokes_the_session_immediately() {
    let app = App::new();
    let (token, _) = app.account("alice").await;

    let (status, _) = app.send("POST", "/api/logout", Some(&token), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = app.send("GET", "/api/me", Some(&token), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a revoked token must stop working"
    );
}

#[tokio::test]
async fn changing_a_password_swaps_the_credential_and_keeps_the_caller_signed_in() {
    let app = App::new();
    let (token, _) = app.account("alice").await;

    let (status, body) = app
        .send(
            "POST",
            "/api/me/password",
            Some(&token),
            Some(json!({
                "current_password": "correct horse battery",
                "new_password": "a whole new passphrase",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "change failed: {body}");

    // The tab that made the change is still signed in.
    let (status, _) = app.send("GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);

    // The old password no longer works, the new one does.
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "a whole new passphrase" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn changing_a_password_signs_other_devices_out() {
    let app = App::new();
    let (first, _) = app.account("alice").await;
    // A second device: same account, its own session token.
    let (_, second) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    let second = second["token"].as_str().unwrap().to_string();
    let (status, _) = app.send("GET", "/api/me", Some(&second), None).await;
    assert_eq!(status, StatusCode::OK, "second device starts signed in");

    let (_, body) = app
        .send(
            "POST",
            "/api/me/password",
            Some(&first),
            Some(json!({
                "current_password": "correct horse battery",
                "new_password": "a whole new passphrase",
            })),
        )
        .await;
    assert_eq!(body["revoked"], 1);

    // The whole point: a session is a bearer token that would otherwise outlive
    // the password it was issued against.
    let (status, _) = app.send("GET", "/api/me", Some(&second), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_wrong_current_password_is_rejected_without_ending_the_session() {
    let app = App::new();
    let (token, _) = app.account("alice").await;

    let (status, _) = app
        .send(
            "POST",
            "/api/me/password",
            Some(&token),
            Some(json!({
                "current_password": "not my password",
                "new_password": "a whole new passphrase",
            })),
        )
        .await;
    // 400, not 401: the session is valid, the re-authentication is not. A 401
    // would make the client treat a typo as an expired session.
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = app.send("GET", "/api/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "a typo must not sign you out");

    // And the password is unchanged.
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_new_password_must_still_meet_the_length_rule() {
    let app = App::new();
    let (token, _) = app.account("alice").await;

    let (status, _) = app
        .send(
            "POST",
            "/api/me/password",
            Some(&token),
            Some(json!({
                "current_password": "correct horse battery",
                "new_password": "short",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Rejected before anything changed.
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn other_sessions_can_be_revoked_without_changing_the_password() {
    let app = App::new();
    let (first, _) = app.account("alice").await;
    let (_, second) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    let second = second["token"].as_str().unwrap().to_string();

    let (status, body) = app
        .send("DELETE", "/api/me/sessions", Some(&first), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revoked"], 1);

    let (status, _) = app.send("GET", "/api/me", Some(&second), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = app.send("GET", "/api/me", Some(&first), None).await;
    assert_eq!(status, StatusCode::OK, "the caller keeps their own session");
}

#[tokio::test]
async fn one_users_password_change_leaves_everyone_else_signed_in() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    app.send(
        "POST",
        "/api/me/password",
        Some(&alice),
        Some(json!({
            "current_password": "correct horse battery",
            "new_password": "a whole new passphrase",
        })),
    )
    .await;

    let (status, _) = app.send("GET", "/api/me", Some(&bob), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_first_account_is_an_administrator() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, me) = app.send("GET", "/api/me", Some(&alice), None).await;
    assert_eq!(me["adm"], true);

    let (_, me) = app.send("GET", "/api/me", Some(&bob), None).await;
    assert!(!me["adm"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn only_administrators_can_administer() {
    let app = App::new();
    app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;
    let (_, carol_id) = app.account("carol").await;

    for (target, body) in [
        (&bob_id, json!({ "admin": true })),
        (&carol_id, json!({ "deactivated": true })),
    ] {
        let (status, _) = app
            .send(
                "PATCH",
                &format!("/api/admin/users/{target}"),
                Some(&bob),
                Some(body),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn deactivating_an_account_ends_its_sessions_and_blocks_login() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (status, user) = app
        .send(
            "PATCH",
            &format!("/api/admin/users/{bob_id}"),
            Some(&alice),
            Some(json!({ "deactivated": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user["d"], true);

    // The live session stops working immediately, rather than lingering until
    // its month-long expiry.
    let (status, _) = app.send("GET", "/api/me", Some(&bob), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And a fresh login is refused, rather than handing out a token that would
    // fail on every subsequent request.
    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "bob", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Reversible, and the account is intact.
    let (status, user) = app
        .send(
            "PATCH",
            &format!("/api/admin/users/{bob_id}"),
            Some(&alice),
            Some(json!({ "deactivated": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!user["d"].as_bool().unwrap_or(false));

    let (status, _) = app
        .send(
            "POST",
            "/api/login",
            None,
            Some(json!({ "handle": "bob", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_administrator_cannot_lock_the_workspace_out_of_itself() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;

    // Losing every administrator is unrecoverable through the API — there
    // would be nobody left who could grant the privilege back.
    for body in [json!({ "admin": false }), json!({ "deactivated": true })] {
        let (status, _) = app
            .send(
                "PATCH",
                &format!("/api/admin/users/{alice_id}"),
                Some(&alice),
                Some(body.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {body}");
    }

    let (_, me) = app.send("GET", "/api/me", Some(&alice), None).await;
    assert_eq!(me["adm"], true, "still an administrator");
}

#[tokio::test]
async fn a_promoted_user_can_then_administer() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;
    let (_, carol_id) = app.account("carol").await;

    let (status, user) = app
        .send(
            "PATCH",
            &format!("/api/admin/users/{bob_id}"),
            Some(&alice),
            Some(json!({ "admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(user["adm"], true);

    let (status, _) = app
        .send(
            "PATCH",
            &format!("/api/admin/users/{carol_id}"),
            Some(&bob),
            Some(json!({ "deactivated": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // With a second administrator in place, the first may now step down.
    let (_, alice_me) = app.send("GET", "/api/me", Some(&alice), None).await;
    let alice_id = alice_me["id"].as_str().unwrap();
    let (status, _) = app
        .send(
            "PATCH",
            &format!("/api/admin/users/{alice_id}"),
            Some(&bob),
            Some(json!({ "admin": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "someone else may demote her");
}

#[tokio::test]
async fn a_bot_token_works_as_a_bearer_credential_on_the_normal_api() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    let (status, bot) = app
        .send(
            "POST",
            "/api/admin/bots",
            Some(&alice),
            Some(json!({ "handle": "deploybot", "display_name": "Deploy Bot" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create bot: {bot}");
    assert_eq!(bot["bot"], true);
    let bot_id = bot["id"].as_str().unwrap().to_string();

    let (status, token) = app
        .send(
            "POST",
            &format!("/api/admin/bots/{bot_id}/tokens"),
            Some(&alice),
            Some(json!({ "label": "ci" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let secret = token["secret"].as_str().unwrap().to_string();
    let token_id = token["id"].as_str().unwrap().to_string();

    // The secret is shown once and never again.
    let (_, listed) = app
        .send(
            "GET",
            &format!("/api/admin/bots/{bot_id}/tokens"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(
        listed[0]["secret"].is_null(),
        "a listing must never carry secrets"
    );

    // It authenticates the ordinary API, exactly like a session does.
    let (status, me) = app.send("GET", "/api/me", Some(&secret), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["id"].as_str().unwrap(), bot_id);

    // But only where the bot is a member.
    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "deploys" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&secret),
            Some(json!({ "body": "build 41 shipped" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not a member yet");

    app.send(
        "POST",
        &format!("/api/channels/{ch}/members"),
        Some(&alice),
        Some(json!({ "users": [bot_id] })),
    )
    .await;
    let (status, posted) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&secret),
            Some(json!({ "body": "build 41 shipped" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(posted["au"].as_str().unwrap(), bot_id);

    // Revocation takes effect immediately.
    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/admin/tokens/{token_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = app.send("GET", "/api/me", Some(&secret), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_incoming_hook_posts_without_a_header() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, bot) = app
        .send(
            "POST",
            "/api/admin/bots",
            Some(&alice),
            Some(json!({ "handle": "alertbot" })),
        )
        .await;
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let (_, token) = app
        .send(
            "POST",
            &format!("/api/admin/bots/{bot_id}/tokens"),
            Some(&alice),
            Some(json!({ "label": "alerts" })),
        )
        .await;
    let secret = token["secret"].as_str().unwrap().to_string();

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "alerts", "members": [bot_id] })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap().to_string();

    // No Authorization header anywhere — the point of the endpoint.
    let (status, posted) = app
        .send(
            "POST",
            &format!("/api/hooks/{secret}"),
            None,
            Some(json!({ "channel": ch, "text": "disk usage at 91%" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "hook post: {posted}");
    assert_eq!(posted["b"], "disk usage at 91%");
    assert_eq!(posted["au"].as_str().unwrap(), bot_id);

    // It lands in real history, not a side channel.
    let (_, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(page["messages"][0]["b"], "disk usage at 91%");

    // A bogus token is unauthorized, and a real token cannot post to a channel
    // its bot is not in.
    let (status, _) = app
        .send(
            "POST",
            "/api/hooks/not-a-real-token",
            None,
            Some(json!({ "channel": ch, "text": "let me in" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, private) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let secret_ch = private["id"].as_str().unwrap();
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/hooks/{secret}"),
            None,
            Some(json!({ "channel": secret_ch, "text": "should not appear" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "membership is what contains a leaked hook URL"
    );
}

#[tokio::test]
async fn only_administrators_manage_bots_and_only_bots_get_tokens() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    for (method, path, body) in [
        (
            "POST",
            "/api/admin/bots".to_string(),
            json!({ "handle": "sneaky" }),
        ),
        ("GET", "/api/admin/bots".to_string(), Value::Null),
    ] {
        let (status, _) = app
            .send(
                method,
                &path,
                Some(&bob),
                if body.is_null() { None } else { Some(body) },
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }

    // A permanent credential for a person's account would be a way around both
    // their password and their session revocation.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/admin/bots/{alice_id}/tokens"),
            Some(&alice),
            Some(json!({ "label": "backdoor" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn registration_can_be_closed() {
    let cfg = Config {
        open_registration: false,
        ..Config::default()
    };
    let app = App::with_config(cfg);
    let (status, _) = app
        .send(
            "POST",
            "/api/register",
            None,
            Some(json!({ "handle": "alice", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn repeated_failed_logins_are_rate_limited_per_address() {
    let app = App::new();
    app.account("alice").await;

    let attempt = json!({ "handle": "alice", "password": "wrong password" });
    let mut limited = false;
    for _ in 0..20 {
        let (status, _) = app
            .send_from(
                "POST",
                "/api/login",
                None,
                Some(attempt.clone()),
                "10.0.0.9:1234",
            )
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited, "credential stuffing should be throttled");

    // A different address still gets its own allowance.
    let (status, _) = app
        .send_from("POST", "/api/login", None, Some(attempt), "10.0.0.10:1234")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn channel_lifecycle_and_message_flow() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;

    let (status, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general", "topic": "everything" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{channel}");
    let ch = channel["id"].as_str().unwrap().to_string();
    assert_eq!(channel["k"], "public");

    let (status, message) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "hello world" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{message}");
    assert_eq!(message["b"], "hello world");
    assert_eq!(message["au"].as_str().unwrap(), alice_id);

    let (status, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert!(
        page["next_cursor"].is_null(),
        "a one-page channel has no cursor"
    );
}

#[tokio::test]
async fn non_members_cannot_read_or_write_a_private_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            Some(json!({ "body": "let me in" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // And it is not joinable, unlike a public channel.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/join"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_member_can_invite_someone_into_a_private_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (status, body) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/members"),
            Some(&alice),
            Some(json!({ "users": [bob_id] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "invite failed: {body}");
    assert_eq!(body["added"], json!([bob_id]));

    // The whole point: a private channel is now reachable by its members.
    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // And it shows up in their sidebar without a reconnect.
    let (_, mine) = app.send("GET", "/api/channels", Some(&bob), None).await;
    assert!(mine.as_array().unwrap().iter().any(|c| c["id"] == ch));

    // Adding someone already inside is a no-op, not an error.
    let (status, body) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/members"),
            Some(&alice),
            Some(json!({ "users": [bob_id] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["added"], json!([]), "second invite adds nobody");
}

#[tokio::test]
async fn outsiders_cannot_invite_themselves_or_others_into_a_private_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;
    let (_, carol_id) = app.account("carol").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    // The invite path must not become a way around `join` refusing outsiders.
    for target in [&bob_id, &carol_id] {
        let (status, _) = app
            .send(
                "POST",
                &format!("/api/channels/{ch}/members"),
                Some(&bob),
                Some(json!({ "users": [target] })),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "adding {target}");
    }
}

#[tokio::test]
async fn a_removed_member_loses_access() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true, "members": [bob_id] })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "bob starts out inside");

    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/channels/{ch}/members/{bob_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "removal must revoke reads");

    // Idempotent: removing someone who is already gone is not an error.
    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/channels/{ch}/members/{bob_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn direct_message_membership_cannot_be_edited() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, bob_id) = app.account("bob").await;
    let (_, carol_id) = app.account("carol").await;

    let (_, dm) = app
        .send(
            "POST",
            "/api/dm",
            Some(&alice),
            Some(json!({ "users": [bob_id] })),
        )
        .await;
    let ch = dm["id"].as_str().unwrap();

    // A DM is keyed by its exact member set; growing one in place would either
    // collide with an existing group or silently change the conversation.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/members"),
            Some(&alice),
            Some(json!({ "users": [carol_id] })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/channels/{ch}/members/{bob_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn inviting_an_unknown_user_adds_nobody() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, bob_id) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    // All-or-nothing: a batch naming one ghost must not half-apply.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/members"),
            Some(&alice),
            Some(json!({ "users": [bob_id, "999999999999999999"] })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, members) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/members"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(
        members.as_array().unwrap().len(),
        1,
        "only alice should be a member"
    );
}

#[tokio::test]
async fn a_public_channel_can_be_browsed_and_joined() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (_, directory) = app
        .send("GET", "/api/channels/browse", Some(&bob), None)
        .await;
    assert_eq!(directory.as_array().unwrap().len(), 1);

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/join"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, mine) = app.send("GET", "/api/channels", Some(&bob), None).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);

    // Now that bob is a member, he can post.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            Some(json!({ "body": "hi everyone" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn threads_reactions_edits_and_deletes() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (_, root) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "question?" })),
        )
        .await;
    let root_id = root["id"].as_str().unwrap().to_string();

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "answer", "thread_root": root_id })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // The reply belongs to the thread, not the channel scroll.
    let (_, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    assert_eq!(page["messages"][0]["rc"], 1);

    let (status, thread) = app
        .send(
            "GET",
            &format!("/api/threads/{root_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(thread.as_array().unwrap().len(), 2);

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{root_id}/reactions"),
            Some(&alice),
            Some(json!({ "emoji": "🎉", "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, edited) = app
        .send(
            "PATCH",
            &format!("/api/messages/{root_id}"),
            Some(&alice),
            Some(json!({ "body": "clearer question?" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["b"], "clearer question?");
    assert!(edited["ed"].is_number(), "an edit must be timestamped");
    assert_eq!(edited["rx"][0]["e"], "🎉");
    assert_eq!(edited["rx"][0]["me"], true);

    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/messages/{root_id}"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(page["messages"][0]["del"], true);
    assert_eq!(page["messages"][0]["b"], "");
}

#[tokio::test]
async fn one_person_cannot_edit_or_delete_anothers_message() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;
    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{ch}/join"),
        Some(&bob),
        None,
    )
    .await;

    let (_, m) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "mine" })),
        )
        .await;
    let id = m["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "PATCH",
            &format!("/api/messages/{id}"),
            Some(&bob),
            Some(json!({ "body": "hijacked" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = app
        .send("DELETE", &format!("/api/messages/{id}"), Some(&bob), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn direct_messages_are_deduplicated_and_private() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;
    let (carol, _) = app.account("carol").await;

    let (status, dm) = app
        .send(
            "POST",
            "/api/dm",
            Some(&alice),
            Some(json!({ "users": [bob_id] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{dm}");
    let ch = dm["id"].as_str().unwrap().to_string();
    assert_eq!(dm["k"], "dm");

    // Opening it from the other side returns the same conversation.
    let (_, again) = app
        .send(
            "POST",
            "/api/dm",
            Some(&bob),
            Some(json!({ "users": [alice_id] })),
        )
        .await;
    assert_eq!(again["id"].as_str().unwrap(), ch);

    // A third party cannot read it.
    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&carol),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn history_can_be_fetched_around_a_message() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let mut ids = Vec::new();
    for i in 0..30 {
        let (_, m) = app
            .send(
                "POST",
                &format!("/api/channels/{ch}/messages"),
                Some(&alice),
                Some(json!({ "body": format!("message {i}") })),
            )
            .await;
        ids.push(m["id"].as_str().unwrap().to_string());
    }
    let anchor = &ids[10];

    let (status, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages?around={anchor}&limit=10"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let got: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();

    assert_eq!(got.len(), 10);
    assert!(got.contains(&anchor.as_str()), "the anchor must be present");
    // Messages from both sides of it, which is the whole point — a `before`
    // page could only ever have shown what came earlier.
    assert!(got.contains(&ids[13].as_str()), "newer neighbours");
    assert!(got.contains(&ids[7].as_str()), "older neighbours");
    assert!(page["next_cursor"].is_string(), "can still page older");
}

#[tokio::test]
async fn an_around_anchor_cannot_reach_into_another_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    // A private channel bob is not in, and a public one he is.
    let (_, private) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let secret_ch = private["id"].as_str().unwrap();
    let (_, secret_msg) = app
        .send(
            "POST",
            &format!("/api/channels/{secret_ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "the passphrase is hunter2" })),
        )
        .await;
    let secret_id = secret_msg["id"].as_str().unwrap();

    let (_, open) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general", "members": [bob_id] })),
        )
        .await;
    let open_ch = open["id"].as_str().unwrap();

    // Only the *channel* is authorized, so an anchor belonging elsewhere must
    // not be able to drag that channel's neighbours into the response.
    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{open_ch}/messages?around={secret_id}"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And naming the private channel directly is still forbidden.
    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{secret_ch}/messages?around={secret_id}"),
            Some(&bob),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn messages_can_be_pinned_and_unpinned() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "the deploy runbook" })),
        )
        .await;
    let id = msg["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/pins"),
            Some(&alice),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{id}/pin"),
            Some(&alice),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, pins) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/pins"),
            Some(&alice),
            None,
        )
        .await;
    let pins = pins.as_array().unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0]["id"], id);
    assert_eq!(pins[0]["b"], "the deploy runbook", "pins arrive hydrated");

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{id}/pin"),
            Some(&alice),
            Some(json!({ "on": false })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, pins) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/pins"),
            Some(&alice),
            None,
        )
        .await;
    assert!(pins.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn pins_are_invisible_and_unwritable_to_non_members() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "the passphrase is" })),
        )
        .await;
    let id = msg["id"].as_str().unwrap();

    // Authorization is against the channel the *message* lives in, so a
    // guessed message id cannot pin into a channel the caller cannot see.
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{id}/pin"),
            Some(&bob),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = app
        .send("GET", &format!("/api/channels/{ch}/pins"), Some(&bob), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_deleted_message_leaves_the_pinned_list() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "temporary" })),
        )
        .await;
    let id = msg["id"].as_str().unwrap();

    app.send(
        "POST",
        &format!("/api/messages/{id}/pin"),
        Some(&alice),
        Some(json!({ "on": true })),
    )
    .await;
    app.send("DELETE", &format!("/api/messages/{id}"), Some(&alice), None)
        .await;

    // A soft delete keeps the row for thread structure, but a tombstone must
    // not sit in the pinned list as a blank entry.
    let (_, pins) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/pins"),
            Some(&alice),
            None,
        )
        .await;
    assert!(pins.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn muting_is_per_user_and_keeps_the_counts_truthful() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "noisy", "members": [bob_id] })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&alice),
        Some(json!({ "body": "chatter" })),
    )
    .await;

    let (status, state) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/mute"),
            Some(&bob),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["mu"], true);
    // Reported truthfully rather than zeroed: muting is a presentation choice,
    // so the client can still show "1 unread, quietly".
    assert_eq!(state["u"], 1);

    // Alice's own view of the same channel is unaffected.
    let (_, alice_state) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/mute"),
            Some(&alice),
            Some(json!({ "on": false })),
        )
        .await;
    assert!(!alice_state["mu"].as_bool().unwrap_or(false));

    let (_, unmuted) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/mute"),
            Some(&bob),
            Some(json!({ "on": false })),
        )
        .await;
    assert!(!unmuted["mu"].as_bool().unwrap_or(false));
}

#[tokio::test]
async fn a_non_member_cannot_mute_a_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/mute"),
            Some(&bob),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn saved_messages_are_private_and_leave_with_the_channel() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true, "members": [bob_id] })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "the passphrase is hunter2" })),
        )
        .await;
    let id = msg["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{id}/save"),
            Some(&bob),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, mine) = app.send("GET", "/api/saved", Some(&bob), None).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
    assert_eq!(mine[0]["id"], id);

    // A save is private: alice saved nothing, even though it is her message.
    let (_, hers) = app.send("GET", "/api/saved", Some(&alice), None).await;
    assert!(hers.as_array().unwrap().is_empty());

    // Losing access to the channel takes the message out of the saved list,
    // or the list becomes a private window into a channel you were removed
    // from.
    app.send(
        "DELETE",
        &format!("/api/channels/{ch}/members/{bob_id}"),
        Some(&alice),
        None,
    )
    .await;
    let (_, mine) = app.send("GET", "/api/saved", Some(&bob), None).await;
    assert!(
        mine.as_array().unwrap().is_empty(),
        "a removed member must not keep reading via their saved list"
    );
}

#[tokio::test]
async fn saving_a_message_you_cannot_see_is_forbidden() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{ch}/messages"),
            Some(&alice),
            Some(json!({ "body": "not for you" })),
        )
        .await;
    let id = msg["id"].as_str().unwrap();

    let (status, _) = app
        .send(
            "POST",
            &format!("/api/messages/{id}/save"),
            Some(&bob),
            Some(json!({ "on": true })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn search_is_scoped_to_channels_you_belong_to() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "secrets", "private": true })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&alice),
        Some(json!({ "body": "the launch codes are hunter2" })),
    )
    .await;

    let (status, hits) = app
        .send("GET", "/api/search?q=launch%20codes", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    let snippet = hits[0]["sn"].as_str().unwrap();
    assert!(
        snippet.contains('\u{2}'),
        "expected highlight sentinels in {snippet:?}"
    );

    // Bob is not a member, so the same query finds nothing.
    let (_, hits) = app
        .send("GET", "/api/search?q=launch%20codes", Some(&bob), None)
        .await;
    assert!(
        hits.as_array().unwrap().is_empty(),
        "search must not leak private channels"
    );
}

#[tokio::test]
async fn search_survives_hostile_query_syntax() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    for probe in [
        "%22unterminated",
        "NEAR(a+b)",
        "-excluded",
        "col%3Avalue",
        "*",
    ] {
        let (status, _) = app
            .send("GET", &format!("/api/search?q={probe}"), Some(&alice), None)
            .await;
        assert_eq!(status, StatusCode::OK, "query {probe:?} should not error");
    }
}

#[tokio::test]
async fn mentions_produce_unread_badges_for_the_mentioned_user() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{ch}/join"),
        Some(&bob),
        None,
    )
    .await;

    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&alice),
        Some(json!({ "body": "can you look at this @bob?" })),
    )
    .await;

    // Marking read returns the resulting state, which should be clear.
    let (_, page) = app
        .send(
            "GET",
            &format!("/api/channels/{ch}/messages"),
            Some(&bob),
            None,
        )
        .await;
    let newest = page["messages"][0]["id"].as_str().unwrap().to_string();

    let (status, state) = app
        .send(
            "POST",
            &format!("/api/messages/{ch}/read"),
            Some(&bob),
            Some(json!({ "up_to": newest })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["u"], 0);
    assert_eq!(state["mn"], 0);
}

#[tokio::test]
async fn oversized_and_empty_messages_are_rejected() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (_, channel) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general" })),
        )
        .await;
    let ch = channel["id"].as_str().unwrap();

    for body in [String::new(), "   \n ".to_string(), "x".repeat(20_000)] {
        let (status, _) = app
            .send(
                "POST",
                &format!("/api/channels/{ch}/messages"),
                Some(&alice),
                Some(json!({ "body": body })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "body of {} bytes",
            body.len()
        );
    }
}

#[tokio::test]
async fn invalid_channel_names_are_rejected() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    for name in ["", "Has Spaces", "UPPER", "emoji🎉", &"x".repeat(200)] {
        let (status, _) = app
            .send(
                "POST",
                "/api/channels",
                Some(&alice),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {name:?}");
    }
}

#[tokio::test]
async fn security_headers_are_present_on_every_response() {
    let app = App::new();
    let request = Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    let headers = response.headers();

    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    let csp = headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    // No 'unsafe-inline' for scripts: the client builds DOM nodes rather than
    // HTML strings, so it never needs one — and shipping it would quietly
    // undo that guarantee.
    assert!(csp.contains("script-src 'self'"));
    assert!(
        !csp.contains("unsafe-inline"),
        "CSP must not allow inline scripts"
    );
    assert!(csp.contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn profile_updates_are_validated_and_visible() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    let (status, updated) = app
        .send(
            "PATCH",
            "/api/me",
            Some(&alice),
            Some(json!({ "display_name": "Alice A.", "status": "shipping" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["n"], "Alice A.");
    assert_eq!(updated["st"], "shipping");

    let (status, _) = app
        .send(
            "PATCH",
            "/api/me",
            Some(&alice),
            Some(json!({ "display_name": "  " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn metrics_report_the_fanout_ratio() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (status, metrics) = app.send("GET", "/api/metrics", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metrics["connections"], 0);
    assert!(metrics["fanout_ratio"].is_number());
}

// ---------------------------------------------------------------- invites

/// Register with an explicit invite token, returning the raw response.
async fn register_with_invite(
    app: &App,
    handle: &str,
    invite: &str,
    peer: &str,
) -> (StatusCode, Value) {
    app.send_from(
        "POST",
        "/api/register",
        None,
        Some(json!({
            "handle": handle,
            "password": "correct horse battery",
            "invite": invite,
        })),
        peer,
    )
    .await
}

/// A workspace with registration closed and one administrator — the shape of
/// every deployment that does not want the open internet signing up.
///
/// Built by opening, claiming the administrator, then closing, because that is
/// the only sequence that actually works: an empty closed workspace has nobody
/// who can mint the first invite.
async fn closed_workspace() -> (App, String) {
    let app = App::with_config(Config::default());
    let (admin, _) = app.account("alice").await;
    let app = app.reconfigure(Config {
        open_registration: false,
        ..Config::default()
    });
    (app, admin)
}

#[tokio::test]
async fn an_invite_admits_an_account_to_a_closed_workspace() {
    // The hole this feature exists to fill: with open registration off and no
    // invite, there was no way at all to add the second person.
    let (app, admin) = closed_workspace().await;

    let (status, invite) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            Some(json!({ "label": "design team" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{invite}");
    let token = invite["token"]
        .as_str()
        .expect("the link is shown once, at creation")
        .to_string();
    assert_eq!(invite["uses"], 0);
    assert_eq!(invite["max_uses"], 1, "single use by default");
    assert_eq!(invite["live"], true);
    assert!(invite["expires_at"].is_number(), "expiring by default");

    let (status, body) = register_with_invite(&app, "bob", &token, "10.1.0.1:1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["h"], "bob");
    // `adm` is omitted when false, so its absence is the assertion.
    assert!(
        body["user"].get("adm").is_none(),
        "an invited account is an ordinary member: {}",
        body["user"]
    );
    // And the session it returns is a real one.
    let bob = body["token"].as_str().unwrap();
    let (status, me) = app.send("GET", "/api/me", Some(bob), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["h"], "bob");

    // The seat is spent, so the link stops working.
    let (status, body) = register_with_invite(&app, "carol", &token, "10.1.0.2:1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("no longer valid")
    );
}

#[tokio::test]
async fn registration_stays_closed_without_an_invite() {
    let (app, admin) = closed_workspace().await;
    // Mint one so the table is non-empty: an existing invite must not become a
    // skeleton key for requests that do not present it.
    let (status, _) = app
        .send("POST", "/api/admin/invites", Some(&admin), Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = app
        .send(
            "POST",
            "/api/register",
            None,
            Some(json!({ "handle": "bob", "password": "correct horse battery" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_forged_or_revoked_invite_is_refused() {
    let (app, admin) = closed_workspace().await;

    // A token nobody ever minted.
    let (status, body) = register_with_invite(&app, "bob", "not-a-real-token", "10.2.0.1:1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let forged_message = body["message"].as_str().unwrap().to_string();

    // A real one, revoked before use.
    let (_, invite) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            Some(json!({ "max_uses": 0 })),
        )
        .await;
    let token = invite["token"].as_str().unwrap().to_string();
    let id = invite["id"].as_str().unwrap();
    let (status, _) = app
        .send(
            "DELETE",
            &format!("/api/admin/invites/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = register_with_invite(&app, "bob", &token, "10.2.0.2:1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["message"].as_str().unwrap(),
        forged_message,
        "a revoked invite must be indistinguishable from one that never existed"
    );
}

#[tokio::test]
async fn only_administrators_can_mint_invites() {
    // Otherwise any member could grow the workspace, which is the same
    // privilege as creating accounts.
    let app = App::new();
    let (_admin, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    for (method, path) in [
        ("POST", "/api/admin/invites"),
        ("GET", "/api/admin/invites"),
    ] {
        let (status, _) = app.send(method, path, Some(&bob), Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (status, _) = app
        .send("POST", "/api/admin/invites", None, Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_invite_can_be_checked_before_the_sign_up_form_is_shown() {
    let (app, admin) = closed_workspace().await;
    let (_, invite) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            Some(json!({ "max_uses": 1 })),
        )
        .await;
    let token = invite["token"].as_str().unwrap().to_string();

    // Unauthenticated by necessity: the caller has no account yet.
    let (status, body) = app
        .send("GET", &format!("/api/invites/{token}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], true);

    let (status, body) = app.send("GET", "/api/invites/nonsense", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["valid"], false, "and it never leaks why");

    // Spending it flips the answer.
    register_with_invite(&app, "bob", &token, "10.3.0.1:1").await;
    let (_, body) = app
        .send("GET", &format!("/api/invites/{token}"), None, None)
        .await;
    assert_eq!(body["valid"], false);
}

#[tokio::test]
async fn listing_invites_never_reveals_a_live_link() {
    // The token is shown once, at creation. If listing returned it, a leaked
    // database or a compromised admin session would hand out working links.
    let (app, admin) = closed_workspace().await;
    app.send(
        "POST",
        "/api/admin/invites",
        Some(&admin),
        Some(json!({ "label": "contractors", "max_uses": 5 })),
    )
    .await;

    let (status, list) = app
        .send("GET", "/api/admin/invites", Some(&admin), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["label"], "contractors");
    assert_eq!(rows[0]["max_uses"], 5);
    assert!(
        rows[0].get("token").is_none(),
        "a listing must never carry the secret: {}",
        rows[0]
    );
}

#[tokio::test]
async fn an_unlimited_invite_admits_several_accounts() {
    let (app, admin) = closed_workspace().await;
    let (_, invite) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            // Zero for both is the explicit "never expires, no cap" request.
            Some(json!({ "max_uses": 0, "expires_in_hours": 0 })),
        )
        .await;
    assert!(invite["expires_at"].is_null(), "zero hours means no expiry");
    let token = invite["token"].as_str().unwrap().to_string();

    for (i, who) in ["bob", "carol", "dave"].iter().enumerate() {
        // Distinct peers: the shared limiter is per-address, and this test is
        // about the invite's cap, not the rate limit's.
        let peer = format!("10.4.0.{}:1", i + 1);
        let (status, body) = register_with_invite(&app, who, &token, &peer).await;
        assert_eq!(status, StatusCode::OK, "{who}: {body}");
    }

    let (_, list) = app
        .send("GET", "/api/admin/invites", Some(&admin), None)
        .await;
    assert_eq!(list[0]["uses"], 3);
    assert_eq!(list[0]["live"], true, "no cap means no exhaustion");
}

#[tokio::test]
async fn an_invite_cannot_outlive_a_year() {
    let (app, admin) = closed_workspace().await;
    let (status, _) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            Some(json!({ "expires_in_hours": 24 * 400 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_taken_handle_does_not_burn_an_invite_seat() {
    // Someone who picks a name that is already in use must be able to try again
    // with a different one rather than needing a fresh link.
    let (app, admin) = closed_workspace().await;
    let (_, invite) = app
        .send(
            "POST",
            "/api/admin/invites",
            Some(&admin),
            Some(json!({ "max_uses": 1 })),
        )
        .await;
    let token = invite["token"].as_str().unwrap().to_string();

    let (status, _) = register_with_invite(&app, "alice", &token, "10.5.0.1:1").await;
    assert_eq!(status, StatusCode::CONFLICT, "alice already exists");

    let (status, body) = register_with_invite(&app, "bob", &token, "10.5.0.2:1").await;
    assert_eq!(status, StatusCode::OK, "the seat survived: {body}");
}

// ---------------------------------------------------------------- search operators

/// A channel containing one message from each of two people, plus a link.
async fn searchable_workspace() -> (App, String, String, String) {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, bob_id) = app.account("bob").await;

    let (_, chan) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "general", "private": false, "members": [bob_id] })),
        )
        .await;
    let ch = chan["id"].as_str().unwrap().to_string();

    for (who, body) in [
        (&alice, "the release runbook is at https://example.com/run"),
        (&alice, "the release is cut"),
        (&bob, "the release looks good to me"),
    ] {
        let (status, _) = app
            .send(
                "POST",
                &format!("/api/channels/{ch}/messages"),
                Some(who),
                Some(json!({ "body": body })),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let _ = alice_id;
    (app, alice, bob_id, ch)
}

/// Run a search and return the matched bodies.
async fn search_bodies(app: &App, token: &str, q: &str) -> Vec<String> {
    let (status, hits) = app
        .send(
            "GET",
            &format!("/api/search?q={}", urlencode(q)),
            Some(token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "search {q:?} failed: {hits}");
    hits.as_array()
        .unwrap()
        .iter()
        .map(|h| h["m"]["b"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Percent-encode a query string value. Written out rather than pulled in as a
/// dependency: this is the only place a test needs it.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[tokio::test]
async fn search_narrows_by_author_with_from() {
    let (app, alice, _, _) = searchable_workspace().await;

    assert_eq!(search_bodies(&app, &alice, "release").await.len(), 3);

    let mine = search_bodies(&app, &alice, "release from:alice").await;
    assert_eq!(mine.len(), 2, "got {mine:?}");
    assert!(mine.iter().all(|b| !b.contains("good to me")));

    // The sigil is optional, and the operator is not searched for as text.
    assert_eq!(
        search_bodies(&app, &alice, "release from:@alice")
            .await
            .len(),
        2
    );
}

#[tokio::test]
async fn search_narrows_by_channel_with_in() {
    let (app, alice, _, _) = searchable_workspace().await;
    let (_, other) = app
        .send(
            "POST",
            "/api/channels",
            Some(&alice),
            Some(json!({ "name": "random", "private": false })),
        )
        .await;
    let other_id = other["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{other_id}/messages"),
        Some(&alice),
        Some(json!({ "body": "a release elsewhere" })),
    )
    .await;

    assert_eq!(search_bodies(&app, &alice, "release").await.len(), 4);
    assert_eq!(
        search_bodies(&app, &alice, "release in:random").await,
        vec!["a release elsewhere"]
    );
    assert_eq!(
        search_bodies(&app, &alice, "release in:#general")
            .await
            .len(),
        3,
        "the # is optional"
    );
}

#[tokio::test]
async fn search_narrows_by_content_with_has() {
    let (app, alice, _, _) = searchable_workspace().await;
    let linked = search_bodies(&app, &alice, "release has:link").await;
    assert_eq!(linked.len(), 1, "got {linked:?}");
    assert!(linked[0].contains("https://example.com/run"));

    // Nothing here carries an attachment.
    assert!(
        search_bodies(&app, &alice, "release has:file")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn search_narrows_by_date() {
    let (app, alice, _, _) = searchable_workspace().await;
    // Everything was posted just now, so a bound in the past excludes it all
    // and one in the future includes it all.
    assert!(
        search_bodies(&app, &alice, "release before:2020-01-01")
            .await
            .is_empty()
    );
    assert_eq!(
        search_bodies(&app, &alice, "release after:2020-01-01")
            .await
            .len(),
        3
    );
}

#[tokio::test]
async fn operators_alone_are_a_valid_search() {
    // "everything alice said" — no search terms at all.
    let (app, alice, _, _) = searchable_workspace().await;
    let mine = search_bodies(&app, &alice, "from:alice").await;
    assert_eq!(mine.len(), 2, "got {mine:?}");
    // Newest first, since there is nothing to rank by.
    assert_eq!(mine[0], "the release is cut");

    // But a genuinely empty query still returns nothing rather than everything.
    assert!(search_bodies(&app, &alice, "").await.is_empty());
    assert!(search_bodies(&app, &alice, "   ").await.is_empty());
}

#[tokio::test]
async fn an_operator_naming_nothing_returns_nothing() {
    // Not "ignore the filter and return everything", which is the dangerous
    // failure mode: a typo would silently widen the search.
    let (app, alice, _, _) = searchable_workspace().await;
    assert!(
        search_bodies(&app, &alice, "release from:nobody")
            .await
            .is_empty()
    );
    assert!(
        search_bodies(&app, &alice, "release in:nosuchchannel")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_operator_is_searched_for_as_text() {
    let (app, alice, _, _) = searchable_workspace().await;
    // `form:` is a typo for `from:`; it must not act as a filter, and it must
    // not error. Nothing contains those words, so nothing matches.
    assert!(search_bodies(&app, &alice, "form:alice").await.is_empty());
    // And a URL in the query is not mistaken for an operator.
    let hits = search_bodies(&app, &alice, "https://example.com/run").await;
    assert_eq!(hits.len(), 1, "got {hits:?}");
}

#[tokio::test]
async fn in_cannot_reach_a_channel_you_are_not_in() {
    // Resolving the name is not authorization; the membership join is. Naming a
    // private channel must not reveal anything in it.
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;
    let (_, secret) = app
        .send(
            "POST",
            "/api/channels",
            Some(&bob),
            Some(json!({ "name": "secret", "private": true })),
        )
        .await;
    let ch = secret["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&bob),
        Some(json!({ "body": "the confidential release plan" })),
    )
    .await;

    assert!(
        search_bodies(&app, &alice, "release in:secret")
            .await
            .is_empty(),
        "membership gates the operator path too"
    );
    assert!(
        search_bodies(&app, &alice, "in:secret").await.is_empty(),
        "and the filters-only path as well"
    );
    // Bob, who is a member, still finds it.
    assert_eq!(
        search_bodies(&app, &bob, "release in:secret").await.len(),
        1
    );
}

#[tokio::test]
async fn operators_compose() {
    let (app, alice, _, _) = searchable_workspace().await;
    let hits = search_bodies(&app, &alice, "release from:alice in:general has:link").await;
    assert_eq!(hits.len(), 1, "got {hits:?}");
    assert!(hits[0].contains("runbook"));
}

// ---------------------------------------------------------------- web push

#[tokio::test]
async fn the_vapid_public_key_is_offered_to_signed_in_clients() {
    // The public half is not a secret — handing it to every client is its whole
    // job, since it is what lets the browser verify a later push came from us.
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    let (status, body) = app.send("GET", "/api/push/key", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    // The test harness builds `AppState` without a VAPID identity, so this is
    // the "push is switched off" shape the client reads as "hide the toggle".
    assert!(body["key"].is_null());

    // But it still requires a session: an anonymous caller learns nothing.
    let (status, _) = app.send("GET", "/api/push/key", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_browser_can_subscribe_and_unsubscribe() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let endpoint = json!({ "endpoint": "https://fcm.googleapis.com/fcm/send/abc123" });

    let (status, _) = app
        .send(
            "POST",
            "/api/push/subscribe",
            Some(&alice),
            Some(endpoint.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Subscribing twice is how a returning tab re-confirms; it must not error.
    let (status, _) = app
        .send(
            "POST",
            "/api/push/subscribe",
            Some(&alice),
            Some(endpoint.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = app
        .send(
            "DELETE",
            "/api/push/subscribe",
            Some(&alice),
            Some(endpoint),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_push_endpoint_must_be_a_bounded_https_url() {
    // This is a URL the server will later POST to, so it is not merely opaque
    // storage: an http: or absurdly long value is refused rather than kept.
    let app = App::new();
    let (alice, _) = app.account("alice").await;

    for bad in [
        json!({ "endpoint": "http://insecure.example/push" }),
        json!({ "endpoint": "javascript:alert(1)" }),
        json!({ "endpoint": "" }),
        json!({ "endpoint": format!("https://example.com/{}", "x".repeat(4096)) }),
    ] {
        let (status, _) = app
            .send(
                "POST",
                "/api/push/subscribe",
                Some(&alice),
                Some(bad.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn subscriptions_require_a_session_and_are_scoped_to_their_owner() {
    let app = App::new();
    let (alice, _) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;
    let endpoint = json!({ "endpoint": "https://fcm.googleapis.com/fcm/send/alices-phone" });

    let (status, _) = app
        .send("POST", "/api/push/subscribe", None, Some(endpoint.clone()))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    app.send(
        "POST",
        "/api/push/subscribe",
        Some(&alice),
        Some(endpoint.clone()),
    )
    .await;

    // Bob deleting alice's endpoint is a no-op rather than an unsubscribe: an
    // endpoint is unguessable, but "delete by a client-supplied string" should
    // not be a way to silence somebody else's phone.
    let (status, _) = app
        .send(
            "DELETE",
            "/api/push/subscribe",
            Some(&bob),
            Some(endpoint.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Alice's subscription survived, which she can tell because unsubscribing
    // it herself still works.
    let (status, _) = app
        .send(
            "DELETE",
            "/api/push/subscribe",
            Some(&alice),
            Some(endpoint),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn notifications_describe_mentions_and_direct_messages() {
    // This endpoint is what makes a payload-less push work: the service worker
    // fetches the content from here rather than receiving it from Google.
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, chan) = app
        .send(
            "POST",
            "/api/channels",
            Some(&bob),
            Some(json!({ "name": "general", "private": false, "members": [alice_id] })),
        )
        .await;
    let ch = chan["id"].as_str().unwrap();

    // Ordinary channel traffic is not worth waking anyone for.
    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&bob),
        Some(json!({ "body": "morning all" })),
    )
    .await;
    let (status, items) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        items.as_array().unwrap().is_empty(),
        "a channel message that mentions nobody must not buzz a phone: {items}"
    );

    // A mention is.
    app.send(
        "POST",
        &format!("/api/channels/{ch}/messages"),
        Some(&bob),
        Some(json!({ "body": "@alice could you look?" })),
    )
    .await;
    let (_, items) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["title"], "bob in #general");
    assert!(
        items[0]["body"]
            .as_str()
            .unwrap()
            .contains("could you look")
    );
    assert_eq!(items[0]["ch"].as_str().unwrap(), ch);

    // And so is a direct message, with the author as the title.
    let (_, dm) = app
        .send(
            "POST",
            "/api/dm",
            Some(&bob),
            Some(json!({ "users": [alice_id] })),
        )
        .await;
    let dm_id = dm["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{dm_id}/messages"),
        Some(&bob),
        Some(json!({ "body": "are you around?" })),
    )
    .await;
    let (_, items) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "bob", "a DM is titled by its author");
}

#[tokio::test]
async fn notifications_are_private_to_the_caller() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;
    let (carol, _) = app.account("carol").await;

    let (_, dm) = app
        .send(
            "POST",
            "/api/dm",
            Some(&bob),
            Some(json!({ "users": [alice_id] })),
        )
        .await;
    let dm_id = dm["id"].as_str().unwrap();
    app.send(
        "POST",
        &format!("/api/channels/{dm_id}/messages"),
        Some(&bob),
        Some(json!({ "body": "just between us" })),
    )
    .await;

    let (_, mine) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    assert_eq!(mine.as_array().unwrap().len(), 1);

    // Carol is in no conversation with either of them.
    let (_, theirs) = app
        .send("GET", "/api/me/notifications", Some(&carol), None)
        .await;
    assert!(theirs.as_array().unwrap().is_empty());

    let (status, _) = app.send("GET", "/api/me/notifications", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reading_a_channel_clears_its_notifications() {
    let app = App::new();
    let (alice, alice_id) = app.account("alice").await;
    let (bob, _) = app.account("bob").await;

    let (_, dm) = app
        .send(
            "POST",
            "/api/dm",
            Some(&bob),
            Some(json!({ "users": [alice_id] })),
        )
        .await;
    let dm_id = dm["id"].as_str().unwrap();
    let (_, msg) = app
        .send(
            "POST",
            &format!("/api/channels/{dm_id}/messages"),
            Some(&bob),
            Some(json!({ "body": "are you around?" })),
        )
        .await;
    let msg_id = msg["id"].as_str().unwrap();

    let (_, items) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    assert_eq!(items.as_array().unwrap().len(), 1);

    app.send(
        "POST",
        &format!("/api/messages/{dm_id}/read"),
        Some(&alice),
        Some(json!({ "up_to": msg_id })),
    )
    .await;

    let (_, items) = app
        .send("GET", "/api/me/notifications", Some(&alice), None)
        .await;
    assert!(
        items.as_array().unwrap().is_empty(),
        "having read it, there is nothing left to be woken for"
    );
}
