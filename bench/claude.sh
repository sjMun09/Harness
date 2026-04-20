#!/usr/bin/env bash
# Claude Code 단일 실행 + 측정.
# 사용: ./claude.sh <prompt_file> <stdout_file> <stderr_file> <metrics_file>
#
# `claude -p <prompt>` 로 1턴 실행 (non-interactive print 모드).
# harness.sh 와 동일한 메트릭 스키마를 기록해 run.sh 가 동일 로직으로 집계할 수 있도록 한다.

set -u

prompt_file=${1:?prompt_file required}
stdout_file=${2:?stdout_file required}
stderr_file=${3:?stderr_file required}
metrics_file=${4:?metrics_file required}

command -v claude >/dev/null 2>&1 || { echo "claude not in PATH" >&2; exit 127; }

if command -v gdate >/dev/null 2>&1; then
  now_ms() { gdate +%s%3N; }
else
  now_ms() { date +%s%3N; }
fi

prompt=$(cat "$prompt_file")

start=$(now_ms)
claude -p "$prompt" >"$stdout_file" 2>"$stderr_file"
exit_code=$?
end=$(now_ms)
wall_ms=$((end - start))

assistant_bytes=$(wc -c <"$stdout_file" | tr -d ' ')
# Claude Code 는 툴 마커 기호가 다를 수 있으므로 근사만. ⏺ 또는 "tool:" 라인.
stderr_lines=$(LC_ALL=en_US.UTF-8 grep -cE '^(⏺ |\* tool:)' "$stderr_file" 2>/dev/null || echo 0)

# Claude Code --print 은 stdout 에 JSON 을 뱉는 모드도 있음. 기본은 텍스트.
tokens_in=""
tokens_out=""
# 단순 휴리스틱 — 향후 `claude -p --output-format json` 지원시 jq 로 교체.

{
  echo "tool=claude"
  echo "wall_ms=$wall_ms"
  echo "exit_code=$exit_code"
  echo "assistant_bytes=$assistant_bytes"
  echo "stderr_lines=$stderr_lines"
  echo "tokens_in=$tokens_in"
  echo "tokens_out=$tokens_out"
} >"$metrics_file"
