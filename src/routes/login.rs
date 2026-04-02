use axum::{
    extract::{Form, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

static LOGIN_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><title>Login</title></head>
<body>
<h1>Login</h1>
<form method="POST" action="/login">
  <label>Username: <input type="text" name="username" required></label><br>
  <label>Password: <input type="password" name="password" required></label><br>
  <button type="submit">Login</button>
</form>
</body>
</html>"#;

fn login_page_with_error(error: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><title>Login</title></head>
<body>
<h1>Login</h1>
<p style="color:red">{error}</p>
<form method="POST" action="/login">
  <label>Username: <input type="text" name="username" required></label><br>
  <label>Password: <input type="password" name="password" required></label><br>
  <button type="submit">Login</button>
</form>
</body>
</html>"#
    )
}

pub async fn login_page() -> Html<&'static str> {
    Html(LOGIN_PAGE)
}

pub async fn login_submit(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Response {
    if state.htpasswd.verify(&form.username, &form.password) {
        debug!(username = %form.username, "Login successful");
        let ttl = state.config.session_ttl_secs;
        let session_id = {
            let mut sessions = state.sessions.write().await;
            sessions.create_session(&form.username, ttl)
        };
        let cookie = format!(
            "session={session_id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ttl}"
        );
        Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/")
            .header(header::SET_COOKIE, cookie)
            .body(axum::body::Body::empty())
            .unwrap()
    } else {
        warn!(username = %form.username, "Login failed");
        let html = login_page_with_error("Invalid username or password");
        (StatusCode::OK, Html(html)).into_response()
    }
}

pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Extract session cookie
    if let Some(cookie_val) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie_val.split(';') {
            let part = part.trim();
            if let Some(sid) = part.strip_prefix("session=") {
                let mut sessions = state.sessions.write().await;
                sessions.remove_session(sid);
                debug!(session_id = %sid, "Session removed on logout");
                break;
            }
        }
    }
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "/login")
        .header(
            header::SET_COOKIE,
            "session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login", get(login_page).post(login_submit))
        .route("/logout", post(logout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request},
    };
    use clap::Parser;
    use tower::ServiceExt;

    use crate::auth::htpasswd::HtpasswdStore;
    use crate::config::AppConfig;
    use crate::state::AppState;

    fn make_test_state(username: &str, password: &str) -> AppState {
        let hash = bcrypt::hash(password, 4).expect("bcrypt hash should not fail in tests");
        let htpasswd_content = format!("{username}:{hash}");
        let htpasswd = HtpasswdStore::from_str(&htpasswd_content);

        let config = AppConfig::try_parse_from([
            "aynmsgui",
            "--htpasswd-file",
            "/dev/null",
        ])
        .expect("test config parse");

        AppState::new(config, htpasswd, None, indexmap::IndexMap::new())
    }

    fn build_test_app(state: AppState) -> axum::Router {
        routes().with_state(state)
    }

    #[tokio::test]
    async fn test_login_page_returns_200() {
        let state = make_test_state("alice", "pass");
        let app = build_test_app(state);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/login")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains("<form"), "response should contain a form element");
    }

    #[tokio::test]
    async fn test_login_success_redirects() {
        let state = make_test_state("alice", "correct");
        let app = build_test_app(state);

        let body = "username=alice&password=correct";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/",
            "successful login should redirect to /"
        );
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie header should be present");
        let cookie_str = set_cookie.to_str().unwrap();
        assert!(
            cookie_str.starts_with("session="),
            "Set-Cookie should start with session="
        );
        assert!(
            cookie_str.contains("HttpOnly"),
            "cookie should be HttpOnly"
        );
    }

    #[tokio::test]
    async fn test_login_failure_returns_login_page() {
        let state = make_test_state("alice", "correct");
        let app = build_test_app(state);

        let body = "username=alice&password=wrong";
        let req = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(
            body.contains("<form"),
            "failed login should return login page with form"
        );
        assert!(
            body.contains("Invalid"),
            "failed login should show error message"
        );
    }

    #[tokio::test]
    async fn test_logout_clears_cookie() {
        let state = make_test_state("alice", "pass");
        // Create a session first
        let session_id = {
            let mut sessions = state.sessions.write().await;
            sessions.create_session("alice", 3600)
        };
        let app = build_test_app(state);

        let req = Request::builder()
            .method(Method::POST)
            .uri("/logout")
            .header(header::COOKIE, format!("session={session_id}"))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/login",
            "logout should redirect to /login"
        );

        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie should be present on logout");
        let cookie_str = set_cookie.to_str().unwrap();
        assert!(
            cookie_str.contains("Max-Age=0"),
            "logout cookie should have Max-Age=0"
        );
    }
}
