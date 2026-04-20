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

# every repo needs the git-push guard hook (Layer 2 of branch protection):
mkdir -p scripts
cp <harness>/templates/settings/hooks/git_push_guard.sh scripts/harness-git-push-guard.sh
chmod +x scripts/harness-git-push-guard.sh

# every repo should also install the Layer 1 pre-push hook:
mkdir -p scripts/git-hooks
cp <harness>/scripts/git-hooks/pre-push scripts/git-hooks/pre-push
chmod +x scripts/git-hooks/pre-push
git config core.hooksPath scripts/git-hooks

# plpgsql repos additionally need the DB-write guard hook:
cp <harness>/templates/settings/hooks/dbwrite_guard.sh scripts/harness-dbwrite-guard.sh
chmod +x scripts/harness-dbwrite-guard.sh

# and critically — drop the Layer 3 Actions alarm into each gated repo:
mkdir -p .github/workflows
cp <harness>/templates/github/enforce-pr-only.yml .github/workflows/
```

## Policy summary (enforced by every template)

- **Read always allow** — `Read(**)`, `Glob(**)`, `Grep(**)`, plus read-only bash commands (`git log`, `git diff`, `git status`, `cat`, `ls`, etc). SSH itself is `ask` because we can't distinguish `ssh host cat file` from `ssh host rm -rf`.
- **Write ask once** — `Write(**)`, `Edit(**)`, plus file-moving bash commands (`rm`, `mv`, `cp`, `scp`, `rsync`, `sudo`).
- **Secrets deny** — `.env*`, `*.pem`, `*.key`, `.ssh/**`, `credentials.json`, `vault.yml`, `secrets/**`, `vault/**` for Read/Write/Edit.
- **Never push to protected branches (main/dev/qa)** — enforced by a 3-layer stack (see next section). Feature branch pushes succeed; `gh pr create` handles the PR side.
- **DB write = ask TWICE** — see next section.
- **Infra/cloud mutations ask** — `terraform apply`, `kubectl apply|delete|exec|replace|patch|edit`, `helm install|upgrade|uninstall|rollback`, `docker rm|rmi|system prune|volume rm`, `aws s3 cp|sync|rm`, `aws iam`, `aws ec2 terminate-instances`, `gcloud iam`, `gh release create`, `gh workflow run`, `gh secret set|delete`, `gh repo delete`, `gh api -X POST|PUT|DELETE|PATCH`. Read-only counterparts (`terraform plan`, `kubectl get|describe|logs`, `helm list|status`, `docker ps|images|inspect|logs`, `aws s3 ls`, `gh api repos`) are allow. `terraform destroy`, `docker push`, `docker run --privileged`, `gh release delete`, `gh workflow disable` are deny.

## Branch protection — 3-layer stack

GitHub server-side branch protection (both classic `branches/*/protection` and the newer Rulesets API) requires a paid Team/Enterprise plan for private repos. `runupcompany` is on Free, so both endpoints return HTTP 403 with `"Upgrade to GitHub Pro or make this repository public to enable this feature."`

Until the org upgrades, we enforce "no direct push to `main`/`dev`/`qa`" via three complementary layers:

| Layer | File | When it fires | Bypass |
|---|---|---|---|
| **1. Local git pre-push hook** | `scripts/git-hooks/pre-push` | Before any `git push` packet leaves the client — fires for Harness AND for a human typing in a plain terminal | `git push --no-verify`, or unsetting `core.hooksPath` |
| **2. Harness PreToolUse guard** | `templates/settings/hooks/git_push_guard.sh` | Every time the model invokes `Bash(git push ...)` inside Harness; parses argv + refspec and blocks protected targets | Running git outside Harness (Layer 1 catches that) |
| **3. GitHub Actions alarm** (PUBLIC repos only) | `templates/github/enforce-pr-only.yml` | After a push to `main`/`dev`/`qa` lands on the server — checks if the head commit has an associated merged PR; fails the run and opens an issue if not | Admin disabling Actions in repo settings |

**Why all three:** Layer 1 blocks the honest mistake but is bypassable locally. Layer 2 is the only place that can say "feature/* yes, main no" inside Harness (the permission system's shlex-prefix matcher can't distinguish them). Layer 3 is the backstop for `--no-verify` and non-Harness pushes — but it's reactive, so the bad commit lands before the alarm fires. None of the three is sufficient alone; all three make the mistake expensive enough to notice before anyone merges on top.

**Intentionally NOT doing auto-revert from Layer 3:** rewriting history on `main` mid-refactor causes more lost work than the policy violation it tries to undo. Alarm only.

### Layer 3 visibility gate — private repos skip it

Every `runupcompany` repo is currently private and the org is on the Free plan. GitHub Actions on private repos draws from the org's **2 000 min/month** free-tier quota (see https://docs.github.com/en/billing/managing-billing-for-your-products/managing-billing-for-github-actions/about-billing-for-github-actions). The team hasn't been running Actions, and paying for minutes just to enforce a policy that Layers 1 + 2 already catch locally is wasteful. So Layer 3 ships with a visibility gate:

```yaml
jobs:
  verify-merged-via-pr:
    if: ${{ github.event.repository.private == false }}
    ...
```

Behavior:

- **Private repo** (current state): workflow file exists on disk for self-documenting policy, but the single job is gated `private == false`, so every run skips instantly and costs **0 minutes**.
- **Public repo** (if ever flipped): the gate evaluates true automatically and the alarm turns on with no additional wiring — no PR, no manual step.

If the org upgrades to Team tier we replace Layer 3 with a real Ruleset (next subsection) and delete the workflow entirely.

### Upgrade path (when runupcompany moves to Team tier)

The equivalent Ruleset that Layers 1-3 approximate, for future `gh api -X POST repos/runupcompany/<repo>/rulesets`:

```json
{
  "name": "protect-main-dev-qa",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["refs/heads/main", "refs/heads/dev", "refs/heads/qa"], "exclude": [] } },
  "rules": [
    { "type": "pull_request", "parameters": { "required_approving_review_count": 1, "dismiss_stale_reviews_on_push": true, "require_code_owner_review": false, "require_last_push_approval": false, "required_review_thread_resolution": false } },
    { "type": "deletion" },
    { "type": "non_fast_forward" }
  ]
}
```

At that point Layer 3 (the Actions alarm) can be deleted; Layers 1 and 2 stay as defense-in-depth for the local workflow.

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

- `Bash(git push origin feature/*)` does **NOT** do what you think. `*` is a literal shlex token — the rule matches only the exact command `git push origin feature/*`. That's why we moved `Bash(git push)` into the `ask` bucket and wired a PreToolUse hook (`git_push_guard.sh`) that parses argv and decides allow vs block by target branch. `Bash(git push --force)` / `--force-with-lease` / `-f` stay in `deny` unconditionally.
- `Bash(mvn deploy)` matches `mvn deploy`, `mvn deploy -DskipTests`, `mvn deploy --settings x.xml` — shlex-prefix is exactly what we want for "deny the subcommand".
- `Bash(psql)` matches every psql invocation including `psql --version`, which is why we list `psql --version` / `psql -l` in `allow` BEFORE `psql` appears in `ask` — the specificity sort in `PermissionSnapshot::new` puts the longer prefix first.
- Quoted strings are ONE shlex token: `psql -c "UPDATE x"` tokenizes to `["psql", "-c", "UPDATE x"]`. You cannot put the SQL verb in a Bash rule pattern; that is why the dbwrite-guard hook inspects the raw command string instead.

## Validation

Every JSON file in this directory is round-tripped through `python3 -c 'import json; json.load(open(...))'` as part of the deploy checklist.
