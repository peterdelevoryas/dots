#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/dots
rsync -az --delete --exclude target --exclude .git --exclude /data --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/dots/"
ssh "$HOST" bash -se -- "$DOTS_DOMAIN" "$DOTS_PRIVATE_IP" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
PRIVATE_IP=$2
cd /root/src/dots
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/dots /usr/local/bin/dots
# The domain and private IP live in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g; s/@PRIVATE_IP@/$PRIVATE_IP/g" deploy/dots.service > /etc/systemd/system/dots.service
# The edge terminates TLS for the domain and proxies here over the private
# network (its route for dots limits request bodies to 80MB).
ufw allow from 10.0.0.0/16 to any port 8760 proto tcp >/dev/null
touch /etc/dots/tokens && chown root:dots /etc/dots/tokens && chmod 0640 /etc/dots/tokens
systemctl daemon-reload
systemctl enable --now dots
systemctl restart dots
systemctl --no-pager --lines=5 status dots
REMOTE
