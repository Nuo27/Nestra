use serde::{Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(String),
    #[error("internal: {0}")]
    Internal(String),
    /// Orchestration control-plane error: invalid policy, unknown task/run,
    /// capability resolution failure, etc. Never carries a credential.
    #[error("orchestration: {0}")]
    Orchestration(String),
    /// Upstream-provider failure observed by the gateway. Classified by the
    /// failure taxonomy (quota/rate-limit/5xx/timeout/auth/4xx) before it
    /// reaches here, so the attached string is the human-readable detail, not
    /// the raw upstream body (which may contain secrets/PII).
    #[error("upstream: {0}")]
    Upstream(String),
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("AppError", 2)?;
        let code = match self {
            AppError::NotFound(_) => "NotFound",
            AppError::Conflict(_) => "Conflict",
            AppError::Validation(_) => "Validation",
            AppError::Io(_) => "Io",
            AppError::Db(_) => "Db",
            AppError::Json(_) => "Json",
            AppError::Http(_) => "Http",
            AppError::Internal(_) => "Internal",
            AppError::Orchestration(_) => "Orchestration",
            AppError::Upstream(_) => "Upstream",
        };
        // Boundary redaction: user-facing variants (NotFound/Validation/…)
        // keep their full text, but low-level wrapped errors (io/db/json)
        // serialize as stable summaries — their Display strings can embed
        // absolute paths, SQL fragments, or data values that shouldn't cross
        // the IPC boundary. Full detail stays in the log via `Debug`/`Display`
        // (tracing captures the error before it reaches the UI).
        let message = match self {
            AppError::Io(e) => format!("io error ({})", e.kind()),
            AppError::Db(_) => "database error".to_string(),
            AppError::Json(_) => "data format error".to_string(),
            other => other.to_string(),
        };
        s.serialize_field("code", code)?;
        s.serialize_field("message", &message)?;
        s.end()
    }
}

pub type AppResult<T> = Result<T, AppError>;