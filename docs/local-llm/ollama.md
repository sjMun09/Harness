# harness + Ollama (로컬 LLM)

> **런북 문서입니다.** `harness ask` / `harness session` 을 로컬
> [Ollama](https://ollama.com) 데몬에 연결해 API 요금 · 네트워크 왕복 없이
> 코딩 에이전트로 쓰는 최소 절차. macOS (darwin 24.3.0) 커맨드를 기본으로
> 보여주고 Linux 분기를 별도 표시합니다.

---

## TL;DR

harness 는 Anthropic 1 급 · OpenAI 2 급 구조고 (`README.md` §Claude Code 와의
비교), Ollama 는 자체 `/api/chat` 외에 OpenAI 호환 엔드포인트
`/v1/chat/completions` 를 노출합니다. 필요한 조작은 환경변수 3 개 +
`--model` 프리픽스 한 줄:

```bash
export OPENAI_BASE_URL=http://localhost:11434
export OPENAI_API_KEY=ollama     # 값 무엇이든 OK (빈 문자열만 X)
harness ask --model openai/qwen2.5-coder:14b "이 레포 구조 설명"
```

하드웨어 감(量): 7 ~ 8B 양자화 모델 ≈ 6 GB RAM, 13 ~ 14B ≈ 10 GB,
32B ≈ 22 GB, 70B 는 64 GB Unified Memory 또는 24 GB+ 전용 GPU. Apple
Silicon 은 Unified Memory 전부를 GPU 가 쓰므로 M2/M3 Max 64 GB 면 70B 를
"굴릴 수는" 있습니다 (토큰/s 는 별개). 이 문서대로 세팅하면 `harness ask`
는 네트워크 없이 Ollama 로만 통신합니다.

---

## 1. 설치 / 모델 받기

### 1.1 Ollama 설치

**macOS (Homebrew)**

```bash
brew install ollama
ollama serve                        # foreground 확인용
brew services start ollama          # 또는 백그라운드 상주
```

기본 listen: `127.0.0.1:11434`. 포트 변경 시
`OLLAMA_HOST=0.0.0.0:11500 ollama serve`, 이때 `OPENAI_BASE_URL` 도
동기화.

**Linux (curl 설치 스크립트)**

```bash
curl -fsSL https://ollama.com/install.sh | sh
sudo systemctl enable --now ollama    # systemd 상주
```

설치 후 생존 확인:

```bash
curl -s http://localhost:11434/api/tags | head -c 200
# → {"models":[...]} 나오면 OK
```

### 1.2 모델 pull

harness 는 코딩 에이전트이므로 **코더 튠 모델**을 권장합니다. 범용
instruct 튠은 툴 호출 JSON 포맷이 불안정하거나 긴 system prompt 에
일관성을 잃는 경우가 많습니다.

| 모델 태그 | 대략 VRAM/RAM | 용도 |
|---|---|---|
| `qwen2.5-coder:7b` | ~6 GB | 빠른 프로토타이핑 · 로컬 스모크 테스트 |
| `qwen2.5-coder:14b` | ~10 GB | **일반 권장 — 코더 튠 + tool_use 대응이 현재 로컬 모델 중 가장 안정적** |
| `qwen2.5-coder:32b` | ~22 GB | Apple Silicon 32 GB+ 또는 24 GB GPU |
| `deepseek-coder-v2:16b` | ~10 GB | 코드 생성 품질 보수적, 약간 verbose |
| `llama3.1:8b-instruct-q4_K_M` | ~6 GB | 일반 목적. 코드 리팩토링 품질은 코더 튠 대비 아래 |

권장 (본 문서의 예시):

```bash
ollama pull qwen2.5-coder:14b
```

받아진 모델 목록 확인:

```bash
ollama list
# NAME                        ID            SIZE    MODIFIED
# qwen2.5-coder:14b           ...           8.9 GB  ...
```

### 1.3 모델 워밍업 (선택)

첫 호출 cold-start (디스크 → RAM/GPU 로 모델 올리기) 는 14B 기준
수 초 ~ 십 수 초. 미리 올리려면:

```bash
ollama run qwen2.5-coder:14b "ok"     # 첫 토큰까지 로드. 이후 종료
ollama ps                             # 현재 로드된 모델 + keep_alive 잔여
```

기본 `keep_alive` 는 5 분. 길게 잡으려면 서버 기동 시
`OLLAMA_KEEP_ALIVE=2h ollama serve` (§4.3 참고). harness 가 요청
body 에 `keep_alive` 를 직접 넣어 주지는 않으므로 서버 설정이 가장 깔끔.

---

## 2. harness 설정

### 2.1 왜 `openai/` 프리픽스가 필요한가

harness 는 `--model` 문자열의 **prefix** 만 보고 provider 를 라우팅합니다
(`crates/harness-cli/src/main.rs:899` 의 `is_openai_model`):

```rust
fn is_openai_model(model: &str) -> bool {
    let m = model.trim();
    m.starts_with("openai/")
        || m.starts_with("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
}
```

`qwen2.5-coder:14b` 를 그대로 넘기면 harness 는 Anthropic 모델로 취급해
`ANTHROPIC_API_KEY` 를 찾으러 갑니다. 로컬 Ollama 모델은 OpenAI 라우트로
가야 하므로 프리픽스가 필요합니다.

OpenAI provider 를 만들 때 harness 는 프리픽스를 **스트립**합니다
(`main.rs:855`):

```rust
let model_norm = model.strip_prefix("openai/").unwrap_or(model).to_string();
```

즉 wire 상으로는 `{"model": "qwen2.5-coder:14b", ...}` 가 Ollama 로
그대로 전달됩니다. **`--model` 값은 `ollama list` 의 태그와 문자 단위로
일치**해야 함 (콜론, 태그, 대소문자 포함).

### 2.2 환경변수 3 개

```bash
# Ollama OpenAI 호환 엔드포인트의 루트. harness 가 내부적으로
# /v1/chat/completions 를 붙입니다 (openai.rs:118-122). Url::join 이
# leading-slash 로 경로를 치환하기 때문에 다음 두 값은 동일한 최종
# URL 이 됩니다: http://localhost:11434  ==  http://localhost:11434/v1
# 혼동 방지용으로 루트만 넘기는 쪽을 권장.
export OPENAI_BASE_URL=http://localhost:11434

# 값은 무엇이든 OK — 단 빈 문자열은 거부 (openai.rs:64-66).
export OPENAI_API_KEY=ollama

# 과금 경로 차단 (선택, §7 wrapper 에서 강제).
unset ANTHROPIC_API_KEY
```

두 변수 모두 harness 기동 시 읽힙니다. `.zshrc` 영구 등록 또는 §7 의
shell 함수로 감싸는 것을 권장.

### 2.3 첫 호출

```bash
harness ask --model openai/qwen2.5-coder:14b "이 레포의 테스트 구조 한 단락으로 요약"
```

stderr 의 `[auth] api-key (OPENAI_API_KEY) provider=openai` 배너가 나오면
정상 (= Ollama 로 라우팅). `provider=anthropic` 이면 프리픽스 오타.

---

## 3. 검증 (smoke test)

harness 문제인지 Ollama 문제인지 분리하려면 **항상 curl 을 먼저** 쏴 봅니다.
harness 까지 가기 전에 Ollama 자체의 OpenAI-호환 경로가 살아 있는지부터
확인하는 게 디버깅 시간을 크게 줄입니다.

### 3.1 Ollama 직접

```bash
curl -s http://localhost:11434/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:14b",
    "messages": [{"role": "user", "content": "say ok"}],
    "stream": false
  }' | jq .
```

기대:

```json
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "choices": [
    { "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }
  ],
  ...
}
```

404 가 나오면 `--model` 값이 `ollama list` 의 tag 와 불일치.
`connection refused` 면 `ollama serve` 가 떠 있지 않거나 포트 충돌.

### 3.2 스트리밍 smoke

harness 는 SSE 스트리밍을 요구합니다 (`openai.rs:3-4`). Ollama 는
`"stream": true` 로 SSE 를 지원:

```bash
curl -N http://localhost:11434/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen2.5-coder:14b","messages":[{"role":"user","content":"count 1 to 5"}],"stream":true}'
```

`data: {...}` 프레임이 여러 개 흐르다 `data: [DONE]` 로 끝나면 정상.

### 3.3 harness 경유

```bash
harness ask --model openai/qwen2.5-coder:14b "hi"
```

단답으로 `hi` 비슷한 게 돌아오면 끝. 이 지점까지 오면
**"harness 가 실제로 로컬 모델과 말하고 있다"** 가 확증됩니다.
이후 `harness session` 으로 resume 해도 같은 경로를 탑니다.

---

## 4. 성능 · 주의 사항

### 4.1 스트리밍

- Ollama OpenAI-호환 엔드포인트는 SSE 를 지원합니다. harness 는 자동으로
  `stream: true` 를 붙여 보냅니다 (OpenAI provider 의 wire contract,
  `openai.rs:3-4`). 별도 설정 불필요.
- harness 에는 **1 MiB SSE 프레임 하드 캡** 이 있습니다 (`openai.rs:40`).
  로컬 모델이 거대한 단일 프레임을 토해내는 경우는 실전에선 거의 없으나,
  커스텀 템플릿으로 대형 blob 을 반환하게 만들면 프레임이 드랍될 수 있습니다.

### 4.2 Tool-calling 은 모델 의존적

로컬 모델 도입 시 **가장 자주 깨지는 지점**입니다.

- harness 는 내장 툴 14 종 (`README.md` §내장 툴) 을 assistant 의
  `tool_use` 블록으로 호출합니다. OpenAI provider 는 wire 상으로
  `tools: [...]` 요청과 `tool_calls: [...]` 응답 포맷을 기대합니다.
- Ollama 는 `/v1/chat/completions` 에서 `tools` 파라미터를 엔드포인트
  수준에서는 받지만, 실제 유효한 `tool_calls` JSON 을 뱉는지는
  **모델이 그 포맷으로 튜닝되어 있느냐** 에 달려 있습니다.
- 검증 권장:
  1. `harness ask --model openai/<model> "hi"` 로 텍스트 왕복 확인.
  2. `harness ask --model openai/<model> "list rust files in this repo"` —
     `⏺ Glob(...)` 배너가 안 찍히면 해당 모델은 harness 루프와 맞지 않음.
- 현재(2026-04) 경향:
  `qwen2.5-coder:14b` / `:32b` = 안정, `llama3.1:8b` = 간단한 툴만,
  `deepseek-coder-v2:16b` = 코드 품질 좋으나 tool_calls 누락 빈도 높음.

모델이 tool_calls 를 **안 뱉고 자연어로 "Glob 을 호출하겠습니다"** 만
쓰면 harness 는 텍스트로 받고 턴을 끝냅니다. 디버깅은
`harness --verbose ask ...` 로 실제 SSE 프레임 확인이 가장 빠릅니다.

### 4.3 Cold-start 지연

- 첫 `ask` 호출에서 모델을 디스크 → 메모리 로 올리는 데 14B 기준
  수 초 ~ 십 수 초 걸립니다. 이 시간 동안 harness stderr 는 조용합니다.
- 해결: `ollama ps` 로 로드 상태 확인, 자주 쓴다면 `ollama run <model> ""`
  으로 미리 올려 두거나 `OLLAMA_KEEP_ALIVE=2h ollama serve` 로 서버 쪽
  기본 keep-alive 를 늘립니다.

### 4.4 Context window

Ollama 의 기본 `num_ctx` 는 모델/서버 설정에 따라 **2048 ~ 8192** 로
좁게 잡혀 있는 경우가 흔합니다. harness 의 토큰 budget 은 `tiktoken-rs`
(cl100k_base) 근사라서 로컬 모델 토크나이저와 정확히 일치하지 않습니다.
실전에선 "모델은 8k 를 받는데 프롬프트가 10k 라 서버가 잘라버림" 을
가장 조심.

해결: `Modelfile` 로 `num_ctx` 를 늘린 custom 태그를 만듭니다.

```bash
cat > Modelfile.qwen14b-16k <<'EOF'
FROM qwen2.5-coder:14b
PARAMETER num_ctx 16384
EOF
ollama create qwen2.5-coder-16k:14b -f Modelfile.qwen14b-16k
harness ask --model openai/qwen2.5-coder-16k:14b "..."
```

`num_ctx` 를 늘리면 VRAM/RAM 사용량이 비례해서 커집니다.

---

## 5. 트러블슈팅

### 5.1 `OPENAI_API_KEY not set` 또는 `OPENAI_API_KEY is empty`

증상:

```
error: build OpenAI provider — is OPENAI_API_KEY set?
caused by: OPENAI_API_KEY not set
```

원인: harness 의 OpenAI provider 는 `OPENAI_API_KEY` 를 필수로
확인합니다 (`openai.rs:62-66`). 비어 있어도 에러입니다.

해결:

```bash
export OPENAI_API_KEY=ollama
```

Ollama 는 이 값을 검증하지 않으므로 문자열 내용은 무엇이든 무방합니다.

### 5.2 `404 model not found` / `the model '<name>' was not found`

증상: curl 의 응답 body, 또는 harness stderr 에 4xx.

원인: `--model` 뒤에 넘긴 태그가 `ollama list` 에 없거나 오타.
"colon 이하 tag" 까지 정확히 일치해야 합니다.

해결:

```bash
ollama list                                 # 있는 태그 목록 확인
ollama pull qwen2.5-coder:14b               # 없으면 받기
harness ask --model openai/qwen2.5-coder:14b "..."
```

### 5.3 `connection refused` / `error sending request`

원인 (상위 빈도 순):

1. `ollama serve` 가 떠 있지 않다 → `brew services start ollama` 또는
   `ollama serve` 재실행.
2. `OPENAI_BASE_URL` 오타 (`http://locahost` 등) → 직접 `curl` 로 확인.
3. 포트 11434 가 다른 프로세스에 잡혀 있다 → `lsof -i :11434`.
   다른 데몬이 잡고 있으면 `OLLAMA_HOST=127.0.0.1:11500 ollama serve`
   로 옮기고 `OPENAI_BASE_URL` 도 동기화.

### 5.4 모델이 gibberish 를 내거나 시스템 프롬프트를 무시

증상: harness 가 `HARNESS.md` · 권한 system prompt 를 제공했는데도
모델이 일반 대화처럼 답하고 툴을 호출하지 않음.

원인 후보:

- **양자화가 너무 공격적** — `q3_K_M` / `q2_K` 급은 instruction-following
  능력이 크게 떨어짐. `q4_K_M` / `q5_K_M` / `q8_0` 으로 올리기.
- **모델 크기가 시스템 프롬프트에 비해 작음** — harness system prompt 는
  툴 14 개 JSON schema + HARNESS.md 포함 수 KB. 3 ~ 4B 급은 일관성
  유지 실패. 최소 7B, 실전 13 ~ 14B.
- **non-coder instruct 튠** — `llama3:8b` vs `qwen2.5-coder:14b`. 후자 권장.

해결 1 차: `qwen2.5-coder:14b` (q4_K_M). 안 되면 `:32b`. 그래도 안 되면
로컬 모델 한계.

### 5.5 Tool 루프가 중간에 끊긴다 (`end_turn` 없이 턴 종료)

증상: assistant 가 첫 툴을 호출해 결과를 받은 후, 다음 턴에서
`tool_calls` 를 안 뱉고 자연어만 내뱉으며 `end_turn`.

원인: 모델이 tool-use 포맷 (`tool_calls: [{function: {...}}]`) 을 놓치는
경우. Ollama 가 응답에 `tool_calls` 필드를 비워 보내면 harness 는
그걸 그대로 받아들이고 루프를 빠져나옵니다.

해결:

- tool-use 품질이 확인된 모델로 교체 (§4.2 리스트 참고).
- `harness --verbose ask ...` 로 실제 원 프레임 확인 —
  assistant 가 tool_calls 를 보낸 적이 있는지.
- 프롬프트에 "툴을 반드시 써서 답하라" 류 직접 지시를 명시.

### 5.6 harness 가 갑자기 Anthropic 으로 가려고 한다

증상: stderr 에 `[auth] oauth` 또는 `ANTHROPIC_API_KEY` 요구 에러.

원인: `--model` 에 프리픽스 빠짐. `qwen2.5-coder:14b` 는 OpenAI 패턴에
매칭되지 않으므로 harness 가 Anthropic 라우트로 빠집니다.

해결: `--model openai/qwen2.5-coder:14b` 로 프리픽스 붙이기.

---

## 6. 성능 팁

- **권한/plan-gate 는 느슨하게 쓰지 말 것.** 로컬 모델은 prompt injection /
  잘못된 tool_use 생성에 더 취약. `settings.json` 의 `permissions.ask` /
  `deny` 는 Anthropic 쓸 때와 동일하게 유지 (`README.md` §권한 문법).
- **세션 resume** 은 그대로 동작 — 턴 히스토리가 JSONL 에 쌓이고 다음
  turn 의 user 메시지로 replay 되기 때문에 wire 계약 이슈 없음.
- **모델 디스크 위치**: macOS `~/.ollama/models`, Linux
  `/usr/share/ollama/.ollama/models` (systemd). 여러 개 받으면 수십 GB 이므로
  `ollama rm <tag>` 로 정리.
- **동시 요청 1 개 제한.** Ollama 는 모델당 1 슬롯이 기본
  (`OLLAMA_NUM_PARALLEL` 로 조정 가능). harness 를 여러 shell 에서 동시에
  띄우면 뒤에 온 요청이 대기.

---

## 7. 보안 · 비용 — "네트워크로 나가지 않음" 을 강제

### 7.1 로컬 고정은 환경변수만으로는 불충분

`OPENAI_BASE_URL=http://localhost:11434` 로 두는 한 harness 의 OpenAI
요청은 로컬로만 갑니다. 다만 "새는" 경로는 있습니다:

1. 실수로 `--model gpt-4o` + `OPENAI_BASE_URL=localhost` → Ollama 가 해당
   모델을 못 찾아 404 로 안전하게 실패. OK.
2. `unset OPENAI_BASE_URL` 상태에서 `--model gpt-4o` → 기본값
   `https://api.openai.com` 으로 가서 **과금**. 위험.
3. `--model claude-opus-4-7` + `ANTHROPIC_API_KEY` 살아있음 또는
   키체인 OAuth 토큰 존재 → Anthropic 으로 과금. 위험.

### 7.2 `HARNESS_REFUSE_API_KEY` 는 만능이 아님

harness 에는 `HARNESS_REFUSE_API_KEY=1` 류 빌링 락이 있지만, 이는
**API key 경로 전체를 막는 스위치** 이지 "localhost 만 허용" 이 아닙니다.
켜면 Ollama 로 향하는 OpenAI provider 도 함께 막힙니다 (OpenAI provider
는 `OPENAI_API_KEY` 를 필수로 요구하기 때문).

**로컬 전용** 을 달성하려면 shell wrapper 로 호출 직전에 두 가지를
검사하는 것이 실전적입니다:

- `OPENAI_BASE_URL` 이 `http://localhost` 또는 `http://127.0.0.1` 로 시작
- `ANTHROPIC_API_KEY` 가 unset

### 7.3 Wrapper 한 줄 예시 (`~/.zshrc` 에 넣기)

```bash
# `hl <prompt>` 로 호출하면 반드시 로컬 Ollama 로만 감
hl() {
  unset ANTHROPIC_API_KEY
  export OPENAI_API_KEY="${OPENAI_API_KEY:-ollama}"
  export OPENAI_BASE_URL="${OPENAI_BASE_URL:-http://localhost:11434}"
  case "$OPENAI_BASE_URL" in
    http://localhost*|http://127.0.0.1*) ;;
    *)
      echo "hl: refusing — OPENAI_BASE_URL is not localhost: $OPENAI_BASE_URL" >&2
      return 1
      ;;
  esac
  harness ask --model "openai/${HARNESS_LOCAL_MODEL:-qwen2.5-coder:14b}" "$@"
}
```

이 함수를 거치면:

- `ANTHROPIC_API_KEY` 가 항상 언셋되어 Anthropic 경로가 차단됨.
- `OPENAI_BASE_URL` 이 실수로 hosted 로 바뀌어 있으면 즉시 거부.
- 모델은 `HARNESS_LOCAL_MODEL` env 로 간단히 전환:
  `HARNESS_LOCAL_MODEL=qwen2.5-coder:32b hl "..."`.

bash 사용자는 `zsh` 구문 그대로 bash 에서도 동작합니다.

### 7.4 방화벽 / 감사

- `pfctl` (macOS) / `iptables` (Linux) 로 harness 프로세스의 outbound
  `443` 차단. Ollama 는 평문 `11434` 라 영향 없음.
- `harness session show <id>` 로 트랜스크립트 감사 — 모델의 모든 툴
  호출이 기록됨. 세션 파일은 0600 perm 으로
  `$XDG_STATE_HOME/harness/sessions/` 에 저장됩니다.

---

## 8. 참고 · 더 읽을 거리

- `README.md` §빠른 시작 — harness 자체 기본 사용법.
- `README.md` §Claude Code 와의 비교 — OpenAI provider 가 "2 급" 인 이유.
- `PLAN.md` §5.12 — `ProviderError` 분류. 로컬 서버 오류 디버깅 시 유용.
- `crates/harness-provider/src/openai.rs` — OpenAI provider 구현 전체.
  `OPENAI_BASE_URL` 처리, SSE 파서, tool_calls 매핑 모두 이 파일 안.
- `crates/harness-cli/src/main.rs:843-906` — provider 라우팅 / 프리픽스
  처리 / 모델 매칭 규칙.
- [Ollama OpenAI 호환성 블로그](https://ollama.com/blog/openai-compatibility) —
  `/v1` 엔드포인트의 공식 명세.
- [Ollama `Modelfile` 문법](https://github.com/ollama/ollama/blob/main/docs/modelfile.md) —
  `num_ctx` 등 파라미터 오버라이드.

---

## 부록 — Anthropic 과 번갈아 쓰기

두 벤더를 자주 오가는 사용자는 §7.3 의 `hl` 함수 옆에 Anthropic 용
alias 를 하나 더 두면 전환이 한 글자로 끝납니다.

```bash
ha() {
  unset OPENAI_API_KEY OPENAI_BASE_URL
  export ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY first}"
  harness ask --model "${HARNESS_REMOTE_MODEL:-claude-opus-4-7}" "$@"
}
```

주의: `harness session resume` 은 **세션을 만들 때 쓴 provider 와 동일한**
provider 로 재개해야 합니다. Anthropic 으로 만든 세션을 Ollama 로
resume 하면 이전 턴의 `tool_use` 문맥 해석이 모델에 따라 드리프트할 수
있습니다.
