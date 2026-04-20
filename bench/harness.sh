#!/usr/bin/env bash
# Harness 단일 실행 + 측정.
# 사용: ./harness.sh <prompt_file> <stdout_file> <stderr_file> <metrics_file>
#
# 출력 파일에 stdout/stderr 를 그대로 남기고, metrics_file 에는 key=value
# 라인으로 wall_ms/exit_code/assistant_bytes/stderr_lines 를 기록한다.
# 토큰 값은 Harness stderr 의 `[usage]` 라인에서 파싱 시도 (실패 시 빈 값).

set -u

prompt_file=${1:?prompt_file required}
stdout_file=${2:?stdout_file required}
stderr_file=${3:?stderr_file required}
metrics_file=${4:?metrics_file required}

command -v harness >/dev/null 2>&1 || { echo "harness not in PATH" >&2; exit 127; }

# macOS 는 gdate, Linux 는 date
if command -v gdate >/dev/null 2>&1; then
  now_ms() { gdate +%s%3N; }
else
  now_ms() { date +%s%3N; }
fi

prompt=$(cat "$prompt_file")

start=$(now_ms)
printf '%s\n' "$prompt" | harness ask - >"$stdout_file" 2>"$stderr_file"
exit_code=$?
end=$(now_ms)
wall_ms=$((end - start))

assistant_bytes=$(wc -c <"$stdout_file" | tr -d ' ')
stderr_lines=$(grep -c '^⏺ ' "$stderr_file" 2>/dev/null || echo 0)

tokens_in=""
tokens_out=""
if grep -q '\[usage\]' "$stderr_file"; then
  tokens_in=$(grep '\[usage\]' "$stderr_file" | tail -1 | sed -nE 's/.*in=([0-9]+).*/\1/p')
  tokens_out=$(grep '\[usage\]' "$stderr_file" | tail -1 | sed -nE 's/.*out=([0-9]+).*/\1/p')
fi

{
  echo "tool=harness"
  echo "wall_ms=$wall_ms"
  echo "exit_code=$exit_code"
  echo "assistant_bytes=$assistant_bytes"
  echo "stderr_lines=$stderr_lines"
  echo "tokens_in=$tokens_in"
  echo "tokens_out=$tokens_out"
} >"$metrics_file"
