#!/usr/bin/env bash
# bench 드라이버. 사용법:
#   ./run.sh --trials 3
#   ./run.sh --prompt glob_scan --trials 5
#   ./run.sh --tool harness --trials 3
#
# 각 (prompt, tool, trial) 조합을 순차 실행하고
# results/<ts>.csv 와 results/<ts>.md 를 쓴다.
#
# CSV 헤더:
#   prompt,tool,trial,wall_ms,exit_code,assistant_bytes,stderr_lines,tokens_in,tokens_out
#
# MD 는 prompt × tool 평균/중앙값/표준편차 표.

set -eu

trials=3
only_prompt=""
only_tool=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --trials) trials="$2"; shift 2 ;;
    --prompt) only_prompt="$2"; shift 2 ;;
    --tool) only_tool="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,12p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

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

ts=$(date -u +%Y%m%dT%H%M%SZ)
csv="results/${ts}.csv"
md="results/${ts}.md"
logdir="results/${ts}"
mkdir -p "$logdir"

echo "prompt,tool,trial,wall_ms,exit_code,assistant_bytes,stderr_lines,tokens_in,tokens_out" >"$csv"

for prompt in "${prompts[@]}"; do
  for tool in "${tools[@]}"; do
    runner="./${tool}.sh"
    [[ -x "$runner" ]] || chmod +x "$runner"
    for ((i=1; i<=trials; i++)); do
      so="$logdir/${prompt}.${tool}.${i}.stdout"
      se="$logdir/${prompt}.${tool}.${i}.stderr"
      mf="$logdir/${prompt}.${tool}.${i}.metrics"
      echo "-> $prompt / $tool / $i" >&2
      "$runner" "prompts/${prompt}.txt" "$so" "$se" "$mf" || true
      # metrics 로드
      wall_ms=""; exit_code=""; assistant_bytes=""; stderr_lines=""; tokens_in=""; tokens_out=""
      if [[ -f "$mf" ]]; then
        # shellcheck disable=SC1090
        . "$mf"
      fi
      printf '%s,%s,%d,%s,%s,%s,%s,%s,%s\n' \
        "$prompt" "$tool" "$i" \
        "${wall_ms:-}" "${exit_code:-}" "${assistant_bytes:-}" \
        "${stderr_lines:-}" "${tokens_in:-}" "${tokens_out:-}" \
        >>"$csv"
    done
  done
done

# 간단 집계 — awk 로 prompt×tool 별 median/mean/stddev
python3 - "$csv" "$md" "$ts" <<'PY'
import csv, statistics, sys
from collections import defaultdict

csv_path, md_path, ts = sys.argv[1], sys.argv[2], sys.argv[3]
rows = list(csv.DictReader(open(csv_path)))
cells = defaultdict(list)
for r in rows:
    try:
        w = int(r["wall_ms"])
    except ValueError:
        continue
    cells[(r["prompt"], r["tool"])].append(w)

with open(md_path, "w") as f:
    f.write(f"# Bench run {ts}\n\n")
    f.write("| prompt | tool | trials | median_ms | mean_ms | stdev_ms |\n")
    f.write("|---|---|---|---|---|---|\n")
    for (p, t), vs in sorted(cells.items()):
        if not vs:
            continue
        med = int(statistics.median(vs))
        mean = int(statistics.mean(vs))
        sd = int(statistics.pstdev(vs)) if len(vs) > 1 else 0
        f.write(f"| {p} | {t} | {len(vs)} | {med} | {mean} | {sd} |\n")
PY

echo
echo "raw:   $csv"
echo "summary: $md"
