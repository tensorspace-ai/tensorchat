//! The HTTP API.
//!
//! Realtime traffic goes over the WebSocket; this surface covers everything
//! that is request/response shaped: authentication, history backfill, search,
//! uploads, and administration. It is also the whole API a bot needs — the
//! WebSocket is an optimization for interactive clients, not a requirement.
//!
//! Responses are JSON (IDs as strings, see `tc_core::id`). The realtime path
//! uses MessagePack because it is the one that runs thousands of times a
//! second; a login does not need those bytes back.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tc_core::text::{self};
use tc_core::{Channel, ChannelKind, Id, Message, SearchHit, User, now_ms};
use tc_store::SearchQuery;

use crate::auth;
use crate::error::{ApiError, ApiResult};
use crate::service;
use crate::state::{Auth, Shared};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me).patch(update_me))
        .route("/api/me/password", post(change_password))
        .route("/api/me/sessions", delete(revoke_other_sessions))
        .route("/api/users", get(list_users))
        .route("/api/channels", get(my_channels).post(create_channel))
        .route("/api/channels/browse", get(browse_channels))
        .route("/api/channels/{id}", patch(update_channel))
        .route("/api/channels/{id}/join", post(join))
        .route("/api/channels/{id}/leave", post(leave))
        .route(
            "/api/channels/{id}/members",
            get(channel_members).post(add_members),
        )
        .route("/api/channels/{id}/members/{user}", delete(remove_member))
        .route(
            "/api/channels/{id}/messages",
            get(history).post(post_message),
        )
        .route(
            "/api/messages/{id}",
            patch(edit_message).delete(delete_message),
        )
        .route("/api/messages/{id}/reactions", post(react))
        .route("/api/messages/{id}/read", post(mark_read))
        .route("/api/threads/{id}", get(thread))
        .route("/api/dm", post(open_dm))
        .route("/api/search", get(search))
        .route("/api/uploads", post(upload))
        .route("/api/files/{id}", get(download))
        .route("/api/metrics", get(metrics))
        .route("/healthz", get(health))
}

// ---------------------------------------------------------------- auth

#[derive(Deserialize)]
pub struct RegisterReq {
    handle: String,
    display_name: Option<String>,
    password: String,
}

#[derive(Serialize)]
pub struct SessionRes {
    token: String,
    user: User,
}

/// Set the session cookie alongside the JSON body.
///
/// `HttpOnly` keeps the token away from any script on the page, which is the
/// entire mitigation for a stolen session via XSS. `SameSite=Lax` blocks
/// cross-site use while still allowing normal top-level navigation.
fn session_response(token: String, user: User, secure: bool) -> Response {
    let mut cookie = format!(
        "tc_session={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        auth::SESSION_TTL_MS / 1000
    );
    if secure {
        cookie.push_str("; Secure");
    }
    let mut headers = HeaderMap::new();
    if let Ok(v) = header::HeaderValue::from_str(&cookie) {
        headers.insert(header::SET_COOKIE, v);
    }
    (headers, Json(SessionRes { token, user })).into_response()
}

async fn register(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<RegisterReq>,
) -> ApiResult<Response> {
    if !st.cfg.open_registration {
        return Err(ApiError::Forbidden);
    }
    if !st.login_limiter.allow(peer.ip()) {
        return Err(ApiError::RateLimited);
    }

    let handle = text::normalize_handle(&req.handle).into_owned();
    text::validate_handle(&handle).map_err(|e| ApiError::BadRequest(e.into()))?;

    let display = req.display_name.unwrap_or_else(|| handle.clone());
    if display.trim().is_empty() || display.chars().count() > text::MAX_DISPLAY_NAME_LEN {
        return Err(ApiError::BadRequest("invalid display name".into()));
    }

    // Hash before touching the database: an expensive KDF on a request that was
    // going to fail validation anyway is wasted work.
    let phc = auth::hash_password(&req.password)?;
    let id = st.next_id();
    let (h, d) = (handle.clone(), display.clone());
    let user = st.db(move |s| s.create_user(id, &h, &d, &phc)).await?;

    let token = auth::new_session_token();
    let hash = token.hash;
    let now = now_ms();
    st.db(move |s| s.create_session(&hash, id, now, now + auth::SESSION_TTL_MS))
        .await?;

    Ok(session_response(token.secret, user, is_secure(&st)))
}

#[derive(Deserialize)]
pub struct LoginReq {
    handle: String,
    password: String,
}

async fn login(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> ApiResult<Response> {
    if !st.login_limiter.allow(peer.ip()) {
        return Err(ApiError::RateLimited);
    }
    let handle = text::normalize_handle(&req.handle).into_owned();
    let found = st.db(move |s| s.user_for_login(&handle)).await?;

    let Some((user, phc)) = found else {
        // Spend the same time as a real verification, so response latency does
        // not reveal whether the account exists.
        auth::equalize_timing(&req.password);
        return Err(ApiError::Unauthorized);
    };
    auth::verify_password(&req.password, &phc)?;

    let token = auth::new_session_token();
    let (hash, id, now) = (token.hash, user.id, now_ms());
    st.db(move |s| s.create_session(&hash, id, now, now + auth::SESSION_TTL_MS))
        .await?;

    Ok(session_response(token.secret, user, is_secure(&st)))
}

/// Whether to mark cookies `Secure`. Off for loopback so that plain-HTTP local
/// development still works; on everywhere else.
fn is_secure(st: &Shared) -> bool {
    !st.cfg.bind.ip().is_loopback()
}

async fn logout(State(st): State<Shared>, headers: HeaderMap) -> ApiResult<StatusCode> {
    // Revoke by the presented token; a session is server-side state, so this
    // takes effect immediately for every client holding it.
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get(header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .and_then(|c| crate::state::cookie_value(c, "tc_session"))
        });

    if let Some(t) = token {
        let hash = auth::token_hash(&t);
        st.db(move |s| s.delete_session(&hash)).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    current_password: String,
    new_password: String,
}

/// Change a password and sign every *other* device out.
///
/// Revoking the rest is the point, not a courtesy: the usual reason to change a
/// password is that the old one may be known to someone else, and sessions here
/// are bearer tokens that outlive it. Without this, an attacker holding a stolen
/// session would keep it for the full month of its TTL.
async fn change_password(
    State(st): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Auth(user): Auth,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult<Json<RevokedRes>> {
    // Rate-limited despite being authenticated: this endpoint verifies the
    // current password, so an attacker holding a stolen session could otherwise
    // use it to brute-force the password itself.
    if !st.login_limiter.allow(peer.ip()) {
        return Err(ApiError::RateLimited);
    }

    let id = user.id;
    let stored = st.db(move |s| s.password_hash(id)).await?;
    // Deliberately *not* the 401 that `AuthError` maps to by default: the
    // session is perfectly valid, only the re-authentication failed. A 401 here
    // would make clients treat a mistyped password as an expired session and
    // bounce the user to the login screen.
    auth::verify_password(&req.current_password, &stored)
        .map_err(|_| ApiError::BadRequest("current password is incorrect".into()))?;
    let phc = auth::hash_password(&req.new_password)?;

    st.db(move |s| s.update_password(id, &phc)).await?;

    // Spare the caller's own session, so changing a password does not bounce
    // the tab that just changed it back to the login screen.
    let keep = crate::state::bearer_token(&headers).map(|t| auth::token_hash(&t));
    let revoked = st
        .db(move |s| s.delete_sessions_for_user(id, keep.as_ref().map(|h| &h[..])))
        .await?;
    Ok(Json(RevokedRes { revoked }))
}

#[derive(Serialize)]
pub struct RevokedRes {
    /// How many other sessions were signed out.
    revoked: usize,
}

/// Sign out every device except this one.
async fn revoke_other_sessions(
    State(st): State<Shared>,
    Auth(user): Auth,
    headers: HeaderMap,
) -> ApiResult<Json<RevokedRes>> {
    let id = user.id;
    let keep = crate::state::bearer_token(&headers).map(|t| auth::token_hash(&t));
    let revoked = st
        .db(move |s| s.delete_sessions_for_user(id, keep.as_ref().map(|h| &h[..])))
        .await?;
    Ok(Json(RevokedRes { revoked }))
}

// ---------------------------------------------------------------- profile

async fn me(Auth(user): Auth) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
pub struct UpdateMeReq {
    display_name: Option<String>,
    status: Option<String>,
}

async fn update_me(
    State(st): State<Shared>,
    Auth(user): Auth,
    Json(req): Json<UpdateMeReq>,
) -> ApiResult<Json<User>> {
    let display = req
        .display_name
        .unwrap_or_else(|| user.display_name.clone());
    let status = req.status.unwrap_or_else(|| user.status.clone());
    if display.trim().is_empty() || display.chars().count() > text::MAX_DISPLAY_NAME_LEN {
        return Err(ApiError::BadRequest("invalid display name".into()));
    }
    if status.chars().count() > text::MAX_STATUS_LEN {
        return Err(ApiError::BadRequest("status is too long".into()));
    }

    let id = user.id;
    let updated = st
        .db(move |s| s.update_profile(id, &display, &status))
        .await?;
    // Everyone renders this user's name; tell every connected client.
    for u in st.hub.online_users() {
        st.hub.send_to_user(
            u,
            &tc_core::ServerFrame::UserUpd {
                user: updated.clone(),
            },
        );
    }
    Ok(Json(updated))
}

async fn list_users(State(st): State<Shared>, Auth(_): Auth) -> ApiResult<Json<Vec<User>>> {
    Ok(Json(st.db(|s| s.all_users()).await?))
}

// ---------------------------------------------------------------- channels

async fn my_channels(State(st): State<Shared>, Auth(u): Auth) -> ApiResult<Json<Vec<Channel>>> {
    let id = u.id;
    Ok(Json(st.db(move |s| s.channels_for_user(id)).await?))
}

async fn browse_channels(State(st): State<Shared>, Auth(_): Auth) -> ApiResult<Json<Vec<Channel>>> {
    Ok(Json(st.db(|s| s.public_channels()).await?))
}

#[derive(Deserialize)]
pub struct CreateChannelReq {
    name: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    members: Vec<Id>,
}

async fn create_channel(
    State(st): State<Shared>,
    Auth(u): Auth,
    Json(req): Json<CreateChannelReq>,
) -> ApiResult<Json<Channel>> {
    let kind = if req.private {
        ChannelKind::Private
    } else {
        ChannelKind::Public
    };
    Ok(Json(
        service::create_channel(&st, &u, &req.name, kind, &req.topic, req.members).await?,
    ))
}

#[derive(Deserialize)]
pub struct UpdateChannelReq {
    name: Option<String>,
    topic: Option<String>,
    archived: Option<bool>,
}

async fn update_channel(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<UpdateChannelReq>,
) -> ApiResult<Json<Channel>> {
    // Only members may reconfigure a channel.
    let uid = u.id;
    if !st.db(move |s| s.is_member(id, uid)).await? {
        return Err(ApiError::Forbidden);
    }
    if let Some(n) = &req.name {
        text::validate_channel_name(n).map_err(|e| ApiError::BadRequest(e.into()))?;
    }
    if req
        .topic
        .as_ref()
        .is_some_and(|t| t.len() > text::MAX_TOPIC_LEN)
    {
        return Err(ApiError::BadRequest("topic is too long".into()));
    }

    let (n, t, a) = (req.name, req.topic, req.archived);
    let channel = st
        .db(move |s| s.update_channel(id, n.as_deref(), t.as_deref(), a))
        .await?;
    st.hub.broadcast_frame(
        id,
        &tc_core::ServerFrame::Chan {
            channel: channel.clone(),
        },
    );
    Ok(Json(channel))
}

async fn join(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<Json<Channel>> {
    Ok(Json(service::join_channel(&st, &u, id).await?))
}

async fn leave(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    service::leave_channel(&st, &u, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn channel_members(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<Json<Vec<Id>>> {
    let uid = u.id;
    if !st.db(move |s| s.is_member(id, uid)).await? {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(st.db(move |s| s.members(id)).await?))
}

#[derive(Deserialize)]
pub struct AddMembersReq {
    users: Vec<Id>,
}

#[derive(Serialize)]
pub struct AddMembersRes {
    /// Only the ids this call actually added. Anyone already in the channel is
    /// absent, so the caller can report "3 added" honestly.
    added: Vec<Id>,
}

async fn add_members(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<AddMembersReq>,
) -> ApiResult<Json<AddMembersRes>> {
    let added = service::add_members(&st, &u, id, req.users).await?;
    Ok(Json(AddMembersRes { added }))
}

async fn remove_member(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path((id, user)): Path<(Id, Id)>,
) -> ApiResult<StatusCode> {
    service::remove_member(&st, &u, id, user).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct OpenDmReq {
    users: Vec<Id>,
}

async fn open_dm(
    State(st): State<Shared>,
    Auth(u): Auth,
    Json(req): Json<OpenDmReq>,
) -> ApiResult<Json<Channel>> {
    Ok(Json(service::open_dm(&st, &u, req.users).await?))
}

// ---------------------------------------------------------------- messages

#[derive(Deserialize)]
pub struct HistoryQuery {
    /// Exclusive cursor: return messages older than this id.
    before: Option<Id>,
    limit: Option<u32>,
}

#[derive(Serialize)]
pub struct HistoryRes {
    messages: Vec<Message>,
    /// `null` once the channel has been read back to its first message.
    next_cursor: Option<Id>,
}

async fn history(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<HistoryRes>> {
    let (uid, limit) = (u.id, q.limit.unwrap_or(50));
    let page = st.db(move |s| s.history(id, uid, q.before, limit)).await?;
    Ok(Json(HistoryRes {
        messages: page.messages,
        next_cursor: page.next_cursor,
    }))
}

#[derive(Deserialize)]
pub struct PostMessageReq {
    body: String,
    #[serde(default)]
    thread_root: Option<Id>,
    #[serde(default)]
    attachments: Vec<Id>,
}

async fn post_message(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<PostMessageReq>,
) -> ApiResult<Json<Message>> {
    Ok(Json(
        service::post_message(&st, &u, id, &req.body, req.thread_root, req.attachments).await?,
    ))
}

#[derive(Deserialize)]
pub struct EditReq {
    body: String,
}

async fn edit_message(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<EditReq>,
) -> ApiResult<Json<Message>> {
    Ok(Json(service::edit_message(&st, u.id, id, &req.body).await?))
}

async fn delete_message(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    service::delete_message(&st, u.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ReactReq {
    emoji: String,
    #[serde(default = "yes")]
    on: bool,
}

fn yes() -> bool {
    true
}

async fn react(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<ReactReq>,
) -> ApiResult<StatusCode> {
    service::set_reaction(&st, u.id, id, &req.emoji, req.on).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct MarkReadReq {
    up_to: Id,
}

async fn mark_read(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
    Json(req): Json<MarkReadReq>,
) -> ApiResult<Json<tc_core::ReadState>> {
    // `id` here is the channel, matching the realtime frame's shape.
    Ok(Json(service::mark_read(&st, u.id, id, req.up_to).await?))
}

async fn thread(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<Json<Vec<Message>>> {
    let uid = u.id;
    Ok(Json(st.db(move |s| s.thread(id, uid)).await?))
}

// ---------------------------------------------------------------- search

#[derive(Deserialize)]
pub struct SearchParams {
    q: String,
    channel: Option<Id>,
    author: Option<Id>,
    limit: Option<u32>,
}

async fn search(
    State(st): State<Shared>,
    Auth(u): Auth,
    Query(p): Query<SearchParams>,
) -> ApiResult<Json<Vec<SearchHit>>> {
    let uid = u.id;
    let hits = st
        .db(move |s| {
            s.search(
                uid,
                SearchQuery {
                    text: &p.q,
                    channel: p.channel,
                    author: p.author,
                    limit: p.limit.unwrap_or(25),
                },
            )
        })
        .await?;
    Ok(Json(hits))
}

// ---------------------------------------------------------------- files

/// Accept an upload and stage it. It becomes visible to others only once a
/// message references it.
async fn upload(
    State(st): State<Shared>,
    Auth(u): Auth,
    mut form: Multipart,
) -> ApiResult<Json<tc_core::Attachment>> {
    let field = form
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("no file in request".into()))?;

    let name = sanitize_filename(field.file_name().unwrap_or("file"));
    // Never trust a client-declared content type for anything but display, and
    // never echo it back in a way that lets the browser execute it — see
    // `download` below.
    let mime = field
        .content_type()
        .unwrap_or("application/octet-stream")
        .to_string();

    let data = field.bytes().await.map_err(|_| ApiError::TooLarge)?;
    if data.is_empty() {
        return Err(ApiError::BadRequest("file is empty".into()));
    }
    if data.len() > st.cfg.max_upload_bytes {
        return Err(ApiError::TooLarge);
    }

    let id = st.next_id();
    // Store under the ID, not the user's filename: the original name is
    // metadata, and letting it reach the filesystem invites path traversal.
    let rel = format!("{id}");
    let path = st.cfg.blob_dir.join(&rel);
    let dims = image_dimensions(&data);

    tokio::fs::write(&path, &data)
        .await
        .map_err(|e| ApiError::Internal(format!("writing blob: {e}")))?;

    let (n, m, sz) = (name, mime, data.len() as u64);
    let owner = u.id;
    let rel2 = rel.clone();
    let att = st
        .db(move |s| s.create_attachment(id, owner, &n, &m, sz, dims, &rel2))
        .await;

    match att {
        Ok(a) => Ok(Json(a)),
        Err(e) => {
            // The row is the source of truth; an orphaned blob would leak disk.
            let _ = tokio::fs::remove_file(&path).await;
            Err(e)
        }
    }
}

async fn download(
    State(st): State<Shared>,
    Auth(u): Auth,
    Path(id): Path<Id>,
) -> ApiResult<Response> {
    let uid = u.id;
    let (rel, name, mime) = st.db(move |s| s.attachment_path(id, uid)).await?;

    // `rel` was generated by us as a decimal id, but re-validate: this is the
    // one place a database value becomes a filesystem path.
    if rel.contains(['/', '\\', '.']) {
        return Err(ApiError::Internal("malformed blob path".into()));
    }
    let path = st.cfg.blob_dir.join(&rel);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let len = file
        .metadata()
        .await
        .map(|m| m.len())
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Stream rather than buffer: a 25 MB upload should not become 25 MB of
    // server memory per concurrent download.
    let stream = tokio_util::io::ReaderStream::new(file);
    let body = axum::body::Body::from_stream(stream);

    let mut headers = HeaderMap::new();
    // Only a short allowlist of types is served with its real content type.
    // Everything else is `application/octet-stream` and forced to download, so
    // an uploaded `.html` or `.svg` can never execute script on this origin.
    let safe_inline = matches!(
        mime.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "audio/mpeg"
            | "audio/ogg"
            | "video/mp4"
            | "video/webm"
            | "text/plain"
            | "application/pdf"
    );
    let (ct, disposition) = if safe_inline {
        (mime.as_str(), "inline")
    } else {
        ("application/octet-stream", "attachment")
    };

    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(ct).unwrap(),
    );
    headers.insert(header::CONTENT_LENGTH, len.into());
    // Belt and braces: even for inline types, forbid sniffing to something
    // executable.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if let Ok(v) = header::HeaderValue::from_str(&format!(
        "{disposition}; filename=\"{}\"",
        name.replace('"', "")
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    // Blobs are immutable — their id never points at different bytes.
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, max-age=31536000, immutable"),
    );

    Ok((headers, body).into_response())
}

/// Reduce a client-supplied filename to something safe to store and echo.
fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("file");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(120)
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Read pixel dimensions from the file header for the formats we can decode
/// cheaply, so the client can reserve layout space and avoid scroll jump.
///
/// Header parsing only — decoding whole images on the request path would be a
/// denial-of-service surface, and we do not need the pixels.
fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: 8-byte signature, then an IHDR chunk whose payload starts at 16.
    if data.len() >= 24 && data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    // GIF: dimensions are little-endian at offset 6.
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([data[6], data[7]]) as u32;
        let h = u16::from_le_bytes([data[8], data[9]]) as u32;
        return (w > 0 && h > 0).then_some((w, h));
    }
    // JPEG: walk the segment chain to a SOFn frame header.
    if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2usize;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            // SOF0..SOF15, excluding the non-frame markers in that range.
            if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return (w > 0 && h > 0).then_some((w, h));
            }
            let seg = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            if seg < 2 {
                break;
            }
            i += 2 + seg;
        }
    }
    None
}

// ---------------------------------------------------------------- ops

#[derive(Serialize)]
pub struct Metrics {
    uptime_seconds: u64,
    connections: usize,
    online_users: usize,
    frames_encoded: u64,
    frames_delivered: u64,
    bytes_encoded: u64,
    dropped_slow_consumers: u64,
    /// Average number of sockets each encoded frame was delivered to. This is
    /// the fanout amplification factor — the higher it is, the more work the
    /// encode-once design is saving.
    fanout_ratio: f64,
}

async fn metrics(State(st): State<Shared>, Auth(_): Auth) -> Json<Metrics> {
    use std::sync::atomic::Ordering::Relaxed;
    let m = st.hub.metrics();
    let encoded = m.frames_encoded.load(Relaxed);
    let delivered = m.frames_delivered.load(Relaxed);
    Json(Metrics {
        uptime_seconds: st.started.elapsed().as_secs(),
        connections: st.hub.connection_count(),
        online_users: st.hub.user_count(),
        frames_encoded: encoded,
        frames_delivered: delivered,
        bytes_encoded: m.bytes_encoded.load(Relaxed),
        dropped_slow_consumers: m.dropped_slow.load(Relaxed),
        fanout_ratio: if encoded == 0 {
            0.0
        } else {
            delivered as f64 / encoded as f64
        },
    })
}

/// Unauthenticated liveness probe.
async fn health() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_stripped_of_paths_and_control_characters() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Windows\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("ok\u{0}name.txt"), "okname.txt");
        assert_eq!(sanitize_filename(r#"quo"te.txt"#), "quote.txt");
        assert_eq!(sanitize_filename("   "), "file");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename(""), "file");
        assert!(sanitize_filename(&"x".repeat(500)).len() <= 120);
        // Unicode names survive intact.
        assert_eq!(sanitize_filename("プレゼン.pdf"), "プレゼン.pdf");
    }

    #[test]
    fn reads_png_dimensions_from_the_header() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&800u32.to_be_bytes());
        png.extend_from_slice(&600u32.to_be_bytes());
        assert_eq!(image_dimensions(&png), Some((800, 600)));
    }

    #[test]
    fn reads_gif_dimensions() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        assert_eq!(image_dimensions(&gif), Some((320, 240)));
    }

    #[test]
    fn reads_jpeg_dimensions_after_skipping_segments() {
        // SOI, an APP0 segment to skip, then SOF0 with 100x50.
        let mut jpg = vec![0xFF, 0xD8];
        jpg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x06, 1, 2, 3, 4]);
        jpg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        jpg.extend_from_slice(&50u16.to_be_bytes());
        jpg.extend_from_slice(&100u16.to_be_bytes());
        jpg.extend_from_slice(&[0; 8]);
        assert_eq!(image_dimensions(&jpg), Some((100, 50)));
    }

    #[test]
    fn unknown_or_truncated_data_yields_no_dimensions() {
        assert_eq!(image_dimensions(b""), None);
        assert_eq!(image_dimensions(b"not an image at all"), None);
        // A PNG signature with a truncated header must not panic.
        assert_eq!(
            image_dimensions(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0]),
            None
        );
        // A JPEG whose segment lengths run off the end must terminate.
        assert_eq!(
            image_dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF, 1, 2]),
            None
        );
    }
}
