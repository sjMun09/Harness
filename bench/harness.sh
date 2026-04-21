#!/usr/bin/env bash
# Harness 단일 실행 + 메트릭 수집.
# 사용: ./harness.sh <prompt_file> <stdout_file> <stderr_file> <metrics_file> [model]
#
# harness 바이너리 자체가 `--metrics-json <path>` 플래그로 구조화 메트릭을
# 원자적으로 기록한다. 이 스크립트는 그 플래그를 넘겨 harness 를 호출하고,
# 종료 코드와 메트릭 파일 존재만 확인한다. date/gdate 나 stderr grep 같은
# 추정치는 사용하지 않는다.
#
# harness 가 비정상 종료했거나 메트릭 파일을 남기지 못한 경우에만,
# run.sh 가 행(row)을 만들 수 있도록 fallback JSON 을 이 스크립트가 대신 쓴다.
# fallback JSON 에서 실제로 계산하는 필드는 prompt_sha256 하나뿐이다.

set -euo pipefail

prompt_file=${1:?prompt_file required}
stdout_file=${2:?stdout_file required}
stderr_file=${3:?stderr_file required}
metrics_file=${4:?metrics_file required}
model=${5:-claude-opus-4-7}

command -v harness >/dev/null 2>&1 || { echo "harness not in PATH" >&2; exit 127; }
command -v shasum  >/dev/null 2>&1 || { echo "bench requires shasum" >&2; exit 127; }

prompt=$(cat "$prompt_file")
prompt_sha256=$(shasum -a 256 "$prompt_file" | awk '{print $1}')

# bench 는 비대화형 실행이므로 --trust-cwd 와 --dangerously-skip-permissions 는 필수다.
# 메트릭은 harness 자체가 --metrics-json 경로에 원자적으로 기록한다.
set +e
harness --model "$model" ask \
  --metrics-json "$metrics_file" \
  --trust-cwd \
  --dangerously-skip-permissions \
  "$prompt" \
  >"$stdout_file" 2>"$stderr_file"
exit_code=$?
set -e

# 정상 종료이고 메트릭 파일이 남아 있으면 harness 가 쓴 값을 그대로 신뢰한다.
if [[ "$exit_code" -eq 0 && -s "$metrics_file" ]]; then
  exit 0
fi

# 실패 경로: harness 가 메트릭 파일을 쓰지 못했거나 비어 있다.
# run.sh 가 집계할 수 있도록 스키마에 맞는 fallback JSON 을 남긴다.
tmp="${metrics_file}.tmp"
cat >"$tmp" <<EOF
{
  "schema_version": 1,
  "tool": "harness",
  "model": "$model",
  "provider": null,
  "wall_ms": null,
  "api_ms": null,
  "exit_code": $exit_code,
  "input_tokens": null,
  "output_tokens": null,
  "cache_read_tokens": null,
  "cache_creation_tokens": null,
  "num_turns": null,
  "prompt_sha256": "$prompt_sha256",
  "session_id": null
}
EOF
mv "$tmp" "$metrics_file"

exit "$exit_code"
