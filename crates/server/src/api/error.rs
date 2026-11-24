//! API error type rendered as JSON.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use storage::StorageError;

/// An error response: `{"error": code, "message": ..., "details": ...}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({ "error": self.code, "message": self.message });
        if let Some(d) = self.details {
            body["details"] = d;
        }
        (self.status, Json(body)).into_response()
    }
}

impl From<StorageError> for ApiError {
    fn from(e: StorageError) -> Self {
        // Unreachable, or reachable but not set up yet (database missing:
        // 3D000, tables missing before migrations ran: 42P01).
        let not_ready = |e: &sqlx::Error| {
            e.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .is_some_and(|c| c == "3D000" || c == "42P01")
        };
        let unavailable = match &e {
            StorageError::Db(
                sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) | sqlx::Error::PoolClosed,
            )
            | StorageError::Migrate(_) => true,
            StorageError::Db(db) => not_ready(db),
            StorageError::Corrupt(_) => false,
        };
        if unavailable {
            tracing::warn!(error = %e, "database unavailable");
            Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "database is not reachable, try again later",
            )
        } else {
            tracing::error!(error = %e, "database error");
            Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal database error",
            )
        }
    }
}

/// Result type of handlers.
pub type ApiResult<T> = Result<T, ApiError>;
