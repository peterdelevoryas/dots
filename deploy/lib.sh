# Sourced by the deploy scripts: loads deploy/config and sets HOST.
config="$(dirname "${BASH_SOURCE[0]}")/config"
[ -f "$config" ] || { echo "missing $config; copy deploy/config.example and fill it in" >&2; exit 1; }
. "$config"
: "${DOTS_PRIVATE_IP:?set DOTS_PRIVATE_IP in deploy/config}"
# The server has no public address and the domain points at the edge, so ssh
# goes to the private IP (through the edge: a ProxyJump in ~/.ssh/config).
HOST=${DOTS_HOST:-root@$DOTS_PRIVATE_IP}
