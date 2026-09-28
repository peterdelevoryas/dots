#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/dots
rsync -az --delete --exclude target --exclude .git --exclude /data --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/dots/"
ssh "$HOST" bash -se -- "$DOTS_DOMAIN" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
cd /root/src/dots
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/dots /usr/local/bin/dots
# The domain lives in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g" deploy/dots.service > /etc/systemd/system/dots.service
sed "s/@DOMAIN@/$DOMAIN/g" deploy/Caddyfile > /etc/caddy/Caddyfile
touch /etc/dots/tokens && chown root:dots /etc/dots/tokens && chmod 0640 /etc/dots/tokens
systemctl daemon-reload
systemctl enable --now dots
systemctl restart dots
systemctl reload caddy
systemctl --no-pager --lines=5 status dots
REMOTE
