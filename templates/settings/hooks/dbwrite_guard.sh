#!/usr/bin/env bash
# Harness PreToolUse DB-write double-confirm guard.
#
# Invoked by the Harness hook dispatcher (crates/harness-core/src/hooks.rs).
# Receives the tool-call JSON on stdin, must emit a HookOutput JSON on stdout:
#   { "action": "allow" }                         # pass through
#   { "action": "block", "reason": "..." }        # deny (forces user to retry)
#
# Policy: if the bash command invokes psql/mysql/mariadb/pg_restore AND it
# contains a write verb (INSERT / UPDATE / DELETE / DROP / ALTER / TRUNCATE /
# GRANT / REVOKE / CREATE / REPLACE), block with a message instructing the
# operator to re-issue + re-approve. Since harness-perm's single Ask already
# happened and the user approved it, this hook provides the required SECOND
# confirmation by forcing another round-trip.
#
# The marker env var HARNESS_DBWRITE_CONFIRMED=1 bypasses the block; operators
# set it inline for the truly-intentional retry:
#   HARNESS_DBWRITE_CONFIRMED=1 psql -c "UPDATE ..."
#
# Dependencies: bash, jq. Exit 0 in all paths — the hook's action is in stdout.

set -eu

payload="$(cat)"

tool="$(printf '%s' "$payload" | jq -r '.tool_name // .tool // empty' 2>/dev/null || true)"
cmd="$(printf '%s'  "$payload" | jq -r '.input.command // .command // empty' 2>/dev/null || true)"

# Not a Bash invocation — nothing to guard.
if [ "$tool" != "Bash" ] || [ -z "$cmd" ]; then
    printf '{"action":"allow"}'
    exit 0
fi

# Only guard the DB client binaries we care about.
case "$cmd" in
    psql*|*\ psql\ *|mysql*|*\ mysql\ *|mariadb*|*\ mariadb\ *|pg_restore*|*\ pg_restore\ *) ;;
    *) printf '{"action":"allow"}'; exit 0 ;;
esac

# Uppercase the command for verb detection (POSIX tr).
upper="$(printf '%s' "$cmd" | tr '[:lower:]' '[:upper:]')"

# Match on word-boundary-ish fragments. Simple substring is acceptable here
# because false-positives (e.g. a table named 'INSERTIONS') erring on the side
# of a second prompt is the whole point.
case "$upper" in
    *INSERT*|*UPDATE*|*DELETE*|*DROP*|*ALTER*|*TRUNCATE*|*GRANT*|*REVOKE*|*CREATE*|*REPLACE*) is_write=1 ;;
    *) is_write=0 ;;
esac

if [ "$is_write" = "0" ]; then
    printf '{"action":"allow"}'
    exit 0
fi

# Escape hatch for the intentional retry.
if [ "${HARNESS_DBWRITE_CONFIRMED:-0}" = "1" ]; then
    printf '{"action":"allow"}'
    exit 0
fi

reason="DB write detected (INSERT/UPDATE/DELETE/DROP/ALTER/TRUNCATE/GRANT/REVOKE/CREATE/REPLACE). This is the SECOND confirmation required by policy. If you really want to run this, set HARNESS_DBWRITE_CONFIRMED=1 inline: HARNESS_DBWRITE_CONFIRMED=1 $cmd"

# Emit block JSON with the reason, properly escaped via jq.
printf '%s' "$reason" | jq -Rs '{action:"block", reason:.}'
exit 0
