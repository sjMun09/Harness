# HARNESS.md — project conventions

Self-hosted convention document. Loaded at session start (global `~/.config/harness/HARNESS.md` then this file overrides). Also used by `PreToolUse` hooks as the canonical reference for "how we do things here."

## Rust style

- `rustfmt` default with `rustfmt.toml` overrides (100-col width).
- `clippy -D warnings`. `pedantic` warn, `nursery` off. Workspace-level lints in root `Cargo.toml`.
- `#![forbid(unsafe_code)]` at every crate root. Opt back in **per-module only**, with a comment explaining why (syscall wrappers in `harness-tools::fs_safe::linux` and `harness-tools::proc`).
- Error types with `thiserror` in libraries; `anyhow` only in the CLI binary.
- No panics outside `todo!()` scaffolding. Replace `unwrap`/`expect` with `?` before merging.

## Module organization

- Crate ownership: `harness-proto` (leaf types), `harness-core` (traits + turn loop), `harness-perm`/`harness-mem`/`harness-token` (utilities), `harness-tools`/`harness-provider`/`harness-tui` (impls), `harness-cli` (wiring).
- `harness-proto` is **semver-frozen after iter 1 exit**. Breaking changes forbidden. Additive-only via new variants / new fields (`#[serde(default)]`).

## Commits

- Conventional Commits: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`, `perf:`, `build:`, `ci:`.
- Scope in parentheses, e.g. `feat(tools): add Edit replace_all flag`.
- Body explains **why**, not **what** (diff already shows what).

## Testing

- Unit tests in `#[cfg(test)] mod tests` at the bottom of each file.
- Integration tests in `tests/` with a `common/` module for shared fixtures (mock provider lives inline in `tests/common/mod.rs` for iter 1; `harness-testkit` is iter 2).
- **Every `Tool` impl must have a `preview()` snapshot test** (via `insta`) — preview is user-facing and must not regress silently.
- Differential (`DiffExec`) and E2E tests are first-class: the turn loop must pass them as part of `/build` exit criteria (§9 of `PLAN.md`).

## PR discipline

- One logical change per PR.
- `harness-proto` changes require a separate PR and an explicit callout in the description.
- CI must pass (`fmt`, `clippy`, `test`, `size-gate <= 45 MiB`).

## Security (§8.2 of `PLAN.md`)

- Path ops via `harness-tools::fs_safe::canonicalize_within` — never call `std::fs::canonicalize` directly in tools.
- Bash tool: argv mode default; `shell=true` requires explicit user opt-in.
- Env allowlist for child processes: `PATH`, `HOME`, `LANG`, `TERM`, `USER`. Any additions require a HARNESS.md update.
- `HOME` is on the allowlist for toolchain UX (git/cargo/npm/...). Per-call `sandbox_home: true` on the `Bash` input rewrites `HOME` + XDG base dirs to a fresh tempdir for security-sensitive invocations. Foreground only. See `docs/security/home-env.md` for the threat model + rationale.
- API keys via env (`ANTHROPIC_API_KEY`) only — `settings.json` plaintext rejected.
- Untrusted external content (file reads, command output, hook `additionalContext`) must be fenced with `<untrusted_tool_output>` / `<untrusted_hook>` before being passed to the model.

## Refactoring conventions (for agent-driven tasks)

- Before rewriting, check existing sibling files for convention patterns (e.g. bucket-pattern XML vs direct Freemarker loops) — match what's already there unless the user explicitly asks to change it.
- When touching legacy `<mapper>` / Freemarker templates: run `ImportTrace` first to map the include chain, then `DiffExec` with ≥4 sample inputs after editing.
