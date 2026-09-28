//! Bearer-token auth. Each client gets its own token; the tokens file stores
//! only SHA-256 hashes, one client per line: `<sha256-hex> <source> <level>`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Read,
    Write,
}

impl std::str::FromStr for Level {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "read" => Level::Read,
            "write" => Level::Write,
            _ => bail!("unknown level {s:?} (expected read or write)"),
        })
    }
}

/// The authenticated caller, attached to each request's extensions.
#[derive(Debug, Clone)]
pub struct Client {
    pub source: String,
    pub level: Level,
}

/// The tokens file, loaded at startup and reloadable (on SIGHUP) without a
/// restart, so adding a client doesn't interrupt the others. Also holds
/// short-lived tokens handed out by the browser login, which live only in
/// memory.
#[derive(Clone)]
pub struct Tokens {
    path: PathBuf,
    map: Arc<RwLock<HashMap<String, Client>>>,
    temporary: Arc<Mutex<HashMap<String, (Client, Instant)>>>,
}

impl Tokens {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            map: Arc::new(RwLock::new(parse(path)?)),
            temporary: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Mints a token that works until `ttl` from now and isn't written anywhere.
    pub fn issue_temporary(&self, source: &str, level: Level, ttl: Duration) -> String {
        let token = generate();
        let client = Client {
            source: source.to_string(),
            level,
        };
        let now = Instant::now();
        let mut temporary = self.temporary.lock().unwrap();
        temporary.retain(|_, (_, expires)| *expires > now);
        temporary.insert(hash(&token), (client, now + ttl));
        token
    }

    /// Re-reads the tokens file. On error the current tokens stay in effect.
    pub fn reload(&self) -> Result<usize> {
        let map = parse(&self.path)?;
        let n = map.len();
        *self.map.write().unwrap() = map;
        Ok(n)
    }

    fn lookup(&self, token: &str) -> Option<Client> {
        let hash = hash(token);
        if let Some(client) = self.map.read().unwrap().get(&hash) {
            return Some(client.clone());
        }
        let temporary = self.temporary.lock().unwrap();
        match temporary.get(&hash) {
            Some((client, expires)) if *expires > Instant::now() => Some(client.clone()),
            _ => None,
        }
    }
}

fn parse(path: &Path) -> Result<HashMap<String, Client>> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut map = HashMap::new();
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [hash, source, level] = fields[..] else {
            bail!(
                "{}:{}: expected `<sha256> <source> <level>`",
                path.display(),
                n + 1
            );
        };
        let client = Client {
            source: source.to_string(),
            level: level.parse()?,
        };
        if map.insert(hash.to_lowercase(), client).is_some() {
            bail!("{}:{}: duplicate token hash", path.display(), n + 1);
        }
    }
    Ok(map)
}

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("dots_{}", hex::encode(bytes))
}

pub async fn middleware(State(tokens): State<Tokens>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match token.and_then(|t| tokens.lookup(t)) {
        Some(client) => {
            req.extensions_mut().insert(client);
            next.run(req).await
        }
        None => {
            tracing::warn!(
                token_present = token.is_some(),
                "rejected unauthenticated request"
            );
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "missing or invalid bearer token\n",
            )
                .into_response()
        }
    }
}

/// Rejects a caller whose token doesn't grant `level`.
pub fn require(client: &Client, level: Level) -> Result<(), (StatusCode, String)> {
    if client.level < level {
        tracing::warn!(
            source = client.source,
            "rejected request needing {level:?} access"
        );
        let msg = format!(
            "token for {} has {:?} access; this needs {level:?}\n",
            client.source, client.level
        );
        return Err((StatusCode::FORBIDDEN, msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_picks_up_new_tokens_and_keeps_old_ones_on_error() -> Result<()> {
        let dir = crate::store::tests::temp_dir("auth");
        let path = dir.join("tokens");
        std::fs::write(&path, format!("{} first write\n", hash("t1")))?;
        let tokens = Tokens::load(&path)?;
        assert_eq!(tokens.lookup("t1").unwrap().source, "first");
        assert!(tokens.lookup("t2").is_none());

        std::fs::write(
            &path,
            format!("{} first write\n{} second read\n", hash("t1"), hash("t2")),
        )?;
        assert_eq!(tokens.reload()?, 2);
        assert_eq!(tokens.lookup("t2").unwrap().level, Level::Read);

        // A broken file is rejected and the previous tokens stay in effect.
        std::fs::write(&path, "not a valid line\n")?;
        assert!(tokens.reload().is_err());
        assert!(tokens.lookup("t2").is_some());
        Ok(())
    }

    #[test]
    fn temporary_tokens_expire() -> Result<()> {
        let dir = crate::store::tests::temp_dir("auth-temp");
        let path = dir.join("tokens");
        std::fs::write(&path, "")?;
        let tokens = Tokens::load(&path)?;
        let short = tokens.issue_temporary("device", Level::Read, Duration::from_millis(50));
        let long = tokens.issue_temporary("device", Level::Read, Duration::from_secs(60));
        assert_eq!(tokens.lookup(&short).unwrap().level, Level::Read);
        std::thread::sleep(Duration::from_millis(100));
        assert!(tokens.lookup(&short).is_none());
        assert!(tokens.lookup(&long).is_some());
        // Reloading the file keeps temporary tokens.
        tokens.reload()?;
        assert!(tokens.lookup(&long).is_some());
        Ok(())
    }
}
