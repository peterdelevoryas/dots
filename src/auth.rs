//! Bearer-token auth. Each client gets its own token; the tokens file stores
//! only SHA-256 hashes, one client per line: `<sha256-hex> <source> <level>`.
//!
//! Tokens handed out by the browser login expire, and are kept in a second
//! file the server writes itself, one per line:
//! `<sha256-hex> <source> <level> <expires-unix-seconds>`. Deleting a line
//! and reloading revokes that login.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
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

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Level::Read => "read",
            Level::Write => "write",
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
/// restart, so adding a client doesn't interrupt the others. Also holds the
/// expiring tokens issued by the browser login, which survive restarts.
#[derive(Clone)]
pub struct Tokens {
    path: PathBuf,
    map: Arc<RwLock<HashMap<String, Client>>>,
    issued_path: PathBuf,
    /// Token hash -> (client, expiry in Unix seconds).
    issued: Arc<Mutex<HashMap<String, (Client, i64)>>>,
}

impl Tokens {
    pub fn load(path: &Path, issued_path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            map: Arc::new(RwLock::new(parse(path)?)),
            issued_path: issued_path.to_path_buf(),
            issued: Arc::new(Mutex::new(parse_issued(issued_path)?)),
        })
    }

    /// Mints a token that works until `ttl` from now and records it (hashed)
    /// in the issued-tokens file.
    pub fn issue(&self, source: &str, level: Level, ttl: Duration) -> Result<String> {
        let token = generate();
        let client = Client {
            source: source.to_string(),
            level,
        };
        let now = chrono::Utc::now().timestamp();
        let mut issued = self.issued.lock().unwrap();
        issued.retain(|_, (_, expires)| *expires > now);
        issued.insert(hash(&token), (client, now + ttl.as_secs() as i64));
        write_issued(&self.issued_path, &issued)?;
        Ok(token)
    }

    /// Re-reads both token files. On error the current tokens stay in effect.
    pub fn reload(&self) -> Result<usize> {
        let map = parse(&self.path)?;
        let issued = parse_issued(&self.issued_path)?;
        let n = map.len() + issued.len();
        *self.map.write().unwrap() = map;
        *self.issued.lock().unwrap() = issued;
        Ok(n)
    }

    fn lookup(&self, token: &str) -> Option<Client> {
        let hash = hash(token);
        if let Some(client) = self.map.read().unwrap().get(&hash) {
            return Some(client.clone());
        }
        let issued = self.issued.lock().unwrap();
        match issued.get(&hash) {
            Some((client, expires)) if *expires > chrono::Utc::now().timestamp() => {
                Some(client.clone())
            }
            _ => None,
        }
    }
}

/// Reads the issued-tokens file, skipping expired entries. A missing file is
/// empty.
fn parse_issued(path: &Path) -> Result<HashMap<String, (Client, i64)>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let now = chrono::Utc::now().timestamp();
    let mut map = HashMap::new();
    for (n, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [hash, source, level, expires] = fields[..] else {
            bail!(
                "{}:{}: expected `<sha256> <source> <level> <expires>`",
                path.display(),
                n + 1
            );
        };
        let expires: i64 = expires
            .parse()
            .with_context(|| format!("{}:{}: bad expiry", path.display(), n + 1))?;
        if expires <= now {
            continue;
        }
        let client = Client {
            source: source.to_string(),
            level: level.parse()?,
        };
        map.insert(hash.to_lowercase(), (client, expires));
    }
    Ok(map)
}

fn write_issued(path: &Path, issued: &HashMap<String, (Client, i64)>) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    for (hash, (client, expires)) in issued {
        lines.push(format!(
            "{hash} {} {} {expires}",
            client.source, client.level
        ));
    }
    lines.sort();
    let mut contents = String::from(
        "# Browser-login tokens: <sha256> <source> <level> <expires>. Delete a line and reload to revoke.\n",
    );
    for line in lines {
        contents.push_str(&line);
        contents.push('\n');
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
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
        let tokens = Tokens::load(&path, &dir.join("issued"))?;
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
    fn issued_tokens_expire_and_survive_restarts() -> Result<()> {
        let dir = crate::store::tests::temp_dir("auth-issued");
        let (path, issued) = (dir.join("tokens"), dir.join("issued"));
        std::fs::write(&path, "")?;
        let tokens = Tokens::load(&path, &issued)?;
        let token = tokens.issue("google:me:laptop", Level::Write, Duration::from_secs(60))?;
        let expired = tokens.issue("google:me:old", Level::Read, Duration::ZERO)?;
        assert_eq!(tokens.lookup(&token).unwrap().level, Level::Write);
        assert!(tokens.lookup(&expired).is_none());

        // A restart (a fresh load) still knows the token; expired ones are dropped.
        let restarted = Tokens::load(&path, &issued)?;
        assert_eq!(restarted.lookup(&token).unwrap().source, "google:me:laptop");
        assert!(restarted.lookup(&expired).is_none());

        // Deleting its line and reloading revokes it.
        std::fs::write(&issued, "")?;
        restarted.reload()?;
        assert!(restarted.lookup(&token).is_none());
        Ok(())
    }
}
