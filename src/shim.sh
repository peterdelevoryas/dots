#!/bin/sh
# Installs your home directory from @ORIGIN@.
#
#   curl -fsSL @ORIGIN@/install | sh
#
# This part is public. It asks you to approve the machine in a browser
# (you sign in with Google), then runs the real install script with the
# token it gets. The token lasts 30 days and is kept (in the macOS keychain,
# or ~/.config/dots/ elsewhere), so re-runs within that time skip the
# browser. With DOTS_TOKEN set, it uses that instead.
#
# Options (environment):
#   DOTS_VERSION=<ver>  install that version instead of the current one
#   DOTS_DRY_RUN=1      show what would change, touch nothing
#   DOTS_SKIP_SETUP=1   lay down files only
#   DOTS_NO_BROWSER=1   print the link instead of opening a browser
#   DOTS_NO_CACHE=1     don't use or keep a saved token
#   DOTS_LOGOUT=1       forget the saved token and exit
#
# For scripts (dotpush uses this):
#   DOTS_SCOPE=write        ask for write access instead of read
#   DOTS_PRINT_TOKEN=1      print the token instead of installing

# Everything is inside main so the whole script is read before any of it runs.
main() {
	set -eu
	origin="@ORIGIN@"
	scope=${DOTS_SCOPE:-read}
	case "$scope" in read | write) ;; *)
		echo "dots: DOTS_SCOPE must be read or write" >&2
		exit 1
		;;
	esac

	if [ -n "${DOTS_LOGOUT:-}" ]; then
		cache_drop
		echo "dots: forgot the saved $scope token" >&2
		return 0
	fi

	if [ -z "${DOTS_TOKEN:-}" ]; then
		DOTS_TOKEN=$(saved_token)
		if [ -z "$DOTS_TOKEN" ]; then
			DOTS_TOKEN=$(login)
			[ -n "${DOTS_NO_CACHE:-}" ] || cache_put "$DOTS_TOKEN"
		fi
	fi
	export DOTS_TOKEN

	if [ -n "${DOTS_PRINT_TOKEN:-}" ]; then
		printf '%s\n' "$DOTS_TOKEN"
		return 0
	fi
	curl -fsSL -H "Authorization: Bearer $DOTS_TOKEN" "$origin/bootstrap" | sh
}

# Prints the saved token if the server still accepts it for $scope;
# otherwise forgets it and prints nothing.
saved_token() {
	[ -n "${DOTS_NO_CACHE:-}" ] && return 0
	token=$(cache_get)
	[ -n "$token" ] || return 0
	who=$(curl -fsS -H "Authorization: Bearer $token" "$origin/whoami" 2>/dev/null) || who=
	level=${who##* }
	if [ "$level" = write ] || { [ -n "$who" ] && [ "$scope" = read ]; }; then
		echo "dots: using saved login (${who% *})" >&2
		printf '%s\n' "$token"
	else
		[ -n "$who" ] || echo "dots: saved login expired or was revoked" >&2
		cache_drop
	fi
}

# Token storage: the login keychain on macOS, a private file elsewhere. Keyed
# by server and scope.
cache_file() {
	echo "$HOME/.config/dots/$(printf '%s' "$origin" | sed 's|^https*://||; s|[^A-Za-z0-9.-]|_|g')-$scope"
}

cache_get() {
	if [ "$(uname -s)" = Darwin ]; then
		security find-generic-password -s dots-login -a "$origin $scope" -w 2>/dev/null || true
	else
		cat "$(cache_file)" 2>/dev/null || true
	fi
}

cache_put() {
	if [ "$(uname -s)" = Darwin ]; then
		security add-generic-password -U -s dots-login -a "$origin $scope" -w "$1" 2>/dev/null ||
			echo "dots: couldn't save the login to the keychain" >&2
	else
		mkdir -p "$HOME/.config/dots"
		(umask 077 && printf '%s\n' "$1" >"$(cache_file)")
	fi
}

cache_drop() {
	if [ "$(uname -s)" = Darwin ]; then
		security delete-generic-password -s dots-login -a "$origin $scope" >/dev/null 2>&1 || true
	else
		rm -f "$(cache_file)"
	fi
}

# Runs the browser login and prints the token it gets.
login() {
	name=$(hostname 2>/dev/null || uname -n)
	reply=$(curl -sS --fail-with-body -X POST --data-urlencode "name=$name" --data-urlencode "scope=$scope" "$origin/device/code") || {
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
