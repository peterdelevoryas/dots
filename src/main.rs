mod auth;
mod device;
mod server;
mod store;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "\
usage:
  dots serve                   run the server
  dots token <source> <level>  mint a client token (level: read, write)

environment:
  DOTS_DATA    directory for bundles (default: data)
  DOTS_TOKENS  tokens file (default: tokens)
  DOTS_ADDR    listen address (default: 127.0.0.1:8760)
  DOTS_ORIGIN  public URL the install script fetches from (default: http://DOTS_ADDR)

browser login (off unless all are set):
  DOTS_GOOGLE_CLIENT_ID      Google OAuth client ID
  DOTS_GOOGLE_CLIENT_SECRET  Google OAuth client secret
  DOTS_ALLOWED_EMAILS        comma-separated Google accounts that may approve machines";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dots=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve"] => serve().await,
        ["token", source, level] => {
            let _: auth::Level = level.parse()?;
            if source.is_empty() || source.contains(char::is_whitespace) {
                bail!("source must be a single word, e.g. laptop");
            }
            let token = auth::generate();
            eprintln!("Token (give this to the client; it is not stored):\n{token}\n");
            eprintln!("Append this line to the tokens file:");
            println!("{} {source} {level}", auth::hash(&token));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let env = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_string());
    let data = PathBuf::from(env("DOTS_DATA", "data"));
    let tokens_path = PathBuf::from(env("DOTS_TOKENS", "tokens"));
    let addr = env("DOTS_ADDR", "127.0.0.1:8760");
    let origin = env("DOTS_ORIGIN", &format!("http://{addr}"));
    let origin = origin.trim_end_matches('/').to_string();

    let store = store::Store::open(&data)?;
    // Browser-login tokens live with the data, where the service can write.
    let tokens = auth::Tokens::load(&tokens_path, &data.join("issued-tokens"))?;
    let google = google_config()?;
    match &google {
        Some(g) => tracing::info!(allowed = ?g.allowed_emails, "browser login enabled"),
        None => tracing::info!("browser login disabled (no Google client configured)"),
    }
    let device = device::Device::new(google, tokens.clone(), origin.clone());
    let app = server::router(store, tokens.clone(), device, origin.clone());

    // `systemctl reload dots` re-reads the tokens file without a restart.
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            match tokens.reload() {
                Ok(n) => tracing::info!(tokens = n, "reloaded tokens"),
                Err(e) => tracing::error!("reloading tokens (keeping the old ones): {e:#}"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(
        "listening on http://{addr} (origin: {origin}, data: {})",
        data.display()
    );
    axum::serve(listener, app)
        // Finish in-flight requests on SIGTERM (systemctl stop/restart) or Ctrl-C.
        .with_graceful_shutdown(async {
            let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}

fn google_config() -> Result<Option<device::Google>> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let (id, secret, emails) = (
        var("DOTS_GOOGLE_CLIENT_ID"),
        var("DOTS_GOOGLE_CLIENT_SECRET"),
        var("DOTS_ALLOWED_EMAILS"),
    );
    let (Some(client_id), Some(client_secret), Some(emails)) =
        (id.clone(), secret.clone(), emails.clone())
    else {
        if id.is_some() || secret.is_some() || emails.is_some() {
            bail!(
                "browser login needs all of DOTS_GOOGLE_CLIENT_ID, DOTS_GOOGLE_CLIENT_SECRET, and DOTS_ALLOWED_EMAILS"
            );
        }
        return Ok(None);
    };
    let mut allowed_emails = Vec::new();
    for email in emails.split(',') {
        let email = email.trim().to_lowercase();
        if !email.is_empty() {
            allowed_emails.push(email);
        }
    }
    Ok(Some(device::Google {
        client_id: client_id.trim().to_string(),
        client_secret: client_secret.trim().to_string(),
        allowed_emails,
    }))
}
