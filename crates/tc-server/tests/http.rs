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
use tc_server::{AppState, Config, build_router};
use tower::ServiceExt;

/// A test harness holding the router and a synthetic client address.
struct App {
    router: Router,
}

impl App {
    fn new() -> App {
        Self::with_config(Config::default())
    }

    fn with_config(cfg: Config) -> App {
        let store = tc_store::Store::open_in_memory().expect("in-memory store");
        let st = Arc::new(AppState::new(cfg, store));
        App {
            router: build_router(st),
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
