//! HTTP error mapping.
//!
//! One rule: an error that reaches a client says only what that client is
//! entitled to know. Internal failures log their detail and return a generic
//! message, so a stack of SQLite strings never ends up in a browser.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tensorchat_core::ErrCode;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("too many requests")]
    RateLimited,
    #[error("payload too large")]
    TooLarge,
    /// Anything unexpected. The inner detail is logged, never sent.
    #[error("internal error")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrBody<'a> {
    /// Stable machine-readable code, shared with the WebSocket protocol so a
    /// client has one error vocabulary rather than two.
    code: ErrCode,
    message: &'a str,
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ApiError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> ErrCode {
        match self {
            ApiError::Unauthorized => ErrCode::Unauthorized,
            ApiError::Forbidden => ErrCode::Forbidden,
            ApiError::NotFound => ErrCode::NotFound,
            ApiError::BadRequest(_) | ApiError::TooLarge => ErrCode::BadRequest,
            ApiError::Conflict(_) => ErrCode::BadRequest,
            ApiError::RateLimited => ErrCode::RateLimited,
            ApiError::Internal(_) => ErrCode::Internal,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            tracing::error!(detail, "internal error");
        }
        let message = match &self {
            // Deliberately generic: the detail is in the log, not the response.
            ApiError::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        };

        (
            self.status(),
            Json(ErrBody {
                code: self.code(),
                message: &message,
            }),
        )
            .into_response()
    }
}

/// Storage errors become HTTP errors without leaking SQL.
impl From<tensorchat_store::Error> for ApiError {
    fn from(e: tensorchat_store::Error) -> Self {
        use tensorchat_store::Error as E;
        match e {
            E::NotFound => ApiError::NotFound,
            E::Forbidden => ApiError::Forbidden,
            E::Conflict(what) => ApiError::Conflict(format!("{what} already exists")),
            E::Invalid(why) => ApiError::BadRequest(why.to_string()),
            // Driver-level failures are ours, not the caller's.
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<crate::auth::AuthError> for ApiError {
    fn from(e: crate::auth::AuthError) -> Self {
        use crate::auth::AuthError as A;
        match e {
            A::WeakPassword => ApiError::BadRequest(e.to_string()),
            // Wrong password and unknown account are the same answer, so the
            // endpoint cannot be used to enumerate handles.
            A::BadCredentials => ApiError::Unauthorized,
            A::Hashing => ApiError::Internal("hashing failed".into()),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(e: ApiError) -> (StatusCode, String) {
        let r = e.into_response();
        let status = r.status();
        let bytes = to_bytes(r.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn internal_errors_never_leak_their_detail() {
        let (status, body) = body_of(ApiError::Internal(
            "SQL: no such column: secret_stuff".into(),
        ))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains("secret_stuff"), "leaked internals: {body}");
        assert!(body.contains("internal error"));
    }

    #[tokio::test]
    async fn client_errors_keep_their_message_and_code() {
        let (status, body) = body_of(ApiError::BadRequest("handle is reserved".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("handle is reserved"));
        assert!(body.contains("bad_request"));
    }

    #[test]
    fn storage_errors_map_to_the_right_status() {
        assert_eq!(
            ApiError::from(tensorchat_store::Error::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(tensorchat_store::Error::Forbidden).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ApiError::from(tensorchat_store::Error::Conflict("that handle")).status(),
            StatusCode::CONFLICT
        );
        // An infrastructure failure must not be reported as the caller's fault.
        assert_eq!(
            ApiError::from(tensorchat_store::Error::SchemaTooNew {
                found: 9,
                supported: 1
            })
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn bad_credentials_are_indistinguishable_from_unknown_accounts() {
        let e = ApiError::from(crate::auth::AuthError::BadCredentials);
        assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(e.to_string(), "unauthorized");
    }
}
