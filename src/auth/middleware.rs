use std::sync::Arc;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::sync::RwLock;
use tracing::debug;

use super::session::SessionStore;

/// Extension type inserted into requests that have passed authentication.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub username: String,
}

/// Extract the value of the `session` cookie from the raw `Cookie` header, if present.
fn extract_session_cookie(req: &Request) -> Option<String> {
    let cookie_header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("session=") {
            return Some(value.to_string());
        }
    }
    None
}

/// Axum middleware that enforces session-based authentication.
///
/// Behaviour:
/// - Valid `session` cookie  → inserts `AuthUser` extension and forwards to next handler.
/// - Missing / invalid cookie + GET → 302 redirect to `/login`.
/// - Missing / invalid cookie + non-GET → 401 Unauthorized.
pub async fn auth_middleware(
    State(store): State<Arc<RwLock<SessionStore>>>,
    req: Request,
    next: Next,
) -> Response {
    let session_id = extract_session_cookie(&req);

    let auth_user = if let Some(ref sid) = session_id {
        let guard = store.read().await;
        guard
            .get_session(sid)
            .map(|s| AuthUser { username: s.username.clone() })
    } else {
        None
    };

    match auth_user {
        Some(user) => {
            debug!(username = %user.username, "Authenticated request");
            let (mut parts, body) = req.into_parts();
            parts.extensions.insert(user);
            let req = Request::from_parts(parts, body);
            next.run(req).await
        }
        None => {
            debug!("Unauthenticated request");
            if req.method() == Method::GET {
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, "/login")
                    .body(Body::empty())
                    .unwrap()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        middleware,
        routing::get,
        Router,
    };
    use tower::ServiceExt;

    /// A trivial handler that returns 200 OK.
    async fn ok_handler() -> StatusCode {
        StatusCode::OK
    }

    fn build_app(store: Arc<RwLock<SessionStore>>) -> Router {
        Router::new()
            .route("/protected", get(ok_handler).post(ok_handler))
            .layer(middleware::from_fn_with_state(store.clone(), auth_middleware))
            .with_state(store)
    }

    #[tokio::test]
    async fn test_request_without_cookie_redirects() {
        let store = Arc::new(RwLock::new(SessionStore::new()));
        let app = build_app(store);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/protected")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
    }

    #[tokio::test]
    async fn test_request_with_valid_session_passes() {
        let store = Arc::new(RwLock::new(SessionStore::new()));
        let session_id = {
            let mut guard = store.write().await;
            guard.create_session("alice", 3600)
        };

        let app = build_app(store);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/protected")
            .header(header::COOKIE, format!("session={session_id}"))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_request_with_invalid_session_redirects() {
        let store = Arc::new(RwLock::new(SessionStore::new()));
        let app = build_app(store);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/protected")
            .header(header::COOKIE, "session=bogus-session-id")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/login");
    }

    #[tokio::test]
    async fn test_post_without_session_returns_401() {
        let store = Arc::new(RwLock::new(SessionStore::new()));
        let app = build_app(store);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/protected")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
