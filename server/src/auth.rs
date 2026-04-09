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

/// Extract the session token from the request.
///
/// Checks (in order):
/// 1. `Authorization: Bearer <token>` header – the primary mechanism used
///    by the SPA client.  Immune to cookie-stripping by SSL-inspecting
///    proxies (e.g. Netskope).
/// 2. `Cookie: session=<token>` – backward-compatible fallback.
/// 3. `token` query parameter – used for WebSocket upgrades where custom
///    headers cannot be set by the browser.
fn extract_session_token(req: &Request<Body>) -> Option<String> {
    // 1. Authorization: Bearer <token>
    if let Some(auth_header) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_header.to_str() {
            if let Some(token) = auth_str.strip_prefix("Bearer ") {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }

    // 2. Cookie: session=<token>
    if let Some(cookie_header) = req.headers().get(header::COOKIE) {
        if let Ok(cookie_str) = cookie_header.to_str() {
            for cookie in cookie_str.split(';') {
                let cookie = cookie.trim();
                if let Some(value) = cookie.strip_prefix("session=") {
                    let value = value.trim();
                    if !value.is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
        }
    }

    // 3. Query parameter: ?token=<token>
    if let Some(query) = req.uri().query() {
        for param in query.split('&') {
            if let Some(value) = param.strip_prefix("token=") {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
}

/// Axum middleware that enforces authentication.
///
/// Public routes (no session required):
/// - `/login`, `/api/login` – login page and login endpoint
/// - Any path ending in a static-asset extension (`.js`, `.wasm`, `.css`,
///   `.png`, `.ico`) – these are not sensitive; protecting them causes module
///   load failures when SSL-inspecting proxies (e.g. Netskope) strip cookies
///   from browser sub-resource requests.  The WebSocket endpoint enforces
///   auth separately, so an unauthenticated client gains nothing from reading
///   the JS/WASM source.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();

    // Allow unauthenticated access to the login page, login API, and the
    // application shell.  The root page (`/`) is just the HTML shell – the
    // client-side JavaScript verifies the session token via `/api/session`
    // and redirects to `/login` when it is missing or invalid.  Serving the
    // shell without a cookie avoids the infinite-redirect problem caused by
    // SSL-inspecting proxies (e.g. Netskope) that strip cookies.
    if path == "/login" || path == "/api/login" || path == "/" || path == "/index.html" {
        return next.run(req).await;
    }

    // Allow static assets without auth so that <script type="module"> loads
    // succeed even when a proxy strips the session cookie from sub-resource
    // requests.
    if path.ends_with(".js")
        || path.ends_with(".wasm")
        || path.ends_with(".css")
        || path.ends_with(".png")
        || path.ends_with(".ico")
    {
        return next.run(req).await;
    }

    // Check for a valid session token (Authorization header, cookie, or
    // query parameter).
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

/// Check whether the current request carries a valid session.
/// Returns `200 OK` when authenticated, `401 Unauthorized` when not.
/// The client JavaScript calls this on start-up so it can redirect to the
/// login page gracefully if the session has expired.
pub async fn session_check(
    State(auth): State<AuthState>,
    req: Request<Body>,
) -> Response {
    if let Some(token) = extract_session_token(&req) {
        if auth.is_valid_session(&token) {
            return StatusCode::OK.into_response();
        }
    }
    StatusCode::UNAUTHORIZED.into_response()
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
///
/// Returns a plain 200 response with `Set-Cookie` on success, or 401 on
/// failure.  The login page uses `fetch()` to call this endpoint and
/// navigates to `/` via JavaScript after a successful response.
///
/// This avoids a 303 redirect, which is important because SSL-inspecting
/// proxies (e.g. Netskope) may follow the redirect server-side and strip
/// the `Set-Cookie` header before the browser ever sees it.
pub async fn login_handler(
    State(auth): State<AuthState>,
    req: Request<Body>,
) -> Response {
    // Extract form data manually so we can also inspect request headers.
    let is_https = is_request_secure(&req);
    let form = match axum::Form::<LoginForm>::from_request(req, &()).await {
        Ok(Form(f)) => f,
        Err(_) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    match auth.login(&form.username, &form.password) {
        Some(token) => {
            let cookie = build_session_cookie(&token, 86400, is_https);
            // Return the token in the response body as well as the
            // Set-Cookie header.  SSL-inspecting proxies (e.g. Netskope)
            // may strip Set-Cookie from the response before it reaches the
            // browser.  The client JavaScript uses the body value to set the
            // cookie via `document.cookie` as a fallback.
            (StatusCode::OK, [(header::SET_COOKIE, cookie)], token).into_response()
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
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

    // Expire the cookie (for clients that still use cookies).
    let cookie = build_session_cookie("", 0, is_https);
    (StatusCode::OK, [(header::SET_COOKIE, cookie)]).into_response()
}

// --- Cookie helpers ---

/// Build a session cookie string.
/// The `Secure` flag is only set when the request arrived over HTTPS
/// (directly or via a reverse proxy such as Apache with `X-Forwarded-Proto`).
/// `SameSite=Lax` is used instead of `Strict` for broader compatibility
/// with SSL-inspecting proxies (e.g. Netskope).
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
