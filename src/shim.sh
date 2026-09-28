#!/bin/sh
# Installs your home directory from @ORIGIN@.
#
#   curl -fsSL @ORIGIN@/install | sh
#
# This part is public. It asks you to approve the machine in a browser
# (you sign in with Google), gets a short-lived token, and runs the real
# install script with it. With DOTS_TOKEN set, it skips the browser.
#
# Options (environment) are passed through to the install script:
#   DOTS_VERSION=<ver>  install that version instead of the current one
#   DOTS_DRY_RUN=1      show what would change, touch nothing
#   DOTS_SKIP_SETUP=1   lay down files only
#   DOTS_NO_BROWSER=1   print the link instead of opening a browser

# Everything is inside main so the whole script is read before any of it runs.
main() {
	set -eu
	origin="@ORIGIN@"

	if [ -z "${DOTS_TOKEN:-}" ]; then
		DOTS_TOKEN=$(login)
	fi
	export DOTS_TOKEN

	curl -fsSL -H "Authorization: Bearer $DOTS_TOKEN" "$origin/bootstrap" | sh
}

# Runs the browser login and prints the token it gets.
login() {
	name=$(hostname 2>/dev/null || uname -n)
	reply=$(curl -sS --fail-with-body -X POST --data-urlencode "name=$name" "$origin/device/code") || {
		echo "dots: couldn't start a login: $reply" >&2
		exit 1
	}
	device_code=$(field device_code)
	user_code=$(field user_code)
	url=$(field url)
	interval=$(field interval)
	expires_in=$(field expires_in)

	{
		echo
		echo "  To approve this machine, open:"
		echo
		echo "    $url"
		echo
		echo "  and check that it shows the code  $user_code"
		echo
	} >&2
	open_browser "$url"

	waited=0
	while [ "$waited" -lt "$expires_in" ]; do
		sleep "$interval"
		waited=$((waited + interval))
		status=$(curl -sS -o "${TMPDIR:-/tmp}/dots-poll.$$" -w '%{http_code}' -X POST \
			--data-urlencode "device_code=$device_code" "$origin/device/token") || status=000
		body=$(cat "${TMPDIR:-/tmp}/dots-poll.$$" 2>/dev/null || true)
		rm -f "${TMPDIR:-/tmp}/dots-poll.$$"
		case "$status" in
		200)
			echo "dots: approved" >&2
			printf '%s\n' "$body"
			return 0
			;;
		202 | 000) ;; # still waiting, or a network blip
		*)
			echo "dots: login ended ($status $body); run the command again" >&2
			exit 1
			;;
		esac
	done
	echo "dots: the code expired; run the command again" >&2
	exit 1
}

# Prints a key from the /device/code reply.
field() {
	printf '%s\n' "$reply" | sed -n "s/^$1=//p"
}

# Opens the link where a browser is likely: on a Mac, or a Linux desktop, and
# never over SSH (the browser would open on the far end, if anywhere).
open_browser() {
	[ -n "${DOTS_NO_BROWSER:-}" ] && return 0
	[ -n "${SSH_CONNECTION:-}" ] && return 0
	if [ "$(uname -s)" = Darwin ]; then
		open "$1" 2>/dev/null || true
	elif [ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ] && command -v xdg-open >/dev/null 2>&1; then
		xdg-open "$1" >/dev/null 2>&1 &
	fi
}

main "$@"
