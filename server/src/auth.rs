use crate::config::AuthConfig;
use axum::{
    body::Body,
    extract::{FromRequest, State},
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Shared authentication state.
#[derive(Clone)]
pub struct AuthState {
    /// Hashed username for constant-time comparison.
    username_hash: [u8; 32],
    /// Hashed password for constant-time comparison.
    password_hash: [u8; 32],
    /// Active session tokens.
    sessions: Arc<RwLock<HashSet<String>>>,
}

impl AuthState {
    pub fn new(auth_config: &AuthConfig) -> Self {
        Self {
            username_hash: Self::hash_str(&auth_config.username),
            password_hash: Self::hash_str(&auth_config.password),
            sessions: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    fn hash_str(s: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        hasher.finalize().into()
    }

    /// Validate credentials. Returns a session token on success.
    pub fn login(&self, username: &str, password: &str) -> Option<String> {
        let u_hash = Self::hash_str(username);
        let p_hash = Self::hash_str(password);

        // Constant-time comparison: always compare both to avoid timing leaks.
        let u_ok = u_hash == self.username_hash;
        let p_ok = p_hash == self.password_hash;
        if !(u_ok & p_ok) {
            return None;
        }

        let token = generate_token();
        if let Ok(mut sessions) = self.sessions.write() {
            sessions.insert(token.clone());
        }
        Some(token)
    }

    /// Check if a session token is valid.
    pub fn is_valid_session(&self, token: &str) -> bool {
        if let Ok(sessions) = self.sessions.read() {
            sessions.contains(token)
        } else {
            false
        }
    }

    /// Invalidate a session token.
    pub fn logout(&self, token: &str) {
        if let Ok(mut sessions) = self.sessions.write() {
            sessions.remove(token);
        }
    }
}

/// Generate a cryptographically random session token.
fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
    hex::encode(bytes)
}

/// Extract session token from cookies.
fn extract_session_token(req: &Request<Body>) -> Option<String> {
    let cookie_header = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for cookie in cookie_header.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("session=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Axum middleware that enforces authentication.
/// Allows access to /login and /api/login without a valid session.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    // Allow unauthenticated access to the login page and login API.
    if path == "/login" || path == "/api/login" || path == "/login.css" {
        return next.run(req).await;
    }

    // Check for valid session cookie.
    if let Some(token) = extract_session_token(&req) {
        if auth.is_valid_session(&token) {
            return next.run(req).await;
        }
    }

    // Not authenticated → redirect to login page (or 401 for API/WS).
    if path.starts_with("/api/") || path == "/ws" {
        StatusCode::UNAUTHORIZED.into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

// --- Login page ---

/// Serve the login page HTML.
pub async fn login_page() -> Html<&'static str> {
    Html(include_str!("../static_auth/login.html"))
}

/// Login form data.
#[derive(serde::Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

/// Handle login form submission.
pub async fn login_handler(
    State(auth): State<AuthState>,
    req: Request<Body>,
) -> Response {
    // Extract form data manually so we can also inspect request headers.
    let is_https = is_request_secure(&req);
    let form = match axum::Form::<LoginForm>::from_request(req, &()).await {
        Ok(Form(f)) => f,
        Err(_) => {
            return Html(include_str!("../static_auth/login_failed.html")).into_response();
        }
    };

    match auth.login(&form.username, &form.password) {
        Some(token) => {
            let cookie = build_session_cookie(&token, 86400, is_https);
            (
                StatusCode::SEE_OTHER,
                [
                    (header::SET_COOKIE, cookie),
                    (header::LOCATION, "/".to_string()),
                ],
            )
                .into_response()
        }
        None => {
            // Return login page with error message.
            Html(include_str!("../static_auth/login_failed.html")).into_response()
        }
    }
}

/// Handle logout.
pub async fn logout_handler(
    State(auth): State<AuthState>,
    req: Request<Body>,
) -> Response {
    let is_https = is_request_secure(&req);
    if let Some(token) = extract_session_token(&req) {
        auth.logout(&token);
    }

    let cookie = build_session_cookie("", 0, is_https);
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, cookie),
            (header::LOCATION, "/login".to_string()),
        ],
    )
        .into_response()
}

// --- Cookie helpers ---

/// Build a session cookie string.
/// The `Secure` flag is only set when the request arrived over HTTPS
/// (directly or via a reverse proxy such as Apache with `X-Forwarded-Proto`).
/// `SameSite=Lax` is used instead of `Strict` so that the cookie survives
/// the POST → 303 redirect after login, even when an SSL-inspecting proxy
/// (e.g. Netskope) is in the path.
fn build_session_cookie(token: &str, max_age: u64, secure: bool) -> String {
    let mut cookie = format!(
        "session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        token, max_age
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Detect whether the original client request was made over HTTPS.
/// Checks the `X-Forwarded-Proto` header (set by reverse proxies like Apache)
/// first, then falls back to the request URI scheme.
fn is_request_secure(req: &Request<Body>) -> bool {
    // Check reverse-proxy header first (Apache: `RequestHeader set X-Forwarded-Proto "https"`)
    if let Some(proto) = req.headers().get("x-forwarded-proto") {
        if let Ok(s) = proto.to_str() {
            return s.eq_ignore_ascii_case("https");
        }
    }
    // Fall back to the request URI scheme.
    req.uri()
        .scheme_str()
        .map(|s| s.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}
