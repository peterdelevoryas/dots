#!/bin/bash
# Turns on the browser login: stores the Google OAuth client and the accounts
# allowed to approve machines in /etc/dots/google.env on the server, then
# restarts dots. Prompts for the client secret so it never lands in shell
# history or this repo.
#
# Usage: deploy/google.sh <client-id> <email>[,<email>...]
# The client's redirect URI must be https://<DOTS_DOMAIN>/auth/callback.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
[ $# -eq 2 ] || { sed -n 's/^# \{0,1\}//; 2,8p' "$0" >&2; exit 2; }
client_id=$1 emails=$2
read -rsp "Client secret: " secret; echo >&2
[ -n "$secret" ] || { echo "no secret given" >&2; exit 1; }

printf 'DOTS_GOOGLE_CLIENT_ID=%s\nDOTS_GOOGLE_CLIENT_SECRET=%s\nDOTS_ALLOWED_EMAILS=%s\n' \
	"$client_id" "$secret" "$emails" |
	ssh "$HOST" 'install -m 0640 -o root -g dots /dev/stdin /etc/dots/google.env && systemctl restart dots && sleep 1 && journalctl -u dots -n 3 --no-pager -o cat'
