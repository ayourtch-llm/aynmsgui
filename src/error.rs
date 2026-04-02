use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Not Found: {0}")]
    NotFound(String),

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Bad Request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::NotFound(msg) => {
                (StatusCode::NOT_FOUND, msg).into_response()
            }
            AppError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
            }
            AppError::BadRequest(msg) => {
                (StatusCode::BAD_REQUEST, msg).into_response()
            }
            AppError::Conflict(msg) => {
                (StatusCode::CONFLICT, msg).into_response()
            }
            AppError::Internal(err) => {
                tracing::error!("Internal server error: {err:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// Helper: convert AppError to a Response and return its status code.
    fn status_of(err: AppError) -> StatusCode {
        err.into_response().status()
    }

    #[test]
    fn test_not_found_returns_404() {
        assert_eq!(
            status_of(AppError::NotFound("device missing".to_string())),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn test_unauthorized_returns_401() {
        assert_eq!(status_of(AppError::Unauthorized), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_bad_request_returns_400() {
        assert_eq!(
            status_of(AppError::BadRequest("invalid input".to_string())),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_conflict_returns_409() {
        assert_eq!(
            status_of(AppError::Conflict("already exists".to_string())),
            StatusCode::CONFLICT
        );
    }

    #[test]
    fn test_internal_returns_500() {
        let err = anyhow::anyhow!("database exploded");
        assert_eq!(
            status_of(AppError::Internal(err)),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let app_err: AppError = anyhow_err.into();
        assert_eq!(status_of(app_err), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
