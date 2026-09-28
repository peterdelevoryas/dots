# dots

Serves a script that installs my home directory on a new machine. The script lays down my dotfiles, then runs a `setup.sh` that installs rust, helix, Claude Code, Ghostty, and so on. Its content is a **bundle** (a tar.gz of `setup.sh` plus `home/…`) pushed from `~/code/dotfiles`, so updating what gets installed never needs a redeploy.

## Install on a new machine

```sh
curl -fsSL https://dots.pjd.dev/install | sh
```

`/install` is public and returns a small shim. The shim prints a link and a code, and opens the link if there's a browser. You check that the machine and code match, then sign in with Google. If the account is in `DOTS_ALLOWED_EMAILS`, the shim gets a read token and runs the real install script (`/bootstrap`). This is OAuth's device flow, as `gh auth login` uses. Over SSH, just open the printed link on any device.

The token lasts 30 days and is saved (in the macOS keychain as `dots-login`, or in `~/.config/dots/` elsewhere), so re-runs within that time skip the browser. The shim checks a saved token with `/whoami` first and falls back to the browser if it has expired or been revoked. `DOTS_LOGOUT=1` forgets it, `DOTS_NO_CACHE=1` neither uses nor saves one, and `DOTS_TOKEN` overrides it.

Options: `DOTS_DRY_RUN=1` lists changes only, `DOTS_SKIP_SETUP=1` skips `setup.sh`, and `DOTS_VERSION=<ver>` pins a version. Files that would be overwritten are moved to `~/.dots-backup-<time>/`.

## Update what gets installed

```sh
cd ~/code/dotfiles
./dotsync          # copy ~ → repo (paths in `manifest`)
./dotpush          # bundle manifest paths + setup.sh, PUT /bundle
```

`dotpush` signs in the same way, but asks for write access; the approval page says so. It runs the install shim with `DOTS_SCOPE=write DOTS_PRINT_TOKEN=1`, so the saved write login works for 30 days too.

Every browser-login token is recorded (hashed, with who approved it for which machine) in `/var/lib/dots/issued-tokens`. To revoke one, delete its line and run `systemctl reload dots`.

## API

The install shim and the browser login are public. Everything else needs `Authorization: Bearer <token>`. Tokens are `read` or `write`; the browser login mints 30-day tokens of either kind (`/device/code` takes `scope=read|write`).

| Method | Path | Level | |
|---|---|---|---|
| GET | `/healthz` | none | liveness |
| GET | `/install` | none | install shim |
| POST | `/device/code`, `/device/token` | none | shim starts a login, then polls |
| GET | `/device`, POST `/device/login`, GET `/auth/callback` | none | browser approval via Google |
| GET | `/bootstrap` | read | the real install script |
| GET | `/whoami` | read | `<source> <level>` for the token |
| GET | `/bundle`, `/bundle/{ver}` | read | tar.gz with `X-Dots-Version` and `X-Dots-Sha256` headers |
| GET | `/versions` | read | JSON list, newest first |
| PUT | `/bundle` | write | upload (≤ 64 MB, must contain `setup.sh`); becomes current |
| POST | `/current` `{"version": …}` | write | roll back or forward |

The server keeps the newest 30 versions and never prunes the current one.

## Deploy

This works like `~/code/memory`:
1. Create a Hetzner VM with `deploy/cloud-init.yaml` and point DNS at it.
2. Copy `deploy/config.example` to `deploy/config` and run `deploy/deploy.sh`.
3. Optionally, mint static tokens for headless use with `deploy/token.sh <name> <read|write>`. They go into the macOS keychain under `dots-token`.
4. Browser login: create a Google OAuth client (see below), then run `deploy/google.sh <client-id> you@gmail.com`. It prompts for the secret.

### Google OAuth client

In the Google Cloud console, under Google Auth Platform:
1. Branding: app name "dots" and your email.
2. Audience: External, left in **Testing**, with your Gmail added as a test user. While the app is in testing, only test users can sign in, which is a second allowlist.
3. Data access: the `openid` and `email` scopes. These are non-sensitive, so Google doesn't need to review the app.
4. Clients → Create client → Web application, with authorized redirect URI `https://dots.pjd.dev/auth/callback`.
