#!/usr/bin/env bash
# Claude Code 단일 실행 + 구조화 메트릭 수집.
# 사용: ./claude.sh <prompt_file> <stdout_file> <stderr_file> <metrics_file> [model]
#
# `claude -p --output-format json` 의 JSON 출력을 1차 소스로 삼고,
# 벽시계/토큰/세션ID 를 claude 자체 메트릭에서 추출한다.
# date/gdate, stderr grep 같은 추정치는 쓰지 않는다.

set -euo pipefail

prompt_file=${1:?prompt_file required}
stdout_file=${2:?stdout_file required}
stderr_file=${3:?stderr_file required}
metrics_file=${4:?metrics_file required}
model=${5:-claude-opus-4-7}

command -v claude >/dev/null 2>&1 || { echo "claude not in PATH" >&2; exit 127; }
command -v jq     >/dev/null 2>&1 || { echo "bench requires jq" >&2; exit 127; }
command -v shasum >/dev/null 2>&1 || { echo "bench requires shasum" >&2; exit 127; }

prompt=$(cat "$prompt_file")
prompt_sha256=$(shasum -a 256 "$prompt_file" | awk '{print $1}')

# claude 의 JSON 출력은 stdout 으로 나간다. stderr 는 그대로 파일에 남긴다.
raw_json_file="${metrics_file}.raw.json"
set +e
claude -p "$prompt" --output-format json --model "$model" \
  >"$raw_json_file" 2>"$stderr_file"
exit_code=$?
set -e

# 기본값(모두 null). claude 가 비정상 종료하거나 JSON 이 깨졌으면 이 값이 유지된다.
wall_ms=null
api_ms=null
input_tokens=null
output_tokens=null
cache_read_tokens=null
cache_creation_tokens=null
num_turns=null
session_id=null
primary_model=null
result_text=""

# JSON 이 유효할 때만 필드 추출.
if [[ -s "$raw_json_file" ]] && jq -e . "$raw_json_file" >/dev/null 2>&1; then
  wall_ms=$(jq       'if .duration_ms               != null then .duration_ms               else null end' "$raw_json_file")
  api_ms=$(jq        'if .duration_api_ms           != null then .duration_api_ms           else null end' "$raw_json_file")
  input_tokens=$(jq  'if .usage.input_tokens        != null then .usage.input_tokens        else null end' "$raw_json_file")
  output_tokens=$(jq 'if .usage.output_tokens       != null then .usage.output_tokens       else null end' "$raw_json_file")
  cache_read_tokens=$(jq     'if .usage.cache_read_input_tokens     != null then .usage.cache_read_input_tokens     else null end' "$raw_json_file")
  cache_creation_tokens=$(jq 'if .usage.cache_creation_input_tokens != null then .usage.cache_creation_input_tokens else null end' "$raw_json_file")
  num_turns=$(jq     'if .num_turns                 != null then .num_turns                 else null end' "$raw_json_file")
  session_id=$(jq    'if .session_id                != null then (.session_id|tostring)     else null end' "$raw_json_file")
  result_text=$(jq -r 'if .result != null then .result else "" end' "$raw_json_file")

  # .modelUsage 에서 토큰 합이 가장 큰 키를 primary model 로 뽑는다.
  # Claude Code 는 메인 모델 + Haiku 서브에이전트로 혼합 실행될 수 있으므로,
  # 키가 2개 이상이면 stderr 에 경고를 남긴다(정직한 비교를 위해).
  model_usage_json=$(jq -c '.modelUsage // {}' "$raw_json_file")
  model_key_count=$(jq -r 'length' <<<"$model_usage_json")

  if [[ "$model_key_count" -gt 0 ]]; then
    primary_model=$(jq -r '
      to_entries
      | map({
          key,
          total: (
            ((.value.inputTokens           // .value.input_tokens           // 0)|tonumber) +
            ((.value.outputTokens          // .value.output_tokens          // 0)|tonumber) +
            ((.value.cacheReadInputTokens  // .value.cache_read_input_tokens  // 0)|tonumber) +
            ((.value.cacheCreationInputTokens // .value.cache_creation_input_tokens // 0)|tonumber)
          )
        })
      | sort_by(-.total)
      | .[0].key
    ' <<<"$model_usage_json")
    # jq 로 다시 JSON 문자열화(따옴표 포함)해 최종 JSON 에 안전하게 삽입한다.
    primary_model=$(jq -Rn --arg m "$primary_model" '$m')
  fi

  if [[ "$model_key_count" -gt 1 ]]; then
    printf 'warning: multi-model run detected, modelUsage=%s\n' \
      "$model_usage_json" >>"$stderr_file"
  fi
fi

# 어시스턴트 실제 텍스트를 stdout_file 에 저장(.result 필드).
printf '%s' "$result_text" >"$stdout_file"

# primary_model 이 null 이면 요청한 model 을 기록(요청 기준). 그러나 감사성을 위해
# 실제 응답자(primary_model)가 있으면 그 값을 우선한다.
if [[ "$primary_model" == "null" || -z "$primary_model" ]]; then
  model_field=$(jq -Rn --arg m "$model" '$m')
else
  model_field="$primary_model"
fi

prompt_sha_field=$(jq -Rn --arg s "$prompt_sha256" '$s')
tool_field=$(jq -Rn --arg s "claude" '$s')
provider_field=$(jq -Rn --arg s "anthropic" '$s')

tmp="${metrics_file}.tmp"
cat >"$tmp" <<EOF
{
  "schema_version": 1,
  "tool": $tool_field,
  "model": $model_field,
  "provider": $provider_field,
  "wall_ms": $wall_ms,
  "api_ms": $api_ms,
  "exit_code": $exit_code,
  "input_tokens": $input_tokens,
  "output_tokens": $output_tokens,
  "cache_read_tokens": $cache_read_tokens,
  "cache_creation_tokens": $cache_creation_tokens,
  "num_turns": $num_turns,
  "prompt_sha256": $prompt_sha_field,
  "session_id": $session_id
}
EOF

# 스키마 자체 검증: 모든 필드가 존재해야 한다(null 은 허용, 누락은 불허).
jq -e '
  (.schema_version          != null) and
  (has("tool"))              and
  (has("model"))             and
  (has("provider"))          and
  (has("wall_ms"))           and
  (has("api_ms"))            and
  (has("exit_code"))         and
  (has("input_tokens"))      and
  (has("output_tokens"))     and
  (has("cache_read_tokens")) and
  (has("cache_creation_tokens")) and
  (has("num_turns"))         and
  (has("prompt_sha256"))     and
  (has("session_id"))
' "$tmp" >/dev/null || {
  echo "claude.sh: metrics schema verification failed" >&2
  exit 2
}

mv "$tmp" "$metrics_file"
rm -f "$raw_json_file"

exit "$exit_code"
