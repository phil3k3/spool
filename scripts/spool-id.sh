#!/bin/sh
# Prints the reverse-DNS prefix for every macOS identifier Spool registers: the driver's
# bundle ID (<prefix>.spool.driver), the LaunchAgent (<prefix>.spoold) and the XPC broker's
# launchd label and Mach service (<prefix>.spool-broker).
#
# Resolved, in order, from:
#   1. $SPOOL_ID_PREFIX, if set (e.g. SPOOL_ID_PREFIX=com.example)
#   2. the `origin` remote: github.com/<owner>/... -> io.github.<owner>,
#      any other host/<owner>/...               -> <reversed host>.<owner>
#   3. local.spool, with a warning (no git, no origin, or a local-path remote)
#
# A fork therefore gets its own identifiers and can be installed next to upstream without
# the two registering the same launchd names. Used by crates/mach/build.rs and
# scripts/build-bundle.sh; the installer reads what was built from target/spool-id.
set -eu

valid() {
    printf '%s\n' "$1" | grep -Eq '^[a-z0-9-]+(\.[a-z0-9-]+)+$'
}

if [ -n "${SPOOL_ID_PREFIX:-}" ]; then
    id=$(printf '%s' "$SPOOL_ID_PREFIX" | tr '[:upper:]' '[:lower:]')
    if ! valid "$id"; then
        echo "spool-id: SPOOL_ID_PREFIX=$SPOOL_ID_PREFIX is not a reverse-DNS prefix like com.example" >&2
        exit 1
    fi
    printf '%s\n' "$id"
    exit 0
fi

repo="$(cd "$(dirname "$0")/.." && pwd)"
url=$(git -C "$repo" remote get-url origin 2>/dev/null || true)

host=""
owner=""
case "$url" in
    *://*)                              # https://host[:port]/owner/repo, ssh://git@host/owner/repo
        rest=${url#*://}
        rest=${rest#*@}
        hostport=${rest%%/*}
        host=${hostport%%:*}
        path=${rest#*/}
        [ "$path" != "$rest" ] && owner=${path%%/*}
        ;;
    *@*:*)                              # git@host:owner/repo.git
        rest=${url#*@}
        host=${rest%%:*}
        path=${rest#*:}
        owner=${path%%/*}
        ;;
esac
case "$url" in file://*) host="" ;; esac

if [ -z "$host" ] || [ -z "$owner" ]; then
    echo "spool-id: no usable 'origin' remote${url:+ ($url)}; using local.spool (set SPOOL_ID_PREFIX to choose)" >&2
    printf 'local.spool\n'
    exit 0
fi

# Lowercase; anything outside [a-z0-9-] becomes '-'.
clean() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9-]/-/g'; }
owner=$(clean "$owner")
host=$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')

case "$host" in
    github.com) base=io.github ;;       # <owner>.github.io is the domain the owner controls
    *) base=$(printf '%s' "$host" | awk -F. '{for (i = NF; i > 0; i--) printf "%s%s", $i, (i > 1 ? "." : "")}') ;;
esac

id="$base.$owner"
if ! valid "$id"; then
    echo "spool-id: cannot derive a prefix from $url (got '$id'); set SPOOL_ID_PREFIX" >&2
    exit 1
fi
printf '%s\n' "$id"
