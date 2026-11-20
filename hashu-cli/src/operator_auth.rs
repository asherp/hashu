//! Login, sessions and cross-site request checks for the operator GUI and
//! control API (`--control-bind`).
//!
//! - **Login.** With `--operator-password-hash` (a bcrypt hash, the one
//!   `caddy hash-password` prints), every route needs a session: a browser
//!   gets the login page, an API call gets 401. `POST /login` checks the
//!   user and password and sets an `HttpOnly`, `SameSite=Strict` session
//!   cookie; `POST /logout` ends it. Sessions live in memory, so a restart
//!   logs everyone out.
//! - **Brute force.** Each client (Caddy's `X-Forwarded-For`) gets
//!   [`MAX_FAILURES`] wrong passwords per [`FAILURE_WINDOW`], then 429
//!   until the window passes. bcrypt runs one check at a time on a
//!   blocking thread, so a flood of guesses holds at most one core.
//! - **CSRF.** A request that can change something (anything but
//!   GET/HEAD/OPTIONS) from a browser must come from this origin
//!   (`Sec-Fetch-Site`, else `Origin`). Clients that send neither (curl,
//!   the MDK app's server) pass. This applies with login off too, since a
//!   page in the operator's browser can reach `localhost` as well.
//! - **Public status.** `GET /status` stays open for the MDK app, but
//!   without a session it leaves out each redemption's pool and worker
//!   (the buyer's pool account).
//!
//! Every response also carries headers that keep the GUI out of frames
//! and caches.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::Form;
use serde_json::json;

/// Session cookie name.
pub const COOKIE: &str = "hashu_operator";
/// How long a login lasts.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Wrong passwords a client may send per [`FAILURE_WINDOW`].
pub const MAX_FAILURES: usize = 5;
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// A login waits at most this long for the bcrypt check ahead of it.
const CHECK_QUEUE: Duration = Duration::from_secs(10);
/// Longest user or password accepted (bcrypt reads 72 bytes anyway).
const MAX_FIELD: usize = 256;

const LOGIN_PAGE: &str = include_str!("operator-login.html");

/// Whether the request carried a valid session (or login is off).
#[derive(Clone, Copy, Debug)]
pub struct Authed(pub bool);

pub struct OperatorAuth {
    /// `None`: login is off (loopback or demo use); CSRF checks still run.
    login: Option<Login>,
}

struct Login {
    user: String,
    hash: String,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
    checking: tokio::sync::Semaphore,
}

/// Outcome of a password check.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginResult {
    Ok(String),
    Wrong,
    Limited,
    Busy,
}

impl OperatorAuth {
    /// Login off: every request is the operator's (CSRF checks still apply).
    pub fn open() -> Self {
        Self { login: None }
    }

    /// Login on, for `user` with bcrypt `hash`. Fails on a malformed hash,
    /// so a typo in .env stops the mint instead of locking the GUI.
    pub fn with_password_hash(user: &str, hash: &str) -> anyhow::Result<Self> {
        let hash = hash.trim().to_string();
        let user = user.trim().to_string();
        anyhow::ensure!(!user.is_empty(), "operator user must not be empty");
        // A malformed hash is an error; a well-formed one just says false.
        bcrypt::verify("", &hash).map_err(|e| {
            anyhow::anyhow!("operator password hash is not a bcrypt hash ({e}); make one with `caddy hash-password`")
        })?;
        Ok(Self {
            login: Some(Login {
                user,
                hash,
                sessions: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                checking: tokio::sync::Semaphore::new(1),
            }),
        })
    }

    pub fn login_enabled(&self) -> bool {
        self.login.is_some()
    }

    /// Whether `headers` carry a live session (always true with login off).
    pub fn authed(&self, headers: &HeaderMap) -> bool {
        let Some(l) = &self.login else {
            return true;
        };
        let Some(token) = cookie(headers, COOKIE) else {
            return false;
        };
        let now = Instant::now();
        let mut s = l.sessions.lock().unwrap_or_else(|e| e.into_inner());
        s.retain(|_, exp| *exp > now);
        s.contains_key(token)
    }

    /// Check `user`/`password` for `client`; on success, a new session token.
    pub async fn login(&self, client: &str, user: &str, password: &str) -> LoginResult {
        let Some(l) = &self.login else {
            return LoginResult::Wrong;
        };
        if l.recent_failures(client) >= MAX_FAILURES {
            return LoginResult::Limited;
        }
        let Ok(Ok(_permit)) = tokio::time::timeout(CHECK_QUEUE, l.checking.acquire()).await else {
            return LoginResult::Busy;
        };
        // Run bcrypt whatever the user, so timing says nothing about it.
        let (hash, pw) = (l.hash.clone(), password.to_string());
        let pw_ok = tokio::task::spawn_blocking(move || bcrypt::verify(pw, &hash).unwrap_or(false))
            .await
            .unwrap_or(false);
        let user_ok = constant_time_eq(user.trim().as_bytes(), l.user.as_bytes());
        if pw_ok && user_ok {
            let token = {
                let mut b = [0u8; 32];
                rand::Rng::fill(&mut rand::thread_rng(), &mut b[..]);
                hex::encode(b)
            };
            l.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(token.clone(), Instant::now() + SESSION_TTL);
            LoginResult::Ok(token)
        } else {
            l.fail(client);
            LoginResult::Wrong
        }
    }

    pub fn logout(&self, headers: &HeaderMap) {
        if let (Some(l), Some(token)) = (&self.login, cookie(headers, COOKIE)) {
            l.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(token);
        }
    }
}

impl Login {
    fn recent_failures(&self, client: &str) -> usize {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now().checked_sub(FAILURE_WINDOW);
        f.retain(|_, q| {
            while q.front().is_some_and(|t| cutoff.is_some_and(|c| *t < c)) {
                q.pop_front();
            }
            !q.is_empty()
        });
        f.get(client).map_or(0, VecDeque::len)
    }

    fn fail(&self, client: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(client.to_string())
            .or_default()
            .push_back(Instant::now());
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Value of cookie `name`.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name && !v.is_empty()).then_some(v)
        })
}

/// Whether a state-changing request may proceed: from this origin, or from
/// a client that is not a browser (no `Sec-Fetch-Site`, no `Origin`).
pub fn same_origin(headers: &HeaderMap) -> bool {
    let get = |h: &str| headers.get(h).and_then(|v| v.to_str().ok());
    if let Some(site) = get("sec-fetch-site") {
        // "none": typed in the address bar or a bookmark.
        return matches!(site, "same-origin" | "none");
    }
    let Some(origin) = get("origin") else {
        return true;
    };
    let host = get("x-forwarded-host").or_else(|| get("host"));
    let origin_host = origin
        .split_once("://")
        .map(|(_, h)| h)
        .unwrap_or(origin)
        .trim_end_matches('/');
    host.is_some_and(|h| h.eq_ignore_ascii_case(origin_host))
}

/// The client as Caddy reports it (it overwrites `X-Forwarded-For`).
fn client_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

#[derive(serde::Deserialize)]
struct LoginForm {
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
}

fn login_page(status: StatusCode, msg: &str) -> Response {
    let note = if msg.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="err" role="alert">{msg}</p>"#)
    };
    (status, Html(LOGIN_PAGE.replace("<!--NOTE-->", &note))).into_response()
}

/// Path prefix Caddy mounts the GUI under (`X-Forwarded-Prefix`), for the
/// cookie's `Path`.
fn prefix(headers: &HeaderMap) -> String {
    let p = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim_end_matches('/');
    if p.starts_with('/')
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_".contains(&b))
    {
        p.to_string()
    } else {
        String::new()
    }
}

fn session_cookie(headers: &HeaderMap, value: &str, max_age: u64) -> String {
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    format!(
        "{COOKIE}={value}; Path={}/; Max-Age={max_age}; HttpOnly; SameSite=Strict{}",
        prefix(headers),
        if secure { "; Secure" } else { "" }
    )
}

async fn handle_login(auth: &OperatorAuth, req: Request) -> Response {
    let headers = req.headers().clone();
    let Ok(Form(f)) = Form::<LoginForm>::from_request(req, &()).await else {
        return login_page(StatusCode::BAD_REQUEST, "Enter the user and password.");
    };
    if f.user.len() > MAX_FIELD || f.password.len() > MAX_FIELD {
        return login_page(StatusCode::BAD_REQUEST, "Wrong user or password.");
    }
    let client = client_of(&headers);
    match auth.login(&client, &f.user, &f.password).await {
        LoginResult::Ok(token) => {
            tracing::info!(%client, "operator logged in");
            let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, "./")]).into_response();
            if let Ok(v) =
                HeaderValue::from_str(&session_cookie(&headers, &token, SESSION_TTL.as_secs()))
            {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            r
        }
        LoginResult::Wrong => {
            tracing::warn!(%client, "operator login failed");
            login_page(StatusCode::UNAUTHORIZED, "Wrong user or password.")
        }
        LoginResult::Limited => {
            tracing::warn!(%client, "operator login rate limited");
            login_page(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many wrong passwords. Try again in 15 minutes.",
            )
        }
        LoginResult::Busy => login_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "The server is busy checking other logins. Try again shortly.",
        ),
    }
}

/// Middleware for the operator router: security headers, CSRF check,
/// login/logout, and the session gate.
pub async fn guard(
    State(auth): State<Arc<OperatorAuth>>,
    mut req: Request,
    next: Next,
) -> Response {
    let resp = match gate(&auth, &mut req).await {
        Some(r) => r,
        None => next.run(req).await,
    };
    with_security_headers(resp)
}

/// `Some(response)` answers the request here; `None` passes it on.
async fn gate(auth: &OperatorAuth, req: &mut Request) -> Option<Response> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let safe = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !same_origin(req.headers()) {
        tracing::warn!(%method, %path, "refused a cross-site operator request");
        return Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({ "error": "cross-site request refused" })),
            )
                .into_response(),
        );
    }
    match (method.clone(), path.as_str()) {
        (Method::POST, "/login") if auth.login_enabled() => {
            let r = std::mem::replace(req, Request::new(Body::empty()));
            return Some(handle_login(auth, r).await);
        }
        (Method::GET, "/login") if auth.login_enabled() => {
            return Some(login_page(StatusCode::OK, ""));
        }
        (Method::POST, "/logout") if auth.login_enabled() => {
            auth.logout(req.headers());
            let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, "./")]).into_response();
            if let Ok(v) = HeaderValue::from_str(&session_cookie(req.headers(), "", 0)) {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            return Some(r);
        }
        _ => {}
    }
    let authed = auth.authed(req.headers());
    req.extensions_mut().insert(Authed(authed));
    if authed || (matches!(method, Method::GET | Method::HEAD) && path == "/status") {
        return None;
    }
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    Some(if method == Method::GET && wants_html {
        login_page(StatusCode::UNAUTHORIZED, "")
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "log in at the operator page first" })),
        )
            .into_response()
    })
}

fn with_security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(h, "x-frame-options", "DENY");
    set(h, "x-content-type-options", "nosniff");
    set(h, "referrer-policy", "no-referrer");
    set(h, "cache-control", "no-store");
    set(
        h,
        "content-security-policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; media-src 'self' blob:; connect-src 'self'; \
         frame-ancestors 'none'; form-action 'self'; base-uri 'none'; object-src 'none'",
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn same_origin_rules() {
        assert!(same_origin(&hdrs(&[])), "curl and servers pass");
        assert!(same_origin(&hdrs(&[("sec-fetch-site", "same-origin")])));
        assert!(!same_origin(&hdrs(&[("sec-fetch-site", "cross-site")])));
        assert!(!same_origin(&hdrs(&[("sec-fetch-site", "same-site")])));
        assert!(same_origin(&hdrs(&[
            ("origin", "https://mint.example.com"),
            ("host", "mint.example.com")
        ])));
        assert!(!same_origin(&hdrs(&[
            ("origin", "https://evil.example"),
            ("host", "mint.example.com")
        ])));
        assert!(!same_origin(&hdrs(&[
            ("origin", "null"),
            ("host", "mint.example.com")
        ])));
    }

    #[test]
    fn cookie_parsing() {
        let h = hdrs(&[("cookie", "a=1; hashu_operator=abc; b=2")]);
        assert_eq!(cookie(&h, COOKIE), Some("abc"));
        assert_eq!(
            cookie(&hdrs(&[("cookie", "hashu_operator=")]), COOKIE),
            None
        );
    }

    #[test]
    fn cookie_path_follows_prefix() {
        let h = hdrs(&[
            ("x-forwarded-prefix", "/operator"),
            ("x-forwarded-proto", "https"),
        ]);
        let c = session_cookie(&h, "t", 60);
        assert!(c.contains("Path=/operator/;"), "{c}");
        assert!(c.ends_with("; Secure"), "{c}");
        let c = session_cookie(&hdrs(&[("x-forwarded-prefix", "/x;Domain=evil")]), "t", 60);
        assert!(c.contains("Path=/;"), "{c}");
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn malformed_hash_is_refused() {
        assert!(OperatorAuth::with_password_hash("operator", "plaintext").is_err());
    }

    #[tokio::test]
    async fn login_sessions_and_rate_limit() {
        // Low cost so the test is quick.
        let hash = bcrypt::hash("hunter22", 4).unwrap();
        let auth = OperatorAuth::with_password_hash("operator", &hash).unwrap();
        assert!(!auth.authed(&HeaderMap::new()));

        let LoginResult::Ok(token) = auth.login("1.2.3.4", "operator", "hunter22").await else {
            panic!("right password refused");
        };
        let h = hdrs(&[("cookie", &format!("{COOKIE}={token}"))]);
        assert!(auth.authed(&h));
        assert!(!auth.authed(&hdrs(&[("cookie", "hashu_operator=forged")])));
        auth.logout(&h);
        assert!(!auth.authed(&h));

        assert_eq!(
            auth.login("1.2.3.4", "root", "hunter22").await,
            LoginResult::Wrong
        );
        for _ in 1..MAX_FAILURES {
            assert_eq!(
                auth.login("1.2.3.4", "operator", "nope").await,
                LoginResult::Wrong
            );
        }
        // Locked out, even with the right password; other clients are not.
        assert_eq!(
            auth.login("1.2.3.4", "operator", "hunter22").await,
            LoginResult::Limited
        );
        assert!(matches!(
            auth.login("5.6.7.8", "operator", "hunter22").await,
            LoginResult::Ok(_)
        ));
    }

    async fn send(app: &axum::Router, req: Request) -> (StatusCode, HeaderMap, String) {
        use tower::ServiceExt;
        let r = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (r.status(), r.headers().clone());
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8_lossy(&body).into_owned())
    }

    fn app(auth: OperatorAuth) -> axum::Router {
        axum::Router::new()
            .route("/", axum::routing::get(|| async { "gui" }))
            .route("/api/controls", axum::routing::post(|| async { "changed" }))
            .route(
                "/status",
                axum::routing::get(|a: Option<axum::Extension<Authed>>| async move {
                    format!("authed={}", a.is_some_and(|a| a.0 .0))
                }),
            )
            .layer(axum::middleware::from_fn_with_state(Arc::new(auth), guard))
    }

    #[tokio::test]
    async fn guard_end_to_end() {
        let hash = bcrypt::hash("hunter22", 4).unwrap();
        let app = app(OperatorAuth::with_password_hash("operator", &hash).unwrap());
        let get = |path: &str, html: bool| {
            let mut b = Request::builder().uri(path);
            if html {
                b = b.header(header::ACCEPT, "text/html");
            }
            b.body(Body::empty()).unwrap()
        };

        // Browser gets the login page, API gets 401, status is public.
        let (st, h, body) = send(&app, get("/", true)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert!(body.contains(r#"action="login""#));
        assert_eq!(h["x-frame-options"], "DENY");
        assert!(h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        assert_eq!(
            send(&app, get("/", false)).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(send(&app, get("/status", false)).await.2, "authed=false");

        // A cross-site form post is refused before anything else.
        let csrf = Request::builder()
            .method(Method::POST)
            .uri("/api/controls")
            .header("sec-fetch-site", "cross-site")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(r#"{"issuance_paused":true,"x":"="}"#))
            .unwrap();
        assert_eq!(send(&app, csrf).await.0, StatusCode::FORBIDDEN);

        // Log in, then the session works.
        let login = Request::builder()
            .method(Method::POST)
            .uri("/login")
            .header("sec-fetch-site", "same-origin")
            .header("x-forwarded-prefix", "/operator")
            .header("x-forwarded-proto", "https")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("user=operator&password=hunter22"))
            .unwrap();
        let (st, h, _) = send(&app, login).await;
        assert_eq!(st, StatusCode::SEE_OTHER);
        let set = h[header::SET_COOKIE].to_str().unwrap().to_string();
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Secure")
        );
        assert!(set.contains("Path=/operator/"));
        let cookie = set.split(';').next().unwrap().to_string();
        let authed = |req: axum::http::request::Builder| {
            req.header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            send(&app, authed(Request::builder().uri("/"))).await.2,
            "gui"
        );
        assert_eq!(
            send(&app, authed(Request::builder().uri("/status")))
                .await
                .2,
            "authed=true"
        );
        let post = Request::builder()
            .method(Method::POST)
            .uri("/api/controls")
            .header("sec-fetch-site", "same-origin");
        assert_eq!(send(&app, authed(post)).await.2, "changed");

        // Even logged in, a cross-site post is refused.
        let post = Request::builder()
            .method(Method::POST)
            .uri("/api/controls")
            .header("origin", "https://evil.example")
            .header("host", "mint.example.com");
        assert_eq!(send(&app, authed(post)).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn open_auth_lets_everyone_in() {
        let auth = OperatorAuth::open();
        assert!(auth.authed(&HeaderMap::new()));
        assert!(!auth.login_enabled());
    }
}
