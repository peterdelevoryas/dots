#!/bin/sh
# Installs your home directory from @ORIGIN@. You normally don't fetch this
# directly: `curl -fsSL @ORIGIN@/install | sh` gets you a token (through a
# browser login) and runs this with it in DOTS_TOKEN.
#
# Options (environment):
#   DOTS_VERSION=<ver>  install that version instead of the current one
#   DOTS_DRY_RUN=1      show what would change, touch nothing, skip setup.sh
#   DOTS_SKIP_SETUP=1   lay down files only
#
# Files that already exist and differ are moved to ~/.dots-backup-<time>/
# before being replaced.

# Everything is inside main so the whole script is read before any of it
# runs; otherwise a command reading stdin would eat the rest of the script.
main() {
	set -eu
	origin="@ORIGIN@"

	if [ -z "${DOTS_TOKEN:-}" ]; then
		echo "dots: no DOTS_TOKEN; run this instead:" >&2
		echo "  curl -fsSL $origin/install | sh" >&2
		exit 1
	fi

	tmp=$(mktemp -d "${TMPDIR:-/tmp}/dots.XXXXXX")
	trap 'rm -rf "$tmp"' EXIT INT TERM

	url="$origin/bundle"
	[ -n "${DOTS_VERSION:-}" ] && url="$url/$DOTS_VERSION"
	echo "dots: fetching $url" >&2
	curl -fsS -H "Authorization: Bearer $DOTS_TOKEN" -D "$tmp/headers" -o "$tmp/bundle.tar.gz" "$url"

	version=$(header x-dots-version)
	want=$(header x-dots-sha256)
	got=$(sha256 "$tmp/bundle.tar.gz")
	if [ -z "$want" ] || [ "$want" != "$got" ]; then
		echo "dots: checksum mismatch (expected '$want', got '$got')" >&2
		exit 1
	fi

	mkdir "$tmp/bundle"
	tar -xzf "$tmp/bundle.tar.gz" -C "$tmp/bundle"
	echo "dots: installing $version" >&2

	backup="$HOME/.dots-backup-$(date -u +%Y%m%dT%H%M%SZ)"
	changed=0
	if [ -d "$tmp/bundle/home" ]; then
		# Files and symlinks only; directories are created as needed.
		(cd "$tmp/bundle/home" && find . \( -type f -o -type l \) | sed 's|^\./||' | sort) >"$tmp/files"
		while IFS= read -r rel; do
			src="$tmp/bundle/home/$rel"
			dst="$HOME/$rel"
			same "$src" "$dst" && continue
			changed=$((changed + 1))
			if [ -n "${DOTS_DRY_RUN:-}" ]; then
				if [ -e "$dst" ] || [ -L "$dst" ]; then echo "  update  ~/$rel"; else echo "  create  ~/$rel"; fi
				continue
			fi
			if [ -e "$dst" ] || [ -L "$dst" ]; then
				mkdir -p "$(dirname "$backup/$rel")"
				mv "$dst" "$backup/$rel"
			fi
			mkdir -p "$(dirname "$dst")"
			if [ -L "$src" ]; then
				ln -s "$(readlink "$src")" "$dst"
			else
				cp -p "$src" "$dst"
			fi
			echo "  wrote   ~/$rel"
		done <"$tmp/files"
	fi

	if [ -n "${DOTS_DRY_RUN:-}" ]; then
		echo "dots: dry run: $changed file(s) would change; setup.sh not run" >&2
		return 0
	fi
	echo "dots: $changed file(s) changed" >&2
	[ -d "$backup" ] && echo "dots: previous versions saved in $backup" >&2

	if [ -z "${DOTS_SKIP_SETUP:-}" ]; then
		echo "dots: running setup.sh" >&2
		# Give setup.sh the terminal (for sudo and installer prompts) when
		# there is one; stdin is this script's pipe.
		if (: </dev/tty) 2>/dev/null; then
			(cd "$tmp/bundle" && DOTS_VERSION="$version" sh ./setup.sh </dev/tty)
		else
			(cd "$tmp/bundle" && DOTS_VERSION="$version" sh ./setup.sh </dev/null)
		fi
	fi

	mkdir -p "$HOME/.local/state/dots"
	echo "$version" >"$HOME/.local/state/dots/version"
	echo "dots: done ($version)" >&2
}

# Prints a response header's value from $tmp/headers.
header() {
	tr -d '\r' <"$tmp/headers" | awk -v name="$1" -F': ' 'tolower($1) == name { print $2 }' | tail -n 1
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d' ' -f1
	else
		shasum -a 256 "$1" | cut -d' ' -f1
	fi
}

# True if $2 already matches $1 (same symlink target, or same file contents).
same() {
	if [ -L "$1" ]; then
		[ -L "$2" ] && [ "$(readlink "$1")" = "$(readlink "$2")" ]
	else
		[ -f "$2" ] && [ ! -L "$2" ] && cmp -s "$1" "$2"
	fi
}

main "$@"
