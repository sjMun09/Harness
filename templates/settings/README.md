# Harness settings templates

Deployable `.harness/settings.json` templates for the 12 company repos.

## Repo → template map

| Repo                   | Language / type   | Template            | Branch |
|------------------------|-------------------|---------------------|--------|
| dynamos-back           | java (Spring+Mvn) | `java.json`         | dev    |
| dynamos-llm            | python (pytest)   | `python.json`       | main   |
| dynamos-front          | typescript (Vite) | `typescript.json`   | dev    |
| dynamos-snapshot       | typescript        | `typescript.json`   | main   |
| jira-claude            | typescript        | `typescript.json`   | main   |
| packages               | typescript (lib)  | `typescript.json`   | main   |
| dynamos-db             | plpgsql (migr.)   | `plpgsql.json`      | main   |
| dynamos-deploy         | plpgsql           | `plpgsql.json`      | main   |
| dynamos-ansible        | ansible-jinja     | `ansible.json`      | main   |
| dynamosconvert         | javascript (109M) | `generic.json`      | main   |
| dynamos-configuration  | config            | `generic.json`      | main   |
| dynamos-monitoring     | config            | `generic.json`      | main   |

## Deploy

```bash
# from the TARGET repo root:
mkdir -p .harness
cp <harness>/templates/settings/<lang>.json .harness/settings.json

# plpgsql repos additionally need the DB-write guard hook:
mkdir -p scripts
cp <harness>/templates/settings/hooks/dbwrite_guard.sh scripts/harness-dbwrite-guard.sh
chmod +x scripts/harness-dbwrite-guard.sh
```

## Policy summary (enforced by every template)

- **Read always allow** — `Read(**)`, `Glob(**)`, `Grep(**)`, plus read-only bash commands (`git log`, `git diff`, `git status`, `cat`, `ls`, etc). SSH itself is `ask` because we can't distinguish `ssh host cat file` from `ssh host rm -rf`.
- **Write ask once** — `Write(**)`, `Edit(**)`, plus file-moving bash commands (`rm`, `mv`, `cp`, `scp`, `rsync`, `sudo`).
- **Secrets deny** — `.env*`, `*.pem`, `*.key`, `.ssh/**`, `credentials.json`, `vault.yml`, `secrets/**`, `vault/**` for Read/Write/Edit.
- **Never push to protected branches** — `Bash(git push)` is denied entirely. `gh pr create` is allowed so feature work still reaches the remote. Operators push feature branches manually by bypassing the permission system (not Harness's concern).
- **DB write = ask TWICE** — see next section.

## Double-confirm for DB writes (plpgsql template)

harness-perm (`crates/harness-perm/src/lib.rs`) is 3-valued: `allow` / `ask` / `deny`. It has **no native "ask twice" mode.** However `crates/harness-core/src/hooks.rs` exposes a `PreToolUse` hook that is run for every tool call and can emit `{"action":"block","reason":"..."}` on stdout to veto the call AFTER the permission system has already approved it. That is our second confirmation.

Flow:

1. Model calls `Bash({"command": "psql -c \"UPDATE users SET ...\""})`.
2. harness-perm evaluates — matches `Bash(psql)` in the `ask` bucket → Ask → user approves (confirmation #1).
3. harness-core dispatches `PreToolUse` hook → `bash scripts/harness-dbwrite-guard.sh` receives the tool-call JSON on stdin.
4. The script greps for write verbs (`INSERT|UPDATE|DELETE|DROP|ALTER|TRUNCATE|GRANT|REVOKE|CREATE|REPLACE`) in the psql/mysql invocation. If any match, it returns `{"action":"block","reason":"..."}` telling the operator to set `HARNESS_DBWRITE_CONFIRMED=1` inline and retry — which produces confirmation #2.
5. Read-only psql (`SELECT`, `\d`, `\dt`) passes the hook silently.

Why a hook and not a wrapper binary: hooks are already in `Settings` (`crates/harness-core/src/config.rs` line 39, `hooks: BTreeMap<String, Vec<HookConfig>>`), run with a configurable timeout, and short-circuit on `block` (see `hooks.rs::HookDispatcher::dispatch`). A wrapper binary would require changing `$PATH` per repo and wouldn't compose with the existing permission bucket. Using the hook keeps everything declarative in settings.json.

The guard script is at `templates/settings/hooks/dbwrite_guard.sh`. It is referenced by `plpgsql.json`'s `hooks.pre_tool_use[].command` as `bash scripts/harness-dbwrite-guard.sh` — so the deploy step copies it under the target repo's `scripts/` folder. Hook timeout is 5000ms with `on_timeout: "deny"` (fail-closed).

## Shlex-prefix reminder (READ BEFORE EDITING ANY BASH RULE)

harness-perm's `Bash(...)` matcher uses **shlex token-prefix** matching, NOT glob. From `crates/harness-perm/src/lib.rs` line 152:

```rust
Matcher::ShlexPrefix(prefix) => bash_command(input).is_some_and(|cmd| {
    shlex::split(&cmd).is_some_and(|cmd_toks| starts_with_tokens(&cmd_toks, prefix))
}),
```

Consequences:

- `Bash(git push origin feature/*)` does **NOT** do what you think. `*` is a literal shlex token — the rule matches only the exact command `git push origin feature/*`. We use `Bash(git push)` in `deny` + explicit `Bash(git push origin main/dev/qa)` deny entries; any other push has to go through `gh pr create`.
- `Bash(mvn deploy)` matches `mvn deploy`, `mvn deploy -DskipTests`, `mvn deploy --settings x.xml` — shlex-prefix is exactly what we want for "deny the subcommand".
- `Bash(psql)` matches every psql invocation including `psql --version`, which is why we list `psql --version` / `psql -l` in `allow` BEFORE `psql` appears in `ask` — the specificity sort in `PermissionSnapshot::new` puts the longer prefix first.
- Quoted strings are ONE shlex token: `psql -c "UPDATE x"` tokenizes to `["psql", "-c", "UPDATE x"]`. You cannot put the SQL verb in a Bash rule pattern; that is why the dbwrite-guard hook inspects the raw command string instead.

## Validation

Every JSON file in this directory is round-tripped through `python3 -c 'import json; json.load(open(...))'` as part of the deploy checklist.
