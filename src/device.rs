//! Browser login for new machines, shaped like OAuth's device flow
//! (RFC 8628). The public install shim asks for a device code, shows you a
//! short user code and a link, and polls. You open the link, check the code
//! and the machine, and sign in with Google; if the Google account is on the
//! allowlist, the shim gets a short-lived read token.
//!
//! Everything here lives in memory: a restart just means re-running the
//! install command.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::{Level, Tokens};

/// How long a device code (and a Google sign-in started for it) stays valid.
pub const DEVICE_TTL: Duration = Duration::from_secs(10 * 60);
/// How long the read token handed to an approved machine works. It only
/// needs to outlast fetching the bootstrap script and the bundle.
pub const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
/// How often the shim should poll, in seconds.
pub const POLL_INTERVAL: u64 = 3;
/// Caps memory use if someone hammers the public endpoint.
const MAX_PENDING: usize = 50;
/// No vowels, so codes never spell words; no easily confused letters.
const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

#[derive(Clone)]
pub struct Google {
    pub client_id: String,
    pub client_secret: String,
    /// Lowercased email addresses allowed to approve machines.
    pub allowed_emails: Vec<String>,
}

#[derive(Clone)]
pub struct Device {
    state: Arc<Mutex<State>>,
    google: Option<Google>,
    tokens: Tokens,
    origin: String,
    http: reqwest::Client,
}

#[derive(Default)]
struct State {
    /// Keyed by device code, the secret only the shim knows.
    requests: HashMap<String, Request>,
    /// Google sign-ins in progress, keyed by OAuth `state`.
    logins: HashMap<String, Login>,
}

#[derive(Clone)]
pub struct Request {
    pub user_code: String,
    /// What the machine calls itself (from `hostname`), for the approval page.
    pub name: String,
    /// The address the request came from, for the approval page.
    pub ip: String,
    pub created: Instant,
    approved: Option<String>,
}

struct Login {
    user_code: String,
    pkce_verifier: String,
    created: Instant,
}

/// What the shim gets back when it polls.
pub enum Poll {
    Pending,
    Approved(String),
    Expired,
}

impl Device {
    pub fn new(google: Option<Google>, tokens: Tokens, origin: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            google,
            tokens,
            origin,
            http: reqwest::Client::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.google.is_some()
    }

    /// Starts a request. Returns (device code, user code), or None if too many
    /// are already pending.
    pub fn start(&self, name: &str, ip: &str) -> Option<(String, String)> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        if state.requests.len() >= MAX_PENDING {
            return None;
        }
        let device_code = random_hex(32);
        let user_code = user_code();
        let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
        state.requests.insert(
            device_code.clone(),
            Request {
                user_code: user_code.clone(),
                name,
                ip: ip.chars().take(64).collect(),
                created: Instant::now(),
                approved: None,
            },
        );
        Some((device_code, user_code))
    }

    /// Checks on a request. An approved token is handed out once, and the
    /// request is then forgotten.
    pub fn poll(&self, device_code: &str) -> Poll {
        let mut state = self.state.lock().unwrap();
        state.prune();
        let Some(request) = state.requests.get(device_code) else {
            return Poll::Expired;
        };
        if request.approved.is_none() {
            return Poll::Pending;
        }
        let request = state.requests.remove(device_code).unwrap();
        Poll::Approved(request.approved.unwrap())
    }

    /// The pending request for a user code, as typed by a person.
    pub fn pending(&self, user_code: &str) -> Option<Request> {
        let user_code = normalize_user_code(user_code)?;
        let mut state = self.state.lock().unwrap();
        state.prune();
        for request in state.requests.values() {
            if request.user_code == user_code && request.approved.is_none() {
                return Some(request.clone());
            }
        }
        None
    }

    /// Begins a Google sign-in to approve `user_code`. Returns the Google URL
    /// to redirect to and the OAuth state, which the caller pins in a cookie.
    pub fn begin_login(&self, user_code: &str) -> Option<(String, String)> {
        let google = self.google.as_ref()?;
        let request = self.pending(user_code)?;
        let oauth_state = random_hex(16);
        let pkce_verifier = random_hex(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce_verifier.as_bytes()));

        let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        url.query_pairs_mut()
            .append_pair("client_id", &google.client_id)
            .append_pair("redirect_uri", &self.redirect_uri())
            .append_pair("response_type", "code")
            .append_pair("scope", "openid email")
            .append_pair("state", &oauth_state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("prompt", "select_account");

        let mut state = self.state.lock().unwrap();
        state.logins.insert(
            oauth_state.clone(),
            Login {
                user_code: request.user_code,
                pkce_verifier,
                created: Instant::now(),
            },
        );
        Some((url.to_string(), oauth_state))
    }

    /// Finishes a Google sign-in: exchanges the code, checks the account
    /// against the allowlist, and approves the request. Returns the request
    /// that was approved and the email that approved it.
    pub async fn finish_login(&self, oauth_state: &str, code: &str) -> Result<(Request, String)> {
        let Some(google) = &self.google else {
            bail!("Google sign-in isn't configured");
        };
        let login = {
            let mut state = self.state.lock().unwrap();
            state.prune();
            state.logins.remove(oauth_state)
        };
        let Some(login) = login else {
            bail!(
                "this sign-in expired or was already used; start again from the link in your terminal"
            );
        };

        let email = self
            .google_email(google, code, &login.pkce_verifier)
            .await?;
        if !google.allowed_emails.contains(&email) {
            tracing::warn!(
                email,
                "rejected device approval from an account not on the allowlist"
            );
            bail!("{email} isn't allowed to approve machines");
        }
        self.approve(&login.user_code, &email)
            .map(|request| (request, email))
    }

    /// Marks a pending request approved, minting its token.
    fn approve(&self, user_code: &str, email: &str) -> Result<Request> {
        let mut state = self.state.lock().unwrap();
        state.prune();
        for request in state.requests.values_mut() {
            if request.user_code != user_code || request.approved.is_some() {
                continue;
            }
            let token =
                self.tokens
                    .issue_temporary(&format!("device:{email}"), Level::Read, TOKEN_TTL);
            request.approved = Some(token);
            tracing::info!(
                email,
                name = request.name,
                ip = request.ip,
                "approved device"
            );
            return Ok(request.clone());
        }
        bail!("this code expired or was already used; run the install command again")
    }

    async fn google_email(&self, google: &Google, code: &str, verifier: &str) -> Result<String> {
        let redirect_uri = self.redirect_uri();
        let form = [
            ("code", code),
            ("client_id", google.client_id.as_str()),
            ("client_secret", google.client_secret.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ];
        let resp = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .form(&form)
            .send()
            .await
            .context("contacting Google")?;
        let status = resp.status();
        let body = resp.text().await.context("reading Google's response")?;
        if !status.is_success() {
            bail!("Google rejected the sign-in ({status}): {body}");
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            id_token: String,
        }
        let token: TokenResponse =
            serde_json::from_str(&body).context("parsing Google's response")?;
        id_token_email(&token.id_token, &google.client_id)
    }

    fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.origin)
    }
}

impl State {
    fn prune(&mut self) {
        let now = Instant::now();
        self.requests
            .retain(|_, r| now.duration_since(r.created) < DEVICE_TTL);
        self.logins
            .retain(|_, l| now.duration_since(l.created) < DEVICE_TTL);
    }
}

/// Pulls the verified email out of a Google ID token. The token came
/// straight from Google's token endpoint over TLS, so (per OpenID Connect
/// Core 3.1.3.7) the TLS connection vouches for it and the signature needn't
/// be checked; the claims still are.
fn id_token_email(id_token: &str, client_id: &str) -> Result<String> {
    let payload = id_token.split('.').nth(1).context("malformed ID token")?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("malformed ID token payload")?;
    #[derive(Deserialize)]
    struct Claims {
        iss: String,
        aud: String,
        exp: i64,
        email: Option<String>,
        email_verified: Option<bool>,
    }
    let claims: Claims = serde_json::from_slice(&payload).context("parsing ID token claims")?;
    if claims.iss != "https://accounts.google.com" && claims.iss != "accounts.google.com" {
        bail!("ID token from unexpected issuer {}", claims.iss);
    }
    if claims.aud != client_id {
        bail!("ID token for a different client");
    }
    if claims.exp < chrono::Utc::now().timestamp() {
        bail!("ID token expired");
    }
    let Some(email) = claims.email else {
        bail!("Google didn't share an email address");
    };
    if claims.email_verified != Some(true) {
        bail!("{email} isn't verified with Google");
    }
    Ok(email.to_lowercase())
}

fn user_code() -> String {
    let mut code = String::new();
    for i in 0..8 {
        if i == 4 {
            code.push('-');
        }
        let n: u8 = rand::random();
        code.push(USER_CODE_ALPHABET[n as usize % USER_CODE_ALPHABET.len()] as char);
    }
    code
}

/// Accepts what a person might type: lowercase, no dash, stray spaces.
fn normalize_user_code(input: &str) -> Option<String> {
    let letters: Vec<char> = input
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if letters.len() != 8 {
        return None;
    }
    let mut code: String = letters[..4].iter().collect();
    code.push('-');
    code.extend(&letters[4..]);
    Some(code)
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    for b in buf.iter_mut() {
        *b = rand::random();
    }
    hex::encode(buf)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    impl Device {
        /// Approves without a Google round trip, for tests.
        pub fn approve_for_test(&self, user_code: &str) {
            self.approve(user_code, "me@example.com").unwrap();
        }
    }

    pub fn google() -> Google {
        Google {
            client_id: "client".into(),
            client_secret: "secret".into(),
            allowed_emails: vec!["me@example.com".into()],
        }
    }

    fn device() -> Device {
        let dir = crate::store::tests::temp_dir("device");
        std::fs::write(dir.join("tokens"), "").unwrap();
        let tokens = Tokens::load(&dir.join("tokens")).unwrap();
        Device::new(Some(google()), tokens, "https://dots.test".into())
    }

    #[test]
    fn flow() {
        let device = device();
        let (device_code, user_code) = device.start("newbox", "1.2.3.4").unwrap();
        assert!(matches!(device.poll(&device_code), Poll::Pending));
        assert!(matches!(device.poll("wrong"), Poll::Expired));

        let typed = user_code.to_lowercase().replace('-', " ");
        let request = device.pending(&typed).unwrap();
        assert_eq!(
            (request.name.as_str(), request.ip.as_str()),
            ("newbox", "1.2.3.4")
        );

        let (url, state) = device.begin_login(&user_code).unwrap();
        assert!(url.starts_with("https://accounts.google.com/"));
        assert!(url.contains(&format!("state={state}")));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fdots.test%2Fauth%2Fcallback"));

        device.approve_for_test(&user_code);
        assert!(device.pending(&user_code).is_none());
        let Poll::Approved(token) = device.poll(&device_code) else {
            panic!("expected approval");
        };
        assert!(token.starts_with("dots_"));
        // Handed out once.
        assert!(matches!(device.poll(&device_code), Poll::Expired));
    }

    #[test]
    fn caps_pending_requests() {
        let device = device();
        for _ in 0..MAX_PENDING {
            assert!(device.start("x", "ip").is_some());
        }
        assert!(device.start("x", "ip").is_none());
    }

    #[test]
    fn user_codes() {
        let code = user_code();
        assert_eq!(code.len(), 9);
        assert_eq!(normalize_user_code(&code.to_lowercase()), Some(code));
        assert_eq!(normalize_user_code("bcdf ghjk"), Some("BCDF-GHJK".into()));
        assert_eq!(normalize_user_code("bcd"), None);
    }

    fn id_token(claims: serde_json::Value) -> String {
        format!("x.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn id_token_checks() {
        let exp = chrono::Utc::now().timestamp() + 60;
        let good = serde_json::json!({"iss": "https://accounts.google.com", "aud": "client", "exp": exp, "email": "Me@Example.com", "email_verified": true});
        assert_eq!(
            id_token_email(&id_token(good.clone()), "client").unwrap(),
            "me@example.com"
        );
        assert!(id_token_email(&id_token(good), "other").is_err());
        let unverified = serde_json::json!({"iss": "accounts.google.com", "aud": "client", "exp": exp, "email": "me@example.com", "email_verified": false});
        assert!(id_token_email(&id_token(unverified), "client").is_err());
        let expired = serde_json::json!({"iss": "accounts.google.com", "aud": "client", "exp": 1, "email": "me@example.com", "email_verified": true});
        assert!(id_token_email(&id_token(expired), "client").is_err());
        let wrong_iss = serde_json::json!({"iss": "https://evil.example", "aud": "client", "exp": exp, "email": "me@example.com", "email_verified": true});
        assert!(id_token_email(&id_token(wrong_iss), "client").is_err());
    }
}
