#!/usr/bin/env bash
# Layer 2 of the Harness branch-protection stack.
#
# Invoked as a PreToolUse hook (see _base.json hooks.pre_tool_use).
# Reads the Harness tool-call payload from stdin and:
#   - Allows every tool that isn't Bash → pass through.
#   - Allows every Bash command that isn't `git push` → pass through.
#   - For `git push`, blocks any invocation whose target is (or could be)
#     a protected branch — main / dev / qa / master / release*. Feature
#     branches (feature/*, fix/*, chore/*, hotfix/*, bugfix/*, wip/*) and
#     any name not in the protected list are allowed.
#
# This exists because Harness's permission system uses shlex-prefix
# matching (see crates/harness-perm/src/lib.rs:152-154) and cannot
# distinguish `git push origin feature/x` from `git push origin main`.
# Fail-closed on timeout: _base.json sets on_timeout: "deny".
#
# Hardening notes (why this script looks paranoid):
#   - `set -o noglob` prevents the shell from expanding bare `*` in the
#     user's command when we loop over tokens.
#   - Empty command / missing Bash field → fail closed (block), not open.
#   - Matches `git push` even when preceded by an env assignment
#     (FOO=bar git push), `-C <dir>`, a full path (/usr/bin/git push),
#     or chained via `;`, `&&`, `||`, `|`, or newline.
#   - Blocks mass-push flags (--all / --mirror / --tags) because they
#     can update protected refs without an explicit refspec.
#   - If refspec is omitted, falls back to the current branch (what git
#     would actually push) and checks that.
set -eu
set -o noglob

payload=$(cat)

tool=$(printf '%s' "$payload" | jq -r '.tool_name // .tool // empty')
cmd=$(printf '%s' "$payload" | jq -r '.input.command // empty')

allow() { printf '{"action":"allow"}'; exit 0; }
block() {
  printf '%s' "$1" | jq -Rs '{action:"block", reason:.}'
  exit 0
}

# Only inspect Bash. Any other tool passes through.
[ "$tool" = "Bash" ] || allow

# Fail-closed on empty command (malformed payload). Layer 1 will still
# catch a real push; blocking here is the safe default.
[ -z "$cmd" ] && block "refused: empty Bash command payload (malformed tool call?)"

# Detect any `git push` anywhere in the command. Allow:
#   - leading env vars:    FOO=bar git push ...
#   - full path:           /usr/bin/git push ...
#   - -C <dir> prefix:     git -C /path push ...
#   - chaining:            ... ; git push, && git push, || git push, | git push
#   - newline chaining
# The regex looks for a statement separator (start-of-line, whitespace,
# ;, &, |, or newline) followed by optional env assignments, then
# git (possibly with a path or -C <dir>), then push.
#
# We do the detection with a Python one-liner to avoid relying on GNU
# grep features like -P or -z in every environment.
is_push=$(printf '%s' "$cmd" | python3 -c '
import re, sys
s = sys.stdin.read()
# Split on command separators so we can check each statement
# independently. This handles newlines + ; && || | all the same.
parts = re.split(r"[;\n]|&&|\|\||\|", s)
git_re = re.compile(
    r"""^\s*                        # leading whitespace
        (?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+)*   # optional env assignments
        (?:\S+/)?git                         # git (optionally with a path)
        (?:\s+-C\s+\S+)?                     # optional -C <dir>
        (?:\s+-[^\s]+)*                      # any other pre-push flags
        \s+push\b                            # the push subcommand
    """,
    re.VERBOSE,
)
for p in parts:
    if git_re.match(p):
        print("1")
        sys.exit(0)
print("0")
')

[ "$is_push" = "1" ] || allow

# At this point we know the command contains `git push`. From here
# everything is fail-closed: if we cannot confidently determine the
# target branch, we block and ask the operator to be explicit.

# Block mass-push flags unconditionally. `--all`/`--mirror`/`--tags`/`-a`
# can advance refs we cannot parse from the refspec alone.
case " $cmd " in
  *" --all "*|*" --mirror "*|*" --tags "*)
    block "refused: git push --all / --mirror / --tags can update protected refs. Push specific refspec instead." ;;
esac

# Extract the `git push ...` segment itself (the *last* one wins — if
# there are multiple, the later one is the one actually executed
# synchronously after the earlier allowances; but we check the refspec
# of all of them conservatively via the protected-name scan below).
#
# First: a quick regex scan for any protected branch name appearing as
# a standalone token in the command. This catches the common forms
# (`git push origin main`, `git push origin HEAD:main`,
# `git push origin +main:main`) without needing a full argv parser.
if printf '%s' "$cmd" | python3 -c '
import re, sys
cmd = sys.stdin.read()
# Match a protected name that appears either:
#   - as a bare token (preceded by whitespace / start / : / +)
#   - as the right side of a refspec (after :)
#   - wrapped in refs/heads/<name>
protected = r"(?:main|dev|qa|master|release(?:/[A-Za-z0-9._/-]+)?)"
pat = re.compile(
    r"(?:^|[\s:+])"              # boundary
    r"(?:refs/heads/)?"          # optional refs/heads/ prefix
    + protected +
    r"(?=[\s:]|$)"               # followed by whitespace, colon, or EOL
)
sys.exit(0 if pat.search(cmd) else 1)
'; then
  block "refused: git push references a protected branch (main/dev/qa/master/release*). Open a PR from a feature branch: git switch -c feature/<x> && git push -u origin feature/<x> && gh pr create --base <branch>."
fi

# If the command contains `git push` with no explicit refspec, the push
# goes to the current branch's upstream. Check the current branch.
#
# This is best-effort: we only reach this code path for push commands
# with no obvious protected token. If the current branch IS protected
# (e.g. the operator is sitting on main and runs a bare `git push`),
# we block.
current_branch=$(git symbolic-ref --short HEAD 2>/dev/null || printf '')
case "$current_branch" in
  main|dev|qa|master)
    # Only block the bare `git push` case — if the command already
    # has an explicit refspec (a third non-flag token after `push`),
    # the earlier scan would have caught a protected target.
    # We detect "no explicit refspec" heuristically: no slash-containing
    # token and no `:` in the portion after `push`.
    after_push=$(printf '%s' "$cmd" | awk 'BEGIN{FS="git[[:space:]]+push"} NR==1{print $2}')
    if ! printf '%s' "$after_push" | grep -Eq '[A-Za-z0-9_.-]+(:|/)' ; then
      block "refused: current branch is '$current_branch' (protected). Switch to a feature branch before pushing."
    fi
    ;;
  release|release/*)
    after_push=$(printf '%s' "$cmd" | awk 'BEGIN{FS="git[[:space:]]+push"} NR==1{print $2}')
    if ! printf '%s' "$after_push" | grep -Eq '[A-Za-z0-9_.-]+(:|/)' ; then
      block "refused: current branch is '$current_branch' (release branches are protected)."
    fi
    ;;
esac

allow
