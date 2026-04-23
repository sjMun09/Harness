# Egress Redaction

## Threat

`harness-mem` redacts session JSONL at write time (`crates/harness-mem/src/redact.rs`): `sk-…`, GitHub PATs, AWS keys, Google API keys, Slack tokens, PEM headers become `[REDACTED:<kind>]` before landing on disk. The on-disk transcript is clean.

But those same tool outputs are sent **raw** to the LLM provider on the next turn. If a shell command prints an API key, the provider sees the real value. For regulated / enterprise users this is a real leak vector.

## Options

1. **Always-on.** Safest; breaks model-threads-token flows.
2. **Opt-in (chosen).** Default off; users with compliance needs flip it on.
3. **Tool-output-only.** Scrub `ToolResult`, leave all text — model echoing secrets defeats it.
4. **Warn-only telemetry.** Zero real protection.
5. **Scan-and-deny.** Refuse secret-laden tool output. Transparent but unrecoverable on false positive.

## Decision: opt-in

When `HARNESS_REDACT_EGRESS=1` (or `harness.redact_egress: true` in `settings.json`), the provider scrubs:

- `ContentBlock::ToolResult.content` — primary leak vector.
- Assistant `ContentBlock::Text` — the model may echo tool-output secrets.

Out of scope: **user `Text`** (user typed it) and **`ToolUse.input`** (synthesized from already-redacted context).

## Rationale

Patterns stay in `harness-mem`; the provider calls `harness_mem::redact::redact_str` directly. Single source of truth means opt-in operators get on-disk/on-wire parity by construction. Default-off preserves current token-threading workflows.

## Known false positives

The regex set is conservative. Long Base64 blobs starting with `sk-`/`ghp_` trigger the rule; identifiers matching `AIza…`/`AKIA…` get replaced. If it breaks a legitimate flow, unset the flag for that session.

## Migration

No-op by default. To enable:

```bash
export HARNESS_REDACT_EGRESS=1
```

or in `settings.json`:

```json
{ "harness": { "redact_egress": true } }
```

The CLI prints `[security] egress redaction ON` at startup when active.
