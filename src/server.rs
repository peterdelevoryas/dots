//! HTTP routes. The install shim and the browser login are public; the
//! bootstrap script and bundles need a bearer token, and pushing a bundle or
//! changing the current one needs a `write` token.

use axum::{
    Extension, Form, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;

use crate::{
    auth::{self, Client, Level, Tokens},
    device::{self, Device, Poll},
    store::{Bundle, MAX_BUNDLE, PutError, Store},
};

const SHIM: &str = include_str!("shim.sh");
const BOOTSTRAP: &str = include_str!("bootstrap.sh");
/// Pins a Google sign-in to the browser that started it.
const LOGIN_COOKIE: &str = "dots_login";

#[derive(Clone)]
struct App {
    store: Store,
    device: Device,
    /// Public URL of this server, e.g. https://dots.pjd.dev, filled into the
    /// scripts so they know where to fetch from.
    origin: String,
}

pub fn router(store: Store, tokens: Tokens, device: Device, origin: String) -> Router {
    let app = App {
        store,
        device,
        origin,
    };
    let authed = Router::new()
        .route("/bootstrap", get(bootstrap))
        .route("/whoami", get(whoami))
        .route("/bundle", get(current_bundle).put(put_bundle))
        .route("/bundle/{version}", get(bundle))
        .route("/versions", get(versions))
        .route("/current", post(set_current))
        // Headroom over MAX_BUNDLE so the store, not the extractor, reports
        // a too-large bundle with a useful message.
        .layer(DefaultBodyLimit::max(MAX_BUNDLE + 1024 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            tokens,
            auth::middleware,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/install", get(install))
        .route("/device/code", post(device_code))
        .route("/device/token", post(device_token))
        .route("/device", get(device_page))
        .route("/device/login", post(device_login))
        .route("/auth/callback", get(auth_callback))
        .merge(authed)
        .with_state(app)
}

fn script(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// Public: a small script that gets a token (from DOTS_TOKEN or a browser
/// login) and then runs the real bootstrap.
async fn install(State(app): State<App>) -> Response {
    script(SHIM.replace("@ORIGIN@", &app.origin))
}

/// Which token this is, so the scripts can check a saved one still works:
/// `<source> <level>`.
async fn whoami(Extension(client): Extension<Client>) -> String {
    format!("{} {}\n", client.source, client.level)
}

async fn bootstrap(State(app): State<App>) -> Response {
    script(BOOTSTRAP.replace("@ORIGIN@", &app.origin))
}

// --- Browser login (see device.rs) ------------------------------------------

#[derive(Deserialize)]
struct CodeForm {
    #[serde(default)]
    name: String,
    /// `read` to install (the default) or `write` to push bundles.
    #[serde(default)]
    scope: Option<String>,
}

/// Public: the shim starts a login here. The reply is `key=value` lines so a
/// POSIX shell can read it without a JSON parser.
async fn device_code(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<CodeForm>,
) -> Response {
    if !app.device.enabled() {
        let msg = "browser login isn't set up on this server; set DOTS_TOKEN instead\n";
        return (StatusCode::SERVICE_UNAVAILABLE, msg).into_response();
    }
    let level = match form.scope.as_deref().unwrap_or("read").parse::<Level>() {
        Ok(level) => level,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response(),
    };
    let ip = client_ip(&headers);
    let Some((device_code, user_code)) = app.device.start(&form.name, &ip, level) else {
        let msg = "too many logins in progress; try again in a few minutes\n";
        return (StatusCode::TOO_MANY_REQUESTS, msg).into_response();
    };
    tracing::info!(name = form.name, ip, user_code, %level, "device login started");
    let body = format!(
        "device_code={device_code}\nuser_code={user_code}\nurl={}/device?code={user_code}\ninterval={}\nexpires_in={}\n",
        app.origin,
        device::POLL_INTERVAL,
        device::DEVICE_TTL.as_secs(),
    );
    ([(header::CACHE_CONTROL, "no-store")], body).into_response()
}

#[derive(Deserialize)]
struct TokenForm {
    device_code: String,
}

/// Public: the shim polls here until you approve in the browser.
async fn device_token(State(app): State<App>, Form(form): Form<TokenForm>) -> Response {
    match app.device.poll(&form.device_code) {
        Poll::Pending => (StatusCode::ACCEPTED, "pending\n".to_string()),
        Poll::Approved(token) => (StatusCode::OK, format!("{token}\n")),
        Poll::Expired => (StatusCode::GONE, "expired\n".to_string()),
    }
    .into_response()
}

#[derive(Deserialize)]
struct DeviceQuery {
    code: Option<String>,
}

/// The page the shim's link opens: confirm the machine, or type a code.
async fn device_page(State(app): State<App>, Query(query): Query<DeviceQuery>) -> Response {
    if !app.device.enabled() {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Browser login is off",
            "<p>This server has no Google sign-in configured. Use a token instead.</p>",
        );
    }
    let code = query.code.unwrap_or_default();
    let request = app.device.pending(&code);
    let Some(request) = request else {
        let error = if code.is_empty() {
            String::new()
        } else {
            "<p class=\"error\">That code has expired or was already used. Check your terminal, or run the install command again.</p>".to_string()
        };
        let body = format!(
            "{error}<p>Enter the code shown in your terminal.</p>\
             <form method=\"get\" action=\"/device\">\
             <input name=\"code\" autocomplete=\"off\" autocapitalize=\"characters\" placeholder=\"XXXX-XXXX\" autofocus>\
             <button>Continue</button></form>"
        );
        return page(StatusCode::OK, "Set up a machine", &body);
    };
    let minutes = request.created.elapsed().as_secs() / 60;
    let when = match minutes {
        0 => "just now".to_string(),
        1 => "1 minute ago".to_string(),
        n => format!("{n} minutes ago"),
    };
    let (asking, command) = match request.level {
        Level::Read => ("to install your home directory", "the install command"),
        Level::Write => (
            "for <strong>write access</strong>, to push new bundles that every future install will get",
            "dotpush",
        ),
    };
    let body = format!(
        "<p>A machine is asking {asking}.</p>\
         <p class=\"code\">{code}</p>\
         <dl><dt>Machine</dt><dd>{name}</dd><dt>Address</dt><dd>{ip}</dd><dt>Requested</dt><dd>{when}</dd><dt>Lasts</dt><dd>30 days on that machine</dd></dl>\
         <p class=\"warn\">Only approve this if you just ran {command} yourself and this code matches your terminal.</p>\
         <form method=\"post\" action=\"/device/login\">\
         <input type=\"hidden\" name=\"user_code\" value=\"{code}\">\
         <button>Approve with Google</button></form>",
        code = escape(&request.user_code),
        name = escape(if request.name.is_empty() {
            "(unnamed)"
        } else {
            &request.name
        }),
        ip = escape(&request.ip),
    );
    page(StatusCode::OK, "Approve this machine?", &body)
}

#[derive(Deserialize)]
struct LoginForm {
    user_code: String,
}

/// The approval form posts here; it sends you to Google.
async fn device_login(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    // Only our own approval page may start a sign-in, so another site can't
    // bounce you through Google to approve a code of its choosing.
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if origin != Some(app.origin.as_str()) {
        tracing::warn!(?origin, "refused cross-site device login");
        return page(
            StatusCode::FORBIDDEN,
            "Refused",
            "<p>This approval didn't come from the dots approval page.</p>",
        );
    }
    let Some((url, oauth_state)) = app.device.begin_login(&form.user_code) else {
        return page(
            StatusCode::GONE,
            "Code expired",
            "<p>That code has expired or was already used. Run the install command again.</p>",
        );
    };
    let secure = if app.origin.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{LOGIN_COOKIE}={oauth_state}; Path=/auth/callback; Max-Age={}; HttpOnly; SameSite=Lax{secure}",
        device::DEVICE_TTL.as_secs()
    );
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, url), (header::SET_COOKIE, cookie)],
    )
        .into_response()
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

/// Google sends you back here after you sign in.
async fn auth_callback(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    let mut resp = finish_callback(&app, &headers, query).await;
    let clear = format!("{LOGIN_COOKIE}=; Path=/auth/callback; Max-Age=0; HttpOnly; SameSite=Lax");
    resp.headers_mut()
        .insert(header::SET_COOKIE, clear.parse().unwrap());
    resp
}

async fn finish_callback(app: &App, headers: &HeaderMap, query: Callback) -> Response {
    if let Some(error) = query.error {
        let body = format!(
            "<p>Google sign-in didn't finish ({}). Open the link from your terminal to try again.</p>",
            escape(&error)
        );
        return page(StatusCode::BAD_REQUEST, "Not approved", &body);
    }
    let (Some(oauth_state), Some(code)) = (query.state, query.code) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Not approved",
            "<p>Google's reply was missing its code.</p>",
        );
    };
    // The sign-in must finish in the browser that started it.
    if read_cookie(headers, LOGIN_COOKIE) != Some(oauth_state.as_str()) {
        return page(
            StatusCode::FORBIDDEN,
            "Not approved",
            "<p>This sign-in didn't start in this browser. Open the link from your terminal to try again.</p>",
        );
    }
    match app.device.finish_login(&oauth_state, &code).await {
        Ok((request, email)) => {
            let name = if request.name.is_empty() {
                "The machine"
            } else {
                &request.name
            };
            let body = format!(
                "<p><strong>{}</strong> can now install your home directory. You can close this tab and go back to your terminal.</p><p class=\"muted\">Approved as {}.</p>",
                escape(name),
                escape(&email)
            );
            page(StatusCode::OK, "Approved", &body)
        }
        Err(e) => {
            tracing::warn!("device approval failed: {e:#}");
            let body = format!("<p>{}</p>", escape(&format!("{e:#}")));
            page(StatusCode::FORBIDDEN, "Not approved", &body)
        }
    }
}

/// Caddy sets X-Forwarded-For to the real client address (it doesn't trust
/// incoming values), and the server only listens on localhost behind it.
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn read_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else { continue };
        for part in value.split(';') {
            if let Some((k, v)) = part.split_once('=')
                && k.trim() == name
            {
                return Some(v.trim());
            }
        }
    }
    None
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A small standalone HTML page. `body` must already be escaped.
///
/// The referrer policy is `same-origin`, not `no-referrer`: the approval URL
/// (with its code) still never reaches Google, but under `no-referrer`
/// browsers send `Origin: null` on our own form posts, which the cross-site
/// check in `device_login` rightly refuses.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="same-origin">
<title>{title} · dots</title>
<style>
:root {{ --bg: #faf7f2; --fg: #2b2622; --muted: #7a7068; --accent: #b0643c; --line: #e4ddd3; --warn: #8a5a00; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg: #1f1c19; --fg: #ece6de; --muted: #a39a90; --accent: #d98a5f; --line: #3a342e; --warn: #e0b060; }} }}
body {{ background: var(--bg); color: var(--fg); font: 16px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; padding: 48px 16px; }}
main {{ max-width: 440px; margin: 0 auto; }}
h1 {{ font-size: 22px; margin: 0 0 16px; }}
.code {{ font: 600 32px/1.2 ui-monospace, "SF Mono", monospace; letter-spacing: 0.08em; margin: 16px 0; }}
dl {{ display: grid; grid-template-columns: max-content 1fr; gap: 4px 16px; margin: 16px 0; }}
dt {{ color: var(--muted); }} dd {{ margin: 0; overflow-wrap: anywhere; }}
.warn {{ color: var(--warn); }} .error {{ color: var(--accent); }} .muted {{ color: var(--muted); }}
input {{ font: 20px ui-monospace, "SF Mono", monospace; padding: 8px 10px; border: 1px solid var(--line); border-radius: 8px; background: transparent; color: var(--fg); width: 12ch; margin-right: 8px; }}
button {{ font: inherit; font-weight: 600; padding: 10px 18px; border: 0; border-radius: 8px; background: var(--accent); color: #fff; cursor: pointer; }}
</style></head>
<body><main><h1>{title}</h1>{body}</main></body></html>
"#,
        title = escape(title),
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response()
}

// --- Bundles ----------------------------------------------------------------

async fn current_bundle(State(app): State<App>) -> Response {
    serve_bundle(app.store, None).await
}

async fn bundle(State(app): State<App>, Path(version): Path<String>) -> Response {
    serve_bundle(app.store, Some(version)).await
}

async fn serve_bundle(store: Store, version: Option<String>) -> Response {
    let found = tokio::task::spawn_blocking(move || store.get(version.as_deref())).await;
    match found {
        Ok(Ok(Some((bundle, bytes)))) => (
            [
                (header::CONTENT_TYPE, "application/gzip".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (
                    header::HeaderName::from_static("x-dots-version"),
                    bundle.version,
                ),
                (
                    header::HeaderName::from_static("x-dots-sha256"),
                    bundle.sha256,
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "no such bundle\n").into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e.into()),
    }
}

async fn versions(State(app): State<App>) -> Response {
    let store = app.store;
    match tokio::task::spawn_blocking(move || store.list()).await {
        Ok(Ok(list)) => Json(list).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e.into()),
    }
}

async fn put_bundle(
    State(app): State<App>,
    Extension(client): Extension<Client>,
    body: Bytes,
) -> Response {
    if let Err(denied) = auth::require(&client, Level::Write) {
        return denied.into_response();
    }
    let store = app.store;
    let result: Result<Bundle, PutError> =
        match tokio::task::spawn_blocking(move || store.put(&body)).await {
            Ok(result) => result,
            Err(e) => return internal(e.into()),
        };
    match result {
        Ok(bundle) => {
            tracing::info!(
                source = client.source,
                version = bundle.version,
                "pushed bundle"
            );
            (StatusCode::CREATED, Json(bundle)).into_response()
        }
        Err(PutError::Invalid(msg)) => {
            (StatusCode::BAD_REQUEST, format!("invalid bundle: {msg}\n")).into_response()
        }
        Err(PutError::Other(e)) => internal(e),
    }
}

#[derive(Deserialize)]
struct SetCurrent {
    version: String,
}

async fn set_current(
    State(app): State<App>,
    Extension(client): Extension<Client>,
    Json(req): Json<SetCurrent>,
) -> Response {
    if let Err(denied) = auth::require(&client, Level::Write) {
        return denied.into_response();
    }
    let store = app.store;
    let version = req.version.clone();
    match tokio::task::spawn_blocking(move || store.set_current(&version)).await {
        Ok(Ok(true)) => {
            tracing::info!(source = client.source, version = req.version, "set current");
            (StatusCode::OK, format!("current is now {}\n", req.version)).into_response()
        }
        Ok(Ok(false)) => (StatusCode::NOT_FOUND, "no such bundle\n").into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e.into()),
    }
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("{e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::store::tests::{tarball, temp_dir};

    fn app() -> Router {
        app_with_device().0
    }

    fn app_with_device() -> (Router, Device) {
        let dir = temp_dir("server");
        let tokens_path = dir.join("tokens");
        std::fs::write(
            &tokens_path,
            format!(
                "{} laptop write\n{} install read\n",
                auth::hash("w"),
                auth::hash("r")
            ),
        )
        .unwrap();
        let store = Store::open(&dir.join("data")).unwrap();
        let tokens = Tokens::load(&tokens_path, &dir.join("issued")).unwrap();
        let origin = "https://dots.test".to_string();
        let device = Device::new(
            Some(device::tests::google()),
            tokens.clone(),
            origin.clone(),
        );
        (router(store, tokens, device.clone(), origin), device)
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Vec<u8>,
    ) -> Response {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if method == "POST" {
            let json = body.first() == Some(&b'{');
            let kind = if json {
                "application/json"
            } else {
                "application/x-www-form-urlencoded"
            };
            req = req.header(header::CONTENT_TYPE, kind);
        }
        app.clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }

    async fn text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn auth_levels() {
        let app = app();
        let bundle = tarball(&[("setup.sh", "true")]);

        assert_eq!(
            send(&app, "GET", "/healthz", None, vec![]).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            send(&app, "GET", "/bootstrap", None, vec![]).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&app, "GET", "/bootstrap", Some("bogus"), vec![])
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&app, "PUT", "/bundle", Some("r"), bundle.clone())
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(&app, "GET", "/bundle", Some("r"), vec![])
                .await
                .status(),
            StatusCode::NOT_FOUND
        );

        let resp = send(&app, "PUT", "/bundle", Some("w"), bundle.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let pushed: serde_json::Value = serde_json::from_str(&text(resp).await).unwrap();
        let version = pushed["version"].as_str().unwrap().to_string();

        let resp = send(&app, "GET", "/bundle", Some("r"), vec![]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["x-dots-version"], version.as_str());
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], &bundle[..]);

        let resp = send(&app, "GET", "/bootstrap", Some("r"), vec![]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let script = text(resp).await;
        assert!(script.contains(r#"origin="https://dots.test""#));
        assert!(!script.contains("@ORIGIN@"));

        let rollback = format!(r#"{{"version":"{version}"}}"#).into_bytes();
        assert_eq!(
            send(&app, "POST", "/current", Some("r"), rollback.clone())
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(&app, "POST", "/current", Some("w"), rollback)
                .await
                .status(),
            StatusCode::OK
        );
        let missing = br#"{"version":"20200101T000000Z-deadbeef"}"#.to_vec();
        assert_eq!(
            send(&app, "POST", "/current", Some("w"), missing)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );

        let resp = send(&app, "PUT", "/bundle", Some("w"), b"junk".to_vec()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn device_login_over_http() {
        let (app, device) = app_with_device();

        // The shim is public.
        let resp = send(&app, "GET", "/install", None, vec![]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(text(resp).await.contains(r#"origin="https://dots.test""#));

        let resp = send(
            &app,
            "POST",
            "/device/code",
            None,
            b"name=newbox&scope=bogus".to_vec(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = send(&app, "POST", "/device/code", None, b"name=newbox".to_vec()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = text(resp).await;
        let field = |k: &str| {
            body.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap()
                .to_string()
        };
        let (device_code, user_code) = (field("device_code"), field("user_code"));
        assert_eq!(
            field("url"),
            format!("https://dots.test/device?code={user_code}")
        );

        let poll = format!("device_code={device_code}").into_bytes();
        let resp = send(&app, "POST", "/device/token", None, poll.clone()).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // The approval page shows the machine; unknown codes get the entry form.
        let resp = send(
            &app,
            "GET",
            &format!("/device?code={user_code}"),
            None,
            vec![],
        )
        .await;
        let html = text(resp).await;
        assert!(html.contains("newbox") && html.contains("Approve with Google"));
        let resp = send(&app, "GET", "/device?code=BBBB-BBBB", None, vec![]).await;
        assert!(text(resp).await.contains("expired"));

        // Starting a sign-in needs our own Origin.
        let form = format!("user_code={user_code}").into_bytes();
        let resp = send(&app, "POST", "/device/login", None, form.clone()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let req = Request::post("/device/login")
            .header(header::ORIGIN, "https://dots.test")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(
            resp.headers()[header::LOCATION]
                .to_str()
                .unwrap()
                .starts_with("https://accounts.google.com/")
        );
        let cookie = resp.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.starts_with("dots_login=") && cookie.contains("Secure"));

        // A callback without the browser's cookie is refused.
        let resp = send(&app, "GET", "/auth/callback?state=x&code=y", None, vec![]).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Approve (skipping Google), then the token unlocks the bootstrap once.
        device.approve_for_test(&user_code);
        let resp = send(&app, "POST", "/device/token", None, poll.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let token = text(resp).await.trim().to_string();
        let resp = send(&app, "GET", "/bootstrap", Some(&token), vec![]).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = send(
            &app,
            "PUT",
            "/bundle",
            Some(&token),
            tarball(&[("setup.sh", "")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = send(&app, "POST", "/device/token", None, poll).await;
        assert_eq!(resp.status(), StatusCode::GONE);
    }

    #[tokio::test]
    async fn write_login_can_push() {
        let (app, device) = app_with_device();
        let resp = send(
            &app,
            "POST",
            "/device/code",
            None,
            b"name=laptop&scope=write".to_vec(),
        )
        .await;
        let body = text(resp).await;
        let field = |k: &str| {
            body.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap()
                .to_string()
        };
        let resp = send(
            &app,
            "GET",
            &format!("/device?code={}", field("user_code")),
            None,
            vec![],
        )
        .await;
        assert!(text(resp).await.contains("write access"));
        device.approve_for_test(&field("user_code"));
        let poll = format!("device_code={}", field("device_code")).into_bytes();
        let token = text(send(&app, "POST", "/device/token", None, poll).await).await;
        let token = token.trim();
        let resp = send(&app, "GET", "/whoami", Some(token), vec![]).await;
        assert_eq!(text(resp).await, "google:me@example.com:laptop write\n");
        let resp = send(
            &app,
            "PUT",
            "/bundle",
            Some(token),
            tarball(&[("setup.sh", "")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }
}
