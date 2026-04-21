# Harness × vLLM — 로컬 LLM 서빙 런북

> **한 줄 요약.** `vllm serve <hf-model>` 로 OpenAI 호환 서버를 띄우고,
> `OPENAI_BASE_URL=http://localhost:8000/v1` + `OPENAI_API_KEY=<아무거나>` 를
> 건 뒤 `harness ask --model openai/<hf-model> "..."` 을 치면 로컬 모델로
> Harness 가 돈다. 이 문서는 그 조합을 "에이전트 루프가 실제로 끝까지
> 도는" 수준까지 맞추기 위한 운영용 가이드.

이 문서는 README.md 와 문체 맞춤 (한/영 혼용, 단정체). 대상 독자는
이미 Harness 는 Anthropic 키로 쓸 줄 아는 사람. 처음이라면 먼저
[`README.md`](../../README.md) 의 "빠른 시작" 섹션부터.

---

## 1. 소개 / TL;DR

**vLLM** 은 PagedAttention · continuous batching · FlashAttention 을
내장한 프로덕션급 LLM 추론 서버다. OpenAI Chat Completions API 와
와이어 레벨 호환이라 Harness 의 OpenAI 프로바이더가 그대로 붙는다.

### Ollama · llama.cpp 대신 vLLM 을 고르는 이유

| 축 | Ollama | llama.cpp | **vLLM** |
|---|---|---|---|
| 일차 타겟 | 로컬 데스크탑 UX | 모든 플랫폼 CPU/GPU | **데이터센터 GPU 서빙** |
| 배칭 | single-request 위주 | 제한적 | **continuous batching (PagedAttention)** |
| Throughput | 낮음 | 중간 | **가장 높음** (동시 요청 많을 때 선형적) |
| Apple Silicon | 1급 지원 | 1급 지원 (Metal) | **비공식** (vllm-metal / vllm-mlx 플러그인) |
| CUDA 요구 | 선택 | 선택 | **사실상 필수** (정공법) |
| 설치 난이도 | 아주 쉬움 | 쉬움 | 중간 (Python · CUDA 매칭 필요) |

**언제 vLLM 을 고르는가.**
- 24GB+ CUDA GPU 가 있고 한 번 띄워서 여러 에이전트·세션을 붙일 예정.
- 14B–70B 급을 FP16 / AWQ / FP8 로 돌려 품질을 지키고 싶다.
- 서버 한 대를 팀원이 공유하고 싶다 (Ollama 는 단일 사용자에 최적).

**언제 vLLM 을 고르지 말아야 하는가.**
- Apple Silicon Mac 에서 혼자 씀 → [`mlx.md`](./mlx.md) 또는 [`ollama.md`](./ollama.md).
- CPU-only 머신 → llama.cpp (GGUF 포맷) 가 훨씬 안정적.
- "그냥 한 줄 띄우고 싶다" → Ollama.

### 하드웨어 감각 (2026-04 기준, 대략)

| VRAM | 돌릴 수 있는 모델 | 비고 |
|---|---|---|
| 12GB | 7B FP16, 8B AWQ | Qwen2.5-Coder-7B 가 현실적 상한 |
| 16GB | 8B FP16, 14B AWQ/Q4 | Llama-3.1-8B 가 편함 |
| 24GB | 14B FP16, 32B AWQ | Qwen2.5-Coder-14B-Instruct 편안 |
| 48GB | 32B FP16, 70B AWQ/FP8 | A6000 / L40S 급 |
| 2×24GB | 70B AWQ (TP=2) | `--tensor-parallel-size 2` |
| 2×80GB | 70B FP16 | A100/H100 |

여기서 "편하게" 는 `--max-model-len 16384` 정도에서 KV 캐시가 충분히
남아 Harness 의 툴 루프(턴당 누적 컨텍스트 증가) 가 OOM 없이 도는 상태.

---

## 2. 설치

### 2.1. 플랫폼 확인

**macOS 사용자는 여기서 멈춘다.** vLLM 의 Apple Silicon 지원은 비공식 플러그인
(`vllm-metal`, `vllm-mlx`) 경로라 빌드 실패·성능 저하가 빈번하다. 공식 권고:

- Apple Silicon → [`mlx.md`](./mlx.md) (MLX 네이티브 Metal).
- Apple Silicon 에서 Ollama 처럼 쓰고 싶다 → [`ollama.md`](./ollama.md).

이 문서는 **Linux + NVIDIA CUDA GPU** 전제.

### 2.2. CUDA 확인

2026-04 기본 wheel 은 CUDA 13.0 타겟, CUDA 12.1+ 드라이버면 자동 감지.
다른 버전이 필요하면 `VLLM_MAIN_CUDA_VERSION` 으로 강제.

```bash
nvidia-smi   # "CUDA Version: 12.x" 이상이어야 함
```

### 2.3. 설치 (권장: uv)

```bash
curl -LsSf https://astral.sh/uv/install.sh | sh   # uv 가 없으면 먼저
uv venv --python 3.11 .venv-vllm
source .venv-vllm/bin/activate
uv pip install vllm
```

대체 (pip): `python -m venv .venv-vllm && source .venv-vllm/bin/activate && pip install -U pip vllm`.

### 2.4. 설치 확인

```bash
python -c "import vllm; print(vllm.__version__)"
vllm --help | head -5
```

`vllm serve` 서브커맨드가 보이면 성공.

---

## 3. 서버 기동

### 3.1. 최소 커맨드 (도구 호출 없음)

```bash
vllm serve Qwen/Qwen2.5-Coder-14B-Instruct \
  --dtype auto \
  --gpu-memory-utilization 0.9
```

기본값으로 `http://0.0.0.0:8000` 에 뜨고, API prefix 는 `/v1` 이라
chat completions 엔드포인트는 `http://localhost:8000/v1/chat/completions`
가 된다. 이건 Harness 가 `OPENAI_BASE_URL` 뒤에 `/v1/chat/completions`
를 붙여서 POST 하는 주소와 정확히 일치한다 (`crates/harness-provider/src/openai.rs:118`).

### 3.2. 풀 커맨드 (Harness 가 툴 콜을 쓰려면 이 쪽)

```bash
vllm serve Qwen/Qwen2.5-Coder-14B-Instruct \
  --host 0.0.0.0 \
  --port 8000 \
  --dtype auto \
  --gpu-memory-utilization 0.9 \
  --max-model-len 16384 \
  --enable-auto-tool-choice \
  --tool-call-parser hermes \
  --api-key sk-local-harness
```

### 3.3. 플래그 요약

| 플래그 | 의미 | 언제 바꾸나 |
|---|---|---|
| `<model>` | HuggingFace repo ID 또는 로컬 경로 | 그대로 repo ID 를 넣으면 최초 호출 시 다운로드 |
| `--host` | 바인드 주소. 기본 `0.0.0.0` | 같은 머신에서만 쓸 거면 `127.0.0.1` 로 좁히는 게 안전 |
| `--port` | 기본 `8000` | 다른 서비스와 충돌 시 |
| `--dtype` | `auto` / `float16` / `bfloat16` / `float8_e4m3fn` | Pascal·Volta 는 bf16 미지원 → `float16` 강제 |
| `--gpu-memory-utilization` | KV 캐시에 쓸 VRAM 비율. 기본 0.9 | CUDA OOM 나면 0.85 → 0.80 순으로 낮춤 |
| `--max-model-len` | 컨텍스트 상한 | 모델 기본이 32K/128K 여도 Harness 턴 루프엔 16K 면 충분. 낮추면 KV 캐시 여유 생김 |
| `--enable-auto-tool-choice` | 모델이 툴 호출을 직접 결정하게 허용 | **Harness 쓰려면 필수** (§5 참조) |
| `--tool-call-parser` | 모델 패밀리별 툴 파서 | `hermes` / `llama3_json` / `mistral` / `deepseek_v3` / `granite` 등 |
| `--api-key` | Bearer 토큰. 설정 시 클라이언트가 값 일치해야 통과 | 생략하면 아무 토큰이나 통과 |
| `--tensor-parallel-size` | 다중 GPU 분산 (샤딩) | 2×24GB 로 70B 돌릴 때 `2` |
| `--quantization` | `awq` / `gptq` / `fp8` 등 | 저정밀 체크포인트 쓸 때 |
| `--chat-template` | Jinja 템플릿 경로 | 모델이 내장 템플릿이 없거나 이상할 때 |

### 3.4. `--api-key` 정책

- **생략**: 아무 Bearer 토큰이든 통과 (`OPENAI_API_KEY` 값 무관, 단 비어 있으면 안 됨).
- **설정**: 클라이언트가 보낸 Bearer 토큰이 정확히 일치해야 통과.

Harness 의 OpenAI 프로바이더는 `OPENAI_API_KEY` 가 비어 있지 않기만 하면 빌드되고
값 그대로 `Bearer` 헤더에 실린다 (`crates/harness-provider/src/openai.rs:62-67`, `:136`).
같은 네트워크에 다른 사람이 있다면 `--api-key` 를 반드시 설정하라.

---

## 4. harness 설정

Harness 는 `--model` 값의 prefix 로 프로바이더를 결정한다
(`crates/harness-cli/src/main.rs:899-906`):

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

vLLM 이 띄우는 HuggingFace 모델 (`Qwen/...`, `meta-llama/...`) 는
`gpt-`/`o1` 접두사가 없으므로 **반드시 `openai/` 를 앞에 붙여야** OpenAI
경로로 라우트된다. 경로에 들어간 뒤엔 Harness 가 **첫 번째 `openai/`
만** 스트립한다 (`crates/harness-cli/src/main.rs:855`):

```rust
let model_norm = model.strip_prefix("openai/").unwrap_or(model).to_string();
```

즉 CLI 에 `openai/Qwen/Qwen2.5-Coder-14B-Instruct` 를 넣으면 서버로
전송되는 `model` 필드는 `Qwen/Qwen2.5-Coder-14B-Instruct` 가 되고,
이건 vLLM 이 `vllm serve` 시 등록한 이름과 정확히 일치해야 한다.

### 4.1. 동작하는 최소 예

```bash
export OPENAI_BASE_URL=http://localhost:8000/v1
export OPENAI_API_KEY=sk-local-harness   # vllm serve --api-key 와 동일해야 함

harness ask --model openai/Qwen/Qwen2.5-Coder-14B-Instruct \
  "이 레포 구조를 한 단락으로 설명해줘"
```

### 4.2. 자주 하는 실수

- `--model Qwen/Qwen2.5-Coder-14B-Instruct` (prefix 누락) → Anthropic 프로바이더로 라우트되어 api.anthropic.com 404/401. **반드시 `openai/` prefix.**
- `OPENAI_BASE_URL=http://localhost:8000/v1/chat/completions` (전체 경로) → Harness 가 `/v1/chat/completions` 를 또 붙여서 중복 404.
- `OPENAI_API_KEY=""` → `ProviderError::Auth("OPENAI_API_KEY is empty")` 즉시 실패 (`openai.rs:64-66`).

### 4.3. `settings.json` 으로 고정하기

매번 `--model` 치기 귀찮으면 프로젝트의 `.harness/settings.json` 에:

```jsonc
{
  "version": 1,
  "model": "openai/Qwen/Qwen2.5-Coder-14B-Instruct",
  "max_turns": 20
}
```

그리고 쉘에서 env 만 걸고:

```bash
export OPENAI_BASE_URL=http://localhost:8000/v1
export OPENAI_API_KEY=sk-local-harness
harness ask "로컬 모델이 잘 붙었나 확인"
```

---

## 5. 툴 콜링 / 함수 호출

Harness 의 코딩 루프는 전적으로 tool calling 에 의존한다. Read/Grep/
Edit/Bash 호출이 `tool_use` 블록으로 나와야 엔진이 이걸 dispatch 한다.
tool call 이 plain text 로 새면 모델은 계속 "파일 읽는 척" 만 하고
실제 툴이 안 돌아서 에이전트가 영원히 수렴하지 않는다.

### 5.1. 필수 플래그

```
--enable-auto-tool-choice
--tool-call-parser <parser>
```

이 둘이 없으면 vLLM 은 tool call JSON 을 assistant content 에 그대로
박아 넣고 OpenAI 스펙의 `tool_calls[]` 필드는 비워 둔다. Harness 는
`tool_calls[]` 를 보고 dispatch 하므로, 이 경우 엔진이 호출할 툴이
없다고 판단하고 턴을 끝낸다. 증상은 "모델이 뭔가 쓰긴 했는데 파일은
안 건드린다" → `--max-turns` 직전까지 비생산적으로 소비.

### 5.2. 모델별 파서 매칭

| 모델 패밀리 | `--tool-call-parser` | 비고 |
|---|---|---|
| Qwen2.5 / Qwen3 / Hermes 계열 | `hermes` | Qwen/Qwen2.5-Coder-\* 는 이 값. |
| Llama-3.1 / 3.2 Instruct | `llama3_json` | 추가로 `--chat-template examples/tool_chat_template_llama3.1_json.jinja` 권장 |
| Mistral-Instruct | `mistral` | Mistral-v0.3+. |
| DeepSeek-V3 / DeepSeek-Coder-V3 | `deepseek_v3` | |
| IBM Granite | `granite` / `granite-20b-fc` | |
| GLM-4 MoE | `glm4_moe` | |

**파서 이름은 vLLM 버전에 따라 추가된다.** 최신 목록은
`vllm serve --help | grep -A2 tool-call-parser` 또는 공식 문서의
[Tool Calling](https://docs.vllm.ai/en/stable/features/tool_calling/) 참조.

### 5.3. 잘못된 파서 선택 시 증상

| 증상 | 실제 원인 |
|---|---|
| Harness 가 `⏺` 툴 호출 로그를 찍지 않고 assistant 만 반복 출력 | 파서가 tool call JSON 을 추출 못해 plain text 로 흘려 보냄 |
| 턴이 `max_turns` 까지 찍고 종료 | 위와 동일 (루프 종료 조건 `end_turn` 이 안 떨어짐) |
| `ProviderError::Parse("...")` 가 stderr 에 뜸 | 파서가 잘못된 JSON 을 뱉어 vLLM 응답이 스키마 위반 |

### 5.4. 첫 스모크 테스트 순서

1. **텍스트 only** — `harness ask "hi, what model are you?"`. 툴 없이 한 번 왕복이 도는지 확인.
2. **간단한 Read** — `harness ask "read README.md and summarize"`. `⏺ Read(README.md)` 로그가 stderr 에 찍혀야 정상.
3. **편집** — `harness ask "add a trailing newline to PLAN.md if missing"`. plan-gate 대상이면 ask 프롬프트가 뜨는데, 이게 떠야 tool dispatch 가 제대로 도는 것.

---

## 6. 검증

### 6.1. vLLM 만 따로 (Harness 빼고) curl 로 확인

```bash
curl -s http://localhost:8000/v1/chat/completions \
  -H "Authorization: Bearer sk-local-harness" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "Qwen/Qwen2.5-Coder-14B-Instruct",
    "messages": [{"role": "user", "content": "hi"}]
  }' | jq .
```

기대:

- HTTP 200.
- `choices[0].message.content` 에 자연스러운 응답.
- `model` 필드가 요청한 값과 일치.
- 실패 시:
  - `401 Unauthorized` → `--api-key` 불일치.
  - `404 Not Found` on `/v1/chat/completions` → 서버가 다른 포트거나 `--host` 가 외부 IP.
  - `model not found` → `vllm serve` 인자의 모델 ID 와 curl payload 의 `model` 이 다름.

### 6.2. 스트리밍 확인 (Harness 가 쓰는 모드)

```bash
curl -N http://localhost:8000/v1/chat/completions \
  -H "Authorization: Bearer sk-local-harness" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "Qwen/Qwen2.5-Coder-14B-Instruct",
    "stream": true,
    "messages": [{"role": "user", "content": "count 1 to 3"}]
  }'
```

`data: {...}` 프레임이 연속으로 나오고 `data: [DONE]` 으로 끝나면 OK.
Harness 는 `text/event-stream` 으로 이 스트림을 받아 `harness_core::
engine::consume_stream` 에서 파싱한다 (`openai.rs` 모듈 docstring 참조).

### 6.3. Harness 로 최종 확인

```bash
export OPENAI_BASE_URL=http://localhost:8000/v1
export OPENAI_API_KEY=sk-local-harness

# 툴 없는 왕복
harness ask --model openai/Qwen/Qwen2.5-Coder-14B-Instruct "say hello"

# 툴 있는 왕복
harness ask --model openai/Qwen/Qwen2.5-Coder-14B-Instruct \
  "glob all .rs files and tell me how many there are"
```

두 번째 명령에서 stderr 에 `⏺ Glob(**/*.rs)` 가 찍히면 tool calling 이
제대로 도는 것. 찍히지 않으면 §5.3 로 돌아가서 `--tool-call-parser`
를 점검.

---

## 7. 성능 튜닝

### 7.1. `--max-model-len` 낮추기

모델 기본 컨텍스트가 128K 여도 Harness 한 턴이 쓰는 건 보통 8K~32K. KV 캐시는
`max_model_len × num_seqs × hidden_dim × 2` 로 스케일하므로 `--max-model-len 16384`
로 낮추면 VRAM 이 크게 여유 생긴다. Harness 의 `harness-token` 크레이트는
`cl100k_base` 토크나이저 + 0.9 안전계수로 예산 계산하므로 16K 는 실무적으로 안전.

```bash
vllm serve Qwen/Qwen2.5-Coder-14B-Instruct --max-model-len 16384
```

### 7.2. `--gpu-memory-utilization` 조절

0.9 (기본) 는 공격적. 다른 CUDA 프로세스와 같이 뜨면 OOM. 증상은 startup 시 cublas
allocator fail. 0.85 → 0.80 순으로 내려가며 시도.

### 7.3. `--quantization awq` / `fp8`

- `awq`: 4-bit weight-only. 품질 드롭 작고 VRAM 2.5x 절감. AWQ 체크포인트 (`*-AWQ`) 를 HF 에서 먼저 받아야 함.
- `fp8`: H100/L40S 같은 FP8 하드웨어가 있을 때. 품질 유지, 속도 향상.

```bash
vllm serve Qwen/Qwen2.5-Coder-14B-Instruct-AWQ --quantization awq --max-model-len 16384
```

### 7.4. `--tensor-parallel-size N` (다중 GPU)

2×24GB → 70B AWQ 가 현실적. 주의: TP 는 NVLink / PCIe Gen4 이상에서만 실효.
느린 PCIe 에선 통신이 병목이라 단일 GPU + 작은 모델이 나을 수 있다.

```bash
vllm serve meta-llama/Llama-3.3-70B-Instruct-AWQ \
  --quantization awq --tensor-parallel-size 2 --max-model-len 16384
```

### 7.5. `--max-num-seqs` (단일 사용자엔 무관)

동시 요청 배치 상한. 1 사람 쓸 땐 기본값 그대로; 여러 에이전트를 병렬로 띄울 때만 손댈 가치.

---

## 8. 트러블슈팅

### 8.1. CUDA OOM on startup

증상: `vllm serve` 가 startup 중 `torch.cuda.OutOfMemoryError` / `cublasLt run failed` 로 죽는다.

조치 (순서대로):
1. `--gpu-memory-utilization 0.85` → 0.80 으로 내린다.
2. `--max-model-len` 을 절반으로 낮춘다.
3. 더 작은 quantization 체크포인트 (`*-AWQ`, `*-GPTQ`, `*-FP8`) 로 교체.
4. `--tensor-parallel-size` 로 다른 GPU 에 분산.
5. 그래도 안 되면 §8.4 참조.

### 8.2. `harness ask` 가 툴 호출 후 무한정 돎

증상: `⏺ Read(...)` 로그는 찍히는데 모델이 같은 툴을 반복 호출하거나 `max_turns` 까지 간다.
또는 tool 로그가 아예 없고 모델이 plain text 로 "파일 내용은..." 만 출력.

원인: `--tool-call-parser` 가 모델과 mismatch.

조치: §5.2 표로 파서 재확인 → vllm 프로세스 **완전 재시작** (reload 없음) →
필요시 `--chat-template` 명시 (모델 저장소 `tokenizer_config.json` 과 vllm 내장 템플릿 불일치 가능).

### 8.3. `401 Unauthorized`

원인: `OPENAI_API_KEY` ≠ `vllm serve --api-key`. 또는 vllm 을 `--api-key` 없이 띄웠는데
`OPENAI_API_KEY` 가 비어 있음. 클라이언트 env 와 서버 플래그를 동일값으로 맞춘다.
`OPENAI_API_KEY` 는 `settings.json` plaintext 금지 (README §8.2) — 반드시 env.

### 8.4. 모델이 Q4 로도 안 들어감

이 VRAM 에 이 모델은 안 된다. 대안:
- llama.cpp (GGUF) 로 CPU+GPU hybrid offload (예: 24GB + 70B Q4 GGUF 를 40/40 split).
- 더 작은 모델 (14B → 7B) 로 다운그레이드.
- 클라우드 GPU 빌려서 vLLM 을 거기 띄우고 `OPENAI_BASE_URL` 을 퍼블릭 주소로 (§9 필독).

### 8.5. Qwen 출력이 gibberish

원인 후보:
1. `--dtype bfloat16` 인데 Pascal/Volta (cc < 7.0) → bf16 미지원. `--dtype float16` 강제.
2. 모델 내장 템플릿 미적용. `--chat-template <path>` 명시.
3. AWQ 체크포인트 손상 — HF 에서 재다운로드.

### 8.6. Harness 쪽 `ProviderError::Parse`

원인: vLLM SSE 프레임이 OpenAI 스펙에서 벗어남 (파서 버그 또는 tool-call-parser mismatch).
§6.2 의 스트리밍 curl 로 실제 프레임을 확인하고, OpenAI Chat Completion chunk 스펙을
따르지 않으면 `uv pip install -U vllm` 으로 버전 업.

---

## 9. 보안 / 비용 / 운영 노트

### 9.1. "localhost 니까 안전" 이라는 착각

`OPENAI_BASE_URL=http://localhost:8000/v1` 로 걸면 요청은 로컬 루프백 밖으로 안 나간다.
다만 **Harness 가 여전히 OpenAI 코드패스를 탄다**는 사실은 남는다. 실무 권고:

1. **`ANTHROPIC_API_KEY` 를 unset** — 실수로 Anthropic 경로로 떨어져 유료 API 를
   때리는 걸 막는다. 같은 쉘에서 Claude API 를 쓸 일이 없다면 rcfile 에서 제거.
2. **`OPENAI_BASE_URL` 이 진짜 localhost 인지 assert** — 쉘 wrapper:
   ```bash
   # ~/.local/bin/harness-local
   #!/usr/bin/env bash
   set -euo pipefail
   case "${OPENAI_BASE_URL:-}" in
     http://localhost:*|http://127.0.0.1:*) ;;
     *) echo "OPENAI_BASE_URL must be localhost (got: ${OPENAI_BASE_URL:-unset})" >&2; exit 2;;
   esac
   unset ANTHROPIC_API_KEY
   exec harness "$@"
   ```
3. **vLLM `--host` 는 필요시 `127.0.0.1`** 로 좁힌다. `0.0.0.0` 은 같은 네트워크의
   다른 호스트가 붙을 수 있다 (사내망·공용 WiFi).

### 9.2. 킬스위치 관련

README §8.2 기준 Harness 자체 보안장치는 Bash env allowlist · 경로 canonicalize ·
cwd 트러스트 프롬프트 등이다. 특정 env 로 OpenAI 경로 전체를 일괄 차단하는
플래그는 현 코드 기준 **존재하지 않으므로**, `OPENAI_API_KEY` 자체를 set/unset
관리하는 게 1차 방어선. 확실히 막고 싶으면 wrapper 쉘로 env 를 고정.

### 9.3. 비용

- Anthropic 경로 대비 토큰 비용 0 (전기세·감가상각만).
- 24GB급 GPU 시간당 전력 ≈ 100~200 Wh. 장시간 켜놓으려면 `systemd` unit + `--gpu-memory-utilization` 로 타 워크로드와 공존.
- 로컬 모델은 **품질이 Claude/GPT-4o 보다 낮다.** 복잡한 리팩토링은 Anthropic 1 급으로 올리고
  grep/보일러플레이트/보조 질의만 로컬로 돌리는 하이브리드가 합리적.

---

## 10. 빠른 체크리스트

서버:
- [ ] `nvidia-smi` → CUDA 12.1+
- [ ] `vllm --help` OK
- [ ] `--enable-auto-tool-choice` + `--tool-call-parser <family>` 함께 설정
- [ ] `--api-key` 를 걸었다면 값 기록
- [ ] `/v1/chat/completions` curl → 200

클라이언트 (Harness):
- [ ] `OPENAI_BASE_URL=http://localhost:8000/v1` (끝 `/v1`)
- [ ] `OPENAI_API_KEY` 비어 있지 않고 서버 `--api-key` 와 일치
- [ ] `--model openai/<hf-repo-id>`
- [ ] `ANTHROPIC_API_KEY` unset
- [ ] `harness ask "hi"` OK → `harness ask "glob all .rs files"` OK

여기까지 통과하면 로컬 vLLM × Harness 는 production 으로 쓸 수 있다.

---

## 참고

- [vLLM Tool Calling (stable)](https://docs.vllm.ai/en/stable/features/tool_calling/) — 공식. 지원 파서 최신 목록.
- [vLLM Installation](https://docs.vllm.ai/en/stable/getting_started/installation/) — CUDA / ROCm / CPU / Apple Silicon.
- [vllm-metal](https://github.com/vllm-project/vllm-metal) — Apple Silicon 커뮤니티 플러그인.
- [vllm-mlx](https://github.com/waybarrios/vllm-mlx) — Apple Silicon MLX 백엔드 (Claude Code 호환 서버 포함).
- Harness 쪽 구현:
  - [`crates/harness-provider/src/openai.rs`](../../crates/harness-provider/src/openai.rs) — `/v1/chat/completions` 호출 경로.
  - [`crates/harness-cli/src/main.rs`](../../crates/harness-cli/src/main.rs) — `is_openai_model`, `openai/` prefix strip.
- 관련 문서: [`./mlx.md`](./mlx.md), [`./ollama.md`](./ollama.md).
