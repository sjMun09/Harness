#!/usr/bin/env bash
# bench driver — rewrites for the new JSON metrics schema.
#
# SCHEMA (per trial): <logdir>/<prompt>.<tool>.<trial>.metrics.json
#   {
#     "prompt": "<name>", "tool": "harness|claude", "trial": <int>,
#     "model": "<model-id>", "provider": "<provider>",
#     "wall_ms": <int>, "api_ms": <int|null>, "exit_code": <int>,
#     "input_tokens": <int|null>, "output_tokens": <int|null>,
#     "cache_read_tokens": <int|null>, "cache_creation_tokens": <int|null>,
#     "num_turns": <int|null>,
#     "prompt_sha256": "<hex>", "session_id": "<string|null>"
#   }
#
# EQUALITY CHECKS (at aggregation time, before summary is written):
#   1. prompt_sha256 must be identical across ALL trials of a given prompt
#      (both tools) — otherwise "prompt drift detected".
#   2. Within each (prompt,tool) cell, model must be identical across trials
#      — otherwise "model drift within cell".
#   3. For each prompt, model must match ACROSS tools
#      — otherwise "model mismatch across tools, harness=X claude=Y".
#
# USAGE:
#   ./run.sh                              # all prompts, both tools, trials=20
#   ./run.sh --trials 5                   # override
#   ./run.sh --prompt cold_start          # one prompt
#   ./run.sh --tool harness               # one tool
#   ./run.sh --model claude-opus-4-7      # pass-through to runners
#
# Runner contract: "$runner" "prompts/<p>.txt" "$so" "$se" "$mf" "$model"
# Metrics are JSON; aggregation uses sample stdev (n>=2) and skips nulls per column.

set -euo pipefail

trials=20
only_prompt=""
only_tool=""
model="claude-opus-4-7"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --trials) trials="$2"; shift 2 ;;
    --prompt) only_prompt="$2"; shift 2 ;;
    --tool)   only_tool="$2"; shift 2 ;;
    --model)  model="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,30p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ "$trials" -lt 10 ]]; then
  echo "warning: n=$trials is statistically weak for latency claims" >&2
fi

script_dir=$(cd "$(dirname "$0")" && pwd)
cd "$script_dir"

prompts=()
if [[ -n "$only_prompt" ]]; then
  p="prompts/${only_prompt}.txt"
  [[ -f "$p" ]] || { echo "prompt not found: $p" >&2; exit 2; }
  prompts+=("$only_prompt")
else
  for p in prompts/*.txt; do
    name=$(basename "$p" .txt)
    prompts+=("$name")
  done
fi

tools=(harness claude)
if [[ -n "$only_tool" ]]; then
  tools=("$only_tool")
fi

command -v jq >/dev/null 2>&1 || { echo "jq required" >&2; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "python3 required" >&2; exit 2; }

ts=$(date -u +%Y%m%dT%H%M%SZ)
csv="results/${ts}.csv"
md="results/${ts}.md"
logdir="results/${ts}"
mkdir -p "$logdir"

echo "prompt,tool,model,provider,trial,wall_ms,api_ms,exit_code,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,num_turns,prompt_sha256,session_id" >"$csv"

for prompt in "${prompts[@]}"; do
  for tool in "${tools[@]}"; do
    runner="./${tool}.sh"
    [[ -x "$runner" ]] || chmod +x "$runner"
    for ((i=1; i<=trials; i++)); do
      so="$logdir/${prompt}.${tool}.${i}.stdout"
      se="$logdir/${prompt}.${tool}.${i}.stderr"
      mf="$logdir/${prompt}.${tool}.${i}.metrics.json"
      echo "-> $prompt / $tool / $i" >&2
      "$runner" "prompts/${prompt}.txt" "$so" "$se" "$mf" "$model" || true
      if [[ -f "$mf" ]]; then
        jq -r '[.prompt,.tool,.model,.provider,.trial,.wall_ms,.api_ms,.exit_code,.input_tokens,.output_tokens,.cache_read_tokens,.cache_creation_tokens,.num_turns,.prompt_sha256,.session_id] | @csv' "$mf" >>"$csv"
      else
        printf '"%s","%s",,,%d,,,,,,,,,,\n' "$prompt" "$tool" "$i" >>"$csv"
      fi
    done
  done
done

# Aggregation + equality checks + MD summary
python3 - "$logdir" "$md" "$ts" "$model" <<'PY'
import json, os, sys, glob, statistics
from collections import defaultdict

logdir, md_path, ts, cli_model = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

trials = []
for mf in sorted(glob.glob(os.path.join(logdir, "*.metrics.json"))):
    try:
        with open(mf) as f:
            trials.append(json.load(f))
    except Exception as e:
        print(f"error: cannot parse {mf}: {e}", file=sys.stderr)
        sys.exit(3)

if not trials:
    print("error: no metrics files found", file=sys.stderr)
    sys.exit(3)

# Equality check 1: prompt_sha256 identical across ALL trials of a prompt
by_prompt = defaultdict(list)
for t in trials:
    by_prompt[t["prompt"]].append(t)
for p, ts_list in by_prompt.items():
    shas = {t.get("prompt_sha256") for t in ts_list}
    if len(shas) > 1:
        print(f"error: prompt drift detected for prompt={p}: {sorted(shas)}", file=sys.stderr)
        sys.exit(4)

# Equality check 2: model identical within each (prompt,tool)
by_cell = defaultdict(list)
for t in trials:
    by_cell[(t["prompt"], t["tool"])].append(t)
for (p, tool), ts_list in by_cell.items():
    models = {t.get("model") for t in ts_list}
    if len(models) > 1:
        print(f"error: model drift within cell prompt={p} tool={tool}: {sorted(models)}", file=sys.stderr)
        sys.exit(5)

# Equality check 3: model matches ACROSS tools for each prompt
for p, ts_list in by_prompt.items():
    per_tool = {}
    for t in ts_list:
        per_tool.setdefault(t["tool"], t["model"])
    if len(set(per_tool.values())) > 1:
        h = per_tool.get("harness", "?")
        c = per_tool.get("claude", "?")
        print(f"error: model mismatch across tools, harness={h} claude={c} (prompt={p})", file=sys.stderr)
        sys.exit(6)

# Multi-model warning propagation — scan stderr files
multi_model_warnings = []
for t in trials:
    se = os.path.join(logdir, f"{t['prompt']}.{t['tool']}.{t['trial']}.stderr")
    if os.path.isfile(se):
        try:
            with open(se, errors="replace") as f:
                for line in f:
                    if "warning: multi-model run detected" in line:
                        multi_model_warnings.append((t["prompt"], t["tool"], t["trial"], line.strip()))
                        break
        except Exception:
            pass

def agg(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return None
    n = len(vals)
    med = statistics.median(vals)
    mean = statistics.mean(vals)
    sd = statistics.stdev(vals) if n >= 2 else None  # sample stdev
    return {"n": n, "median": med, "mean": mean, "stdev": sd,
            "min": min(vals), "max": max(vals)}

def fmt_int(x):
    return "-" if x is None else f"{int(round(x))}"

def fmt_range(a):
    if a is None:
        return "-"
    return f"{int(round(a['min']))}-{int(round(a['max']))}"

with open(md_path, "w") as f:
    f.write(f"# Bench run {ts}\n\n")
    f.write(f"- cli model arg: `{cli_model}`\n")
    f.write(f"- total trials: {len(trials)}\n\n")

    if multi_model_warnings:
        f.write("## Warnings\n\n")
        f.write("| prompt | tool | trial | message |\n|---|---|---|---|\n")
        for p, t, i, msg in multi_model_warnings:
            f.write(f"| {p} | {t} | {i} | {msg} |\n")
        f.write("\n")

    f.write("## Summary\n\n")
    f.write("| prompt | tool | model | n | wall_ms (median) | wall_ms (stdev) | wall_ms (min-max) | api_ms (median) | tokens_in+out (median) | num_turns (median) |\n")
    f.write("|---|---|---|---|---|---|---|---|---|---|\n")

    for (p, tool), ts_list in sorted(by_cell.items()):
        model_id = ts_list[0].get("model", "?")
        wall = agg([t.get("wall_ms") for t in ts_list])
        api = agg([t.get("api_ms") for t in ts_list])
        tokens_sum = []
        for t in ts_list:
            ti, to = t.get("input_tokens"), t.get("output_tokens")
            if ti is None and to is None:
                continue
            tokens_sum.append((ti or 0) + (to or 0))
        tok = agg(tokens_sum)
        turns = agg([t.get("num_turns") for t in ts_list])

        n = wall["n"] if wall else 0
        wall_med = fmt_int(wall["median"]) if wall else "-"
        wall_sd = "-" if (wall is None or wall["stdev"] is None) else f"{wall['stdev']:.1f}"
        wall_range = fmt_range(wall)
        api_med = fmt_int(api["median"]) if api else "-"
        tok_med = fmt_int(tok["median"]) if tok else "-"
        turns_med = fmt_int(turns["median"]) if turns else "-"
        f.write(f"| {p} | {tool} | {model_id} | {n} | {wall_med} | {wall_sd} | {wall_range} | {api_med} | {tok_med} | {turns_med} |\n")
PY

echo
echo "raw:     $csv"
echo "summary: $md"
echo "logs:    $logdir"
