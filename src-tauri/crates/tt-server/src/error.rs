use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde::Serialize;
use thiserror::Error;
use tt_application::errors::ApplicationError;
use tt_domain::{errors::DomainError, models::upstream_failure::UpstreamFailure};

/// The browser invoke boundary retains the desktop command error wire format.
#[derive(Debug, Error, Serialize)]
pub enum CommandError {
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Conflict: {0}")]
    Conflict(String),
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Unauthorized: {0}")]
    Unauthorized(String),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    TooManyRequests(String),
    #[error("{0}")]
    UpstreamFailure(UpstreamFailure),
    #[error("Internal server error: {0}")]
    InternalServerError(String),
}

impl From<ApplicationError> for CommandError {
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::ValidationError(v) => Self::BadRequest(v),
            ApplicationError::Conflict(v) => Self::Conflict(v),
            ApplicationError::NotFound(v) => Self::NotFound(v),
            ApplicationError::Unauthorized(v) | ApplicationError::PermissionDenied(v) => {
                Self::Unauthorized(v)
            }
            ApplicationError::RateLimited(v) => Self::TooManyRequests(v),
            ApplicationError::Transient(v) | ApplicationError::InternalError(v) => {
                Self::InternalServerError(v)
            }
            ApplicationError::UpstreamFailure(v) => Self::UpstreamFailure(v),
            ApplicationError::Cancelled(v) => Self::Cancelled(v),
        }
    }
}
impl From<DomainError> for CommandError {
    fn from(error: DomainError) -> Self {
        ApplicationError::from(error).into()
    }
}
impl IntoResponse for CommandError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unauthorized(_) => StatusCode::FORBIDDEN,
            Self::Cancelled(_) => StatusCode::CONFLICT,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::UpstreamFailure(_) => StatusCode::BAD_GATEWAY,
            Self::InternalServerError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, axum::Json(self)).into_response()
    }
}
