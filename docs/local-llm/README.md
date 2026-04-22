# 로컬 LLM 런타임으로 Harness 쓰기

이 디렉터리는 harness 를 **로컬에서 돌아가는 LLM 런타임** 에 물릴 때의
런타임별 가이드를 모아둔 곳입니다. harness 자체는 Anthropic 과 OpenAI 두
벤더만 1 급으로 지원하지만, OpenAI provider 는 **OpenAI 호환 엔드포인트**
면 뭐든 받기 때문에, 대부분의 로컬 LLM 서버가 drop-in 으로 붙습니다.
아래 5 개 런타임은 그 중 쓸 만한 것들입니다.

---

## 공통 패턴

런타임이 무엇이든, harness 를 로컬 서버에 물리는 방법은 한 줄입니다:

```bash
harness ask \
  --model openai/<model-id> \
  --base-url http://localhost:<PORT>/v1 \
  "your question"
```

왜 이게 되는지:

1. **모델 prefix 로 provider 가 결정됩니다** — `--model` 값이 `openai/`,
   `gpt-`, `o1`, `o3`, `o4` 중 하나로 시작하면 harness 가 OpenAI provider
   경로로 라우팅합니다 (`crates/harness-cli/src/main.rs` 의
   `is_openai_model`).
2. **선행 `openai/` 는 떨어져 나갑니다** — harness 는 `openai/` 를 벗겨낸
   뒤 나머지를 모델명으로 `{base_url}/v1/chat/completions` 로 POST 합니다.
   즉 서버에 도착하는 모델명은 `<model-id>` 그 자체.
3. **localhost 면 API 키 생략 가능** — `--base-url` 이 `localhost` /
   `127.0.0.1` / `::1` 로 resolve 되면 `OPENAI_API_KEY` 환경변수가 없어도
   OK. harness 는 placeholder bearer (`local`) 로 전송하고, 로컬 런타임은
   보통 값을 검증하지 않습니다. 원격 base_url 에 대해선 여전히 key 필수
   (안전 기본값).
4. **`HARNESS_REFUSE_API_KEY=1` 락도 localhost 는 통과** — 로컬 추론은
   과금 경로가 아니기 때문에, 락이 걸려 있어도 loopback URL 로는 그대로
   나갑니다. 외부 과금 API 만 차단.

환경변수로 고정하고 싶으면 `export OPENAI_BASE_URL=...` 해두고 `--base-url`
플래그는 생략해도 같은 동작. 플래그가 env 보다 우선.

로컬 런타임이 `/v1/chat/completions` 만 OpenAI 스펙대로 구현해 두면,
harness 입장에선 OpenAI 가 움직이는 것과 구분되지 않습니다.

---

## 어떤 런타임이 어디에 맞는가

| 런타임 | 플랫폼 | 기본 포트 | 주요 강점 | 주요 약점 |
|---|---|---|---|---|
| [Ollama](ollama.md) | mac / linux / win | 11434 | 설치/모델 관리 쉬움, CLI + API 일체 | 툴콜 품질 모델 편차 |
| [vLLM](vllm.md) | linux + CUDA (주로) | 8000 | 고성능 배치, 프로덕션 서비스용 | CUDA 필수, 맥 비우대 |
| [LM Studio](lm-studio.md) | mac / linux / win GUI | 1234 | GUI 로 모델 검색 · 로드, MLX / llama.cpp 자동 선택 | 폐쇄 소스, 헤드리스 덜 편함 |
| [llama.cpp](llama-cpp.md) | 어디서나 | 8080 | GGUF 직접 제어, CPU-only 도 가능 | 모델 다운로드 · 퀀트 선택 직접 |
| [MLX](mlx.md) | Apple Silicon 전용 | 8080 | M 시리즈에서 최고 속도, 통합 메모리로 큰 모델 | Apple 전용, 툴콜 템플릿 편차 |

포트는 각 런타임의 기본값입니다 — 바꿀 수 있고, 실제로 llama.cpp 와
MLX 는 둘 다 8080 이라 동시에 띄울 땐 한 쪽을 옮겨야 합니다. 포트 충돌이
있으면 각 런타임의 서버 실행 플래그 (`--port`) 로 옮기고, harness 쪽
`OPENAI_BASE_URL` 에 바꾼 포트를 반영하면 끝입니다.

각 런타임의 전체 설치 · 기동 · 툴콜 세부사항은 아래 개별 문서를 보세요.
이 README 는 위 공통 패턴과 선택 가이드만 담고 있습니다.

---

## 어떻게 고를까

빠른 결정 가이드:

- **Mac, 처음 해보는 사람** → [LM Studio](lm-studio.md) (GUI 로 모델
  검색 / 로드 / 서버 켜기가 버튼 하나) 또는 [Ollama](ollama.md) (CLI 가
  편하면).
- **Mac, 최대 속도를 원함** → [MLX](mlx.md). 같은 모델 · 같은 4bit 기준으로
  llama.cpp Metal 보다 빠름.
- **Linux + NVIDIA, 일회성 / 개인용** → [Ollama](ollama.md). GPU 자동 감지
  + 모델 관리가 한 번에.
- **Linux + NVIDIA, 프로덕션 / 여러 유저** → [vLLM](vllm.md). 배치 · 연속
  배치 · 페이지드 어텐션으로 동시 요청 처리량이 압도적.
- **GGUF 퀀트를 내가 직접 고르고 싶음** → [llama.cpp](llama-cpp.md).
  HF 에 있는 거의 모든 GGUF 를 그대로 씀.
- **Windows, 전용 GPU 없음** → [Ollama](ollama.md) (CPU 모드) 또는
  [llama.cpp](llama-cpp.md). 작은 모델 (7B 이하) 을 깔끔하게 돌릴 수 있음.

런타임은 **언제든 바꿔도 됩니다**. harness 쪽 설정은 위 3 줄 env 뿐이라,
포트/모델 id 만 갈아끼우면 그대로 돕니다.

---

## 툴콜링 — 환상 없이 보기

harness 의 agent loop 는 모델이 **OpenAI `tool_calls` JSON 필드** 에 툴 호출을
제대로 실어 주는 걸 전제로 돕니다. 안타깝게도 오픈소스 모델의 툴콜 품질은
(1) 모델 자체의 파인튜닝, (2) 런타임의 파서, (3) tokenizer 의 chat template
세 축에 다 걸려 있어서, **"이 런타임을 쓰면 무조건 된다" 는 게 없습니다.**
같은 모델이라도 Ollama 에서는 잘 되고 MLX 에서는 안 되거나, Qwen 은
되는데 같은 크기 Gemma 는 안 되는 식으로 갈립니다.

실무 규칙:

1. 새 조합 (런타임 × 모델) 을 쓰기 전에 **항상** 짧은 툴 스모크로 확인.
   예: `harness ask "list files in ."` — 모델이 `Glob` / `Bash` 를 실제로
   호출해야 통과. 산문으로 `"You can run ls ..."` 라고 답하면 템플릿 /
   파서가 맞지 않는 것.
2. 스모크 실패 시 복잡한 태스크로 넘어가지 말고, 먼저 모델을 바꾸거나
   런타임을 바꿔서 루프 자체를 살리세요.
3. 같은 모델이라도 런타임을 바꾸면 결과가 달라집니다. 모델 선택과 런타임
   선택은 **동시에 실험할 축** 입니다.

각 런타임 문서의 "툴 콜링" 섹션에 관찰된 실패 모드를 모아 뒀습니다.

---

## 과금 사고 방지

`HARNESS_REFUSE_API_KEY=1` 은 harness 의 **OpenAI provider 경로를 통째로
차단** 합니다. 로컬 LLM 사용 중엔 이 락이 **꺼져 있어야** 동작합니다.
즉 평소엔 키고 (실수로 유료 API 호출 방지), 로컬 세션에서만 끄는 흐름이
가장 안전합니다.

셸 래퍼 예:

```bash
# ~/.zshrc
harness-local() {
  if [[ "$OPENAI_BASE_URL" != http://localhost:* \
     && "$OPENAI_BASE_URL" != http://127.0.0.1:* ]]; then
    echo "refusing: OPENAI_BASE_URL must point at localhost" >&2
    return 1
  fi
  unset ANTHROPIC_API_KEY
  HARNESS_REFUSE_API_KEY= harness "$@"
}
```

이걸 쓰면 세 가지가 같이 강제됩니다:

- `OPENAI_BASE_URL` 이 localhost 가 아니면 실행 거부.
- 혹시 env 에 남아 있을 `ANTHROPIC_API_KEY` 를 세션에서 제거.
- `HARNESS_REFUSE_API_KEY` 를 비워 OpenAI 경로만 이 호출에 한해 허용.

평소엔 `export HARNESS_REFUSE_API_KEY=1` 을 전역으로 유지하고, 로컬
실험은 `harness-local ask "..."` 로만 하세요. 실수로 유료 엔드포인트에
요청이 나가는 걸 구조적으로 막는 유일한 방법입니다.

---

## See also

- 프로젝트 상위 문서: [`/README.md`](../../README.md) — harness 전반 개요,
  CLI 문법, 권한 모델.
- 설계 문서: [`/PLAN.md`](../../PLAN.md) — provider 추상화, wire contract,
  security model.
- 각 런타임 상세:
  - [ollama.md](ollama.md)
  - [vllm.md](vllm.md)
  - [lm-studio.md](lm-studio.md)
  - [llama-cpp.md](llama-cpp.md)
  - [mlx.md](mlx.md)
