//! Typed errors with stable wire codes and HTTP status mapping.
//!
//! The code/status table is part of the public HTTP contract and mirrors the
//! reference implementation so the SDK and existing clients keep working.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidRequest,
    Unauthorized,
    BlockedDestination,
    NotFound,
    MethodNotAllowed,
    Conflict,
    Cancelled,
    PayloadTooLarge,
    UnsupportedContent,
    CompatibilityError,
    ElementNotFound,
    RateLimited,
    UnsupportedCapability,
    NavigationFailed,
    ResponseTooLarge,
    QueueFull,
    Overloaded,
    TooManySessions,
    ResourceLimit,
    ShuttingDown,
    NavigationTimeout,
    TaskTimeout,
    WaitTimeout,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            InvalidRequest => "invalid_request",
            Unauthorized => "unauthorized",
            BlockedDestination => "blocked_destination",
            NotFound => "not_found",
            MethodNotAllowed => "method_not_allowed",
            Conflict => "conflict",
            Cancelled => "cancelled",
            PayloadTooLarge => "payload_too_large",
            UnsupportedContent => "unsupported_content",
            CompatibilityError => "compatibility_error",
            ElementNotFound => "element_not_found",
            RateLimited => "rate_limited",
            UnsupportedCapability => "unsupported_capability",
            NavigationFailed => "navigation_failed",
            ResponseTooLarge => "response_too_large",
            QueueFull => "queue_full",
            Overloaded => "overloaded",
            TooManySessions => "too_many_sessions",
            ResourceLimit => "resource_limit",
            ShuttingDown => "shutting_down",
            NavigationTimeout => "navigation_timeout",
            TaskTimeout => "task_timeout",
            WaitTimeout => "wait_timeout",
            Internal => "internal",
        }
    }

    pub fn status(self) -> u16 {
        use ErrorCode::*;
        match self {
            InvalidRequest => 400,
            Unauthorized => 401,
            BlockedDestination => 403,
            NotFound => 404,
            MethodNotAllowed => 405,
            Conflict | Cancelled => 409,
            PayloadTooLarge => 413,
            UnsupportedContent => 415,
            CompatibilityError | ElementNotFound => 422,
            RateLimited => 429,
            UnsupportedCapability => 501,
            NavigationFailed | ResponseTooLarge => 502,
            QueueFull | Overloaded | TooManySessions | ResourceLimit | ShuttingDown => 503,
            NavigationTimeout | TaskTimeout | WaitTimeout => 504,
            Internal => 500,
        }
    }

    /// Whether a caller can reasonably retry the same request unchanged.
    pub fn default_retryable(self) -> bool {
        use ErrorCode::*;
        matches!(
            self,
            NavigationFailed
                | NavigationTimeout
                | QueueFull
                | Overloaded
                | TooManySessions
                | RateLimited
                | ShuttingDown
                | TaskTimeout
        )
    }
}

#[derive(Debug, Clone)]
pub struct ScoutError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    pub retry_after_sec: Option<u64>,
    pub details: Option<serde_json::Value>,
}

impl ScoutError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.default_retryable(),
            retry_after_sec: None,
            details: None,
        }
    }

    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_sec = Some(secs);
        self
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, msg)
    }
    pub fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "Missing or invalid API key")
    }
    pub fn blocked(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BlockedDestination, msg).with_retryable(false)
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::UnsupportedCapability, msg).with_retryable(false)
    }

    /// Wire representation: `{"error":{"code","message","retryable","details"?}}`
    pub fn to_json(&self) -> serde_json::Value {
        let mut err = serde_json::Map::new();
        err.insert("code".into(), self.code.as_str().into());
        err.insert("message".into(), self.message.clone().into());
        err.insert("retryable".into(), self.retryable.into());
        if let Some(details) = &self.details {
            err.insert("details".into(), details.clone());
        }
        serde_json::json!({ "error": err })
    }
}

impl fmt::Display for ScoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ScoutError {}

impl From<serde_json::Error> for ScoutError {
    fn from(e: serde_json::Error) -> Self {
        ScoutError::new(ErrorCode::InvalidRequest, format!("Body is not valid JSON: {e}"))
    }
}

impl axum::response::IntoResponse for ScoutError {
    fn into_response(self) -> axum::response::Response {
        let e = self;
        let status =
            axum::http::StatusCode::from_u16(e.code.status()).unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let mut res = (status, axum::Json(e.to_json())).into_response();
        if let Some(secs) = e.retry_after_sec {
            if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        if e.code == ErrorCode::Unauthorized {
            res.headers_mut()
                .insert(axum::http::header::WWW_AUTHENTICATE, axum::http::HeaderValue::from_static("Bearer"));
        }
        res
    }
}

pub type Result<T> = std::result::Result<T, ScoutError>;
