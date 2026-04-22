# MLX (Apple Silicon)

> Harness 를 Apple 의 **MLX / mlx-lm** 에 물려서, M 시리즈 맥 위에서 완전 로컬로
> 모델을 돌리는 법. 같은 디렉터리의 다른 런타임 문서
> ([ollama](ollama.md) · [vllm](vllm.md) · [lm-studio](lm-studio.md) ·
> [llama-cpp](llama-cpp.md)) 와 같은 구조를 따릅니다.

---

## 소개 / TL;DR

MLX 는 Apple 이 만든 Metal 가속 array 프레임워크입니다. 두 개의 관련 패키지:

- **`mlx`** — 핵심 수치 라이브러리 (NumPy 스타일 API, Metal 백엔드).
- **`mlx-lm`** — MLX 위에 얹은 LLM 추론 스택. `mlx_lm.generate` / `mlx_lm.server`
  CLI 와 Python API 를 제공.

harness 가 관심 있는 건 **`mlx_lm.server`** 입니다. 이건
**OpenAI 호환 HTTP 엔드포인트** 를 여는 로컬 서버라서, harness 의
OpenAI provider 경로 (`OPENAI_BASE_URL` + `--model openai/...`) 로 그대로 꽂힙니다.

### 왜 MLX 인가

Apple Silicon 에서 **가장 빠른 로컬 LLM 추론 경로** 입니다. 같은 모델 · 같은 양자화
기준으로 llama.cpp Metal 대비 토큰/초가 더 높게 나옵니다. 이유는 두 가지:

1. **Unified memory** — CPU/GPU 사이 복사가 없음. 70B 4bit (~40GB) 모델이
   64GB Mac Studio 한 대에서 돕니다. 같은 걸 NVIDIA 로 돌리려면
   24GB 카드 2장이 필요합니다.
2. **Metal-native 커널** — MLX 는 처음부터 Metal 전용으로 쓰였습니다.
   llama.cpp 는 CUDA/CPU 중심이라 Metal 경로는 이식 버전입니다.

### 언제 MLX 가 아닌가

- **Apple Silicon 이 아닌 머신** — MLX 자체가 못 돎. [llama.cpp](llama-cpp.md) / [Ollama](ollama.md) 로.
- **프로덕션 멀티유저** — 배치/텐서 병렬 없음. [vLLM](vllm.md) 이 압도적.
- **GGUF 를 그대로 쓰고 싶을 때** — [llama.cpp](llama-cpp.md) 쪽 모델 수가 훨씬 많음.

---

## 설치

Apple Silicon (M1/M2/M3/M4) + macOS 에서만 동작합니다. Intel Mac 은 불가.

### pip / uv / conda

```bash
# pip
python -m venv ~/.venvs/mlx && source ~/.venvs/mlx/bin/activate
pip install --upgrade pip && pip install mlx mlx-lm

# uv (권장)
uv venv ~/.venvs/mlx && source ~/.venvs/mlx/bin/activate
uv pip install mlx mlx-lm

# conda
conda install -c conda-forge mlx-lm
```

### 설치 확인

```bash
python -c "import mlx; print(mlx.__version__)"
python -c "import mlx_lm; print(mlx_lm.__version__)"
python -m mlx_lm.server --help | head -20
```

일부 고급 메모리 기능 (wire-memory 튜닝) 은 **macOS 15.0 이상** 을 요구합니다.
일반 추론만 할 거면 그 이하 버전도 문제없습니다.

---

## 모델 받기 & 서버 기동

### 모델 고르기

HuggingFace 의 [`mlx-community`](https://huggingface.co/mlx-community) org 가
미리 양자화된 MLX 모델을 호스팅합니다. 네이밍 컨벤션:

```
mlx-community/<BaseName>-<Size>-<Variant>-<Bits>bit
```

실사용 예:

- `mlx-community/Qwen2.5-Coder-14B-Instruct-4bit` — 코딩 태스크 기준 추천 기본값.
- `mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit` — 더 큰 맥 (48GB+) 용.
- `mlx-community/Meta-Llama-3.1-8B-Instruct-4bit` — 일반 대화.

bit 수는 메모리-품질 트레이드오프:

| 양자화 | 14B 메모리 (대략) | 품질 |
|---|---|---|
| `-8bit` | ~14 GB | 원본에 근접 |
| `-4bit` | ~8 GB | 실사용 스윗스팟 |
| `-3bit` | ~6 GB | 품질 저하 눈에 띔 |
| `-2bit` | ~4 GB | 명백히 깨짐, 실험용 |

### 서버 기동

```bash
python -m mlx_lm.server \
  --model mlx-community/Qwen2.5-Coder-14B-Instruct-4bit \
  --host 127.0.0.1 \
  --port 8080
```

첫 실행 시 weights 가 `~/.cache/huggingface/hub/` 아래로 다운로드됩니다
(14B-4bit 기준 ~8 GB). 다음 실행부턴 로컬 캐시에서 바로 로드.

기본값: `--host 127.0.0.1`, `--port 8080`. 노출 엔드포인트는
`/v1/chat/completions` 와 `/v1/models`. 즉 harness 가 볼 base URL 은
`http://127.0.0.1:8080/v1`.

공식 README 자체가 "basic security checks only" 라고 명시합니다.
`127.0.0.1` 바인딩 유지 전제로 쓰세요.

---

## harness 설정

harness 는 `--model` 값이 다음 prefix 중 하나로 시작하면 OpenAI provider 로
라우팅합니다: `openai/`, `gpt-`, `o1`, `o3`, `o4` (`crates/harness-cli/src/main.rs:899`
의 `is_openai_model`). MLX 모델명은 자연스럽게 이 조건에 안 맞으니,
**`openai/` prefix 를 붙여서 강제 라우팅** 합니다. harness 는 서버로 보낼 때
`openai/` 를 떼고 나머지를 그대로 모델명으로 실어 보냅니다.

### env

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
export OPENAI_API_KEY=mlx     # 아무 non-empty string. 빈 값은 harness 가 거부.
```

### 실행

```bash
harness ask \
  --model openai/mlx-community/Qwen2.5-Coder-14B-Instruct-4bit \
  "이 디렉터리의 파일 목록을 보여줘"
```

서버 쪽에서 요청을 받아 `mlx_lm` 이 토큰을 생성하기 시작하면 harness
stderr 에 평소처럼 `⏺ Tool(args)` 로그가 찍힙니다.

### settings.json 으로 고정

매번 긴 모델명을 치기 싫다면 `~/.config/harness/settings.json` 에:

```jsonc
{
  "version": 1,
  "model": "openai/mlx-community/Qwen2.5-Coder-14B-Instruct-4bit"
}
```

그 뒤엔 `harness ask "..."` 만으로 MLX 로 라우팅됩니다.

---

## 툴 콜링 (주의 · 반드시 읽기)

**여기가 MLX 특유의 함정 구간입니다.** harness 의 agent loop 는 모델이
`tool_calls` JSON 을 제대로 뱉어주는 것에 전적으로 의존합니다. 그런데
`mlx_lm.server` 의 툴콜 지원은 2025 년 현재 **모델 의존적이고 부분적** 입니다.

### 메커니즘

`mlx_lm.server` 는 모델의 tokenizer chat template 을 그대로 쓰고, 응답에서
툴콜 블록을 파싱할 때는 내부 `_infer_tool_parser()` 가 모델 패밀리를 보고
알맞은 파서를 고릅니다. 이게 엇나가는 두 경로:

1. **모델의 chat template 에 tool 섹션 자체가 없거나 빈약할 때**
   — 서버가 `tools=[...]` 파라미터를 렌더링하지 못 하고, 모델은 툴의
   존재를 모름. harness 가 툴을 써야 할 질문에 산문으로만 대답함.

2. **템플릿은 있지만 파서 branch 가 없는 포맷일 때**
   — 대표적으로 Gemma 4 의 `<|tool_call>...<tool_call|>` 포맷이 현재 (2025)
   `_infer_tool_parser()` 에 매칭되는 branch 가 없습니다
   ([ml-explore/mlx-lm#1096](https://github.com/ml-explore/mlx-lm/issues/1096)).
   이 경우 모델은 툴콜을 뱉는데 서버가 파싱 실패, OpenAI-compat 응답의
   `tool_calls` 필드가 비고 원문만 `content` 에 들어옴 → harness 는 그걸
   그냥 텍스트로 받습니다.

### 실무 권장 순서

1. **스모크 테스트 먼저** — 툴이 필요 없는 한 줄로 서버/모델/라우팅이
   일단 살아있는지 확인:
   ```bash
   harness ask --model openai/mlx-community/Qwen2.5-Coder-14B-Instruct-4bit "hi"
   ```
   응답이 오면 기본 경로는 OK.

2. **툴 스모크 테스트** — `harness ask "list files in ."` 류. 모델이 Glob/Bash
   를 실제로 **호출** 해야 통과. 산문으로 `"You can run ls ..."` 로 답하면
   템플릿 gap.

3. **실패 시 분기**:
   - 같은 패밀리의 다른 quant 로 교체 (`-4bit` 대신 원본 HF 레포의 비-MLX
     버전이 더 나은 template 를 갖는 경우가 있음 → llama.cpp 로 가는 게
     빠름).
   - **llama.cpp Metal** ([llama-cpp.md](llama-cpp.md)) 로 전환 — 약간
     느리지만 GGUF 의 tool template 커버리지가 훨씬 넓음.
   - **LM Studio** ([lm-studio.md](lm-studio.md)) 의 MLX 백엔드 — LM Studio
     가 툴 파싱 레이어를 자체적으로 얹어 줌. GUI 가 맞으면 가장 편한 길.

### 어떤 모델이 잘 되나 (2026-04 시점 경험칙)

- Qwen 2.5 / Qwen 3 / Qwen3-Coder 계열 `mlx-community` 퀀트 — 대체로 OK.
- Llama 3.1 / 3.3 instruct — 대체로 OK.
- Gemma 4 — 위 이슈 미해결 시 **안 됨**.
- 소형 (<4B) 범용 모델 — 템플릿은 있어도 툴 사용 추론 자체가 약해서
  harness 의 agent loop 가 공회전.

이건 빠르게 바뀌는 영역이라, harness PR 단계에서 검증한 조합을 그대로 믿지 말고
**항상 자기 모델로 스모크 테스트** 하는 게 맞습니다.

---

## 검증

### 1) 서버 직접 (harness 를 끼지 않고)

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "mlx-community/Qwen2.5-Coder-14B-Instruct-4bit",
    "messages": [{"role":"user","content":"say hi"}]
  }' | python -m json.tool
```

- HTTP 200 + `choices[0].message.content` 가 텍스트면 OK.
- 404 → 서버 안 떠 있거나 포트 다름.
- Connection refused → `--host` 가 `127.0.0.1` 이 맞는지 확인.

### 2) 모델 목록

```bash
curl -s http://127.0.0.1:8080/v1/models | python -m json.tool
```

### 3) harness 최소 루프

```bash
OPENAI_BASE_URL=http://127.0.0.1:8080/v1 \
OPENAI_API_KEY=mlx \
harness ask --model openai/mlx-community/Qwen2.5-Coder-14B-Instruct-4bit "hi"
```

### 4) 툴 루프

```bash
OPENAI_BASE_URL=http://127.0.0.1:8080/v1 \
OPENAI_API_KEY=mlx \
harness ask --model openai/mlx-community/Qwen2.5-Coder-14B-Instruct-4bit \
  "현재 디렉터리의 .rs 파일 수를 Glob 으로 세서 알려줘"
```

stderr 에 `⏺ Glob(**/*.rs)` 같은 줄이 찍히면 툴콜이 살아있는 것.
안 찍히고 모델이 텍스트로 답만 하면 §5 의 템플릿 문제.

---

## 성능 메모

### Apple Silicon 에서의 이점

Unified memory 덕에 큰 모델이 상대적으로 낮은 사양에서 돕니다.
경험칙 (4-bit 기준):

| 모델 | 메모리 | 추천 머신 |
|---|---|---|
| 7B | ~5 GB | M1/M2 16GB 이상 |
| 14B | ~8 GB | M 시리즈 Pro 16GB 이상 |
| 30B | ~18 GB | M 시리즈 Pro/Max 32GB 이상 |
| 70B | ~40 GB | Mac Studio 64GB 이상 |

### 알아둬야 할 약점

- **Prompt processing (prefill)** 은 CUDA 대비 느립니다. 긴 프롬프트 (20k
  토큰 같은 코드 컨텍스트) 를 많이 꽂으면 첫 토큰까지의 대기시간이 눈에
  띄게 깁니다. Decode 속도 자체는 괜찮음.
- **배치/텐서 병렬 없음.** 동시 요청 2개 이상은 순차 처리처럼 동작. 1인
  사용엔 상관없음, 여러 사용자에게 서비스하려면 vLLM 쪽으로 가야 합니다.
- **동적 토큰 상한** — `--max-tokens` 같은 플래그로 상한을 올릴 수 있지만,
  메모리 기준 KV cache 가 빠르게 커집니다. 긴 세션은 도중에 OOM 낼 수 있음.

### 튜닝 한 가지

macOS 15+ 에서 큰 모델을 돌릴 때 MLX 의 wire-memory 기능을 쓰면 swap
대신 통합 메모리에 고정해서 속도 안정성이 좋아집니다. 공식 README 의
해당 섹션 참고 (관리자 권한 필요).

---

## 트러블슈팅

### `ModuleNotFoundError: No module named 'mlx'`

- venv 활성화 안 함. `source ~/.venvs/mlx/bin/activate`.
- 설치 Python 과 실행 Python 이 다름. `which python && python -c "import sys; print(sys.executable)"`.
- conda + pip 혼용 이슈. 한쪽으로 통일.

### 툴콜이 JSON 문자열로 `content` 에 들어가고 `tool_calls` 는 빔

§5 의 chat template gap. 3가지 선택지:
1. 다른 모델로 교체 (Qwen/Llama 계열 instruct).
2. llama.cpp Metal 로 전환.
3. LM Studio 로 전환 (MLX 백엔드 + LM Studio 파서).

### 서버가 첫 요청에서 영원히 멈춤

`mlx_lm.server` 는 요청 들어온 시점에 모델 로드를 시작하기도 합니다
(fork/version 에 따라 lazy 하게 움직임). 실제론 weights 다운로드 중인데
progress bar 가 HTTP 응답에 섞이지 않는 것처럼 보일 뿐. 확인:

```bash
du -sh ~/.cache/huggingface/hub/models--mlx-community--*
# 크기가 계속 커지는지
```

14B-4bit 기준 첫 다운로드 ~8 GB, 네트워크 속도에 따라 수 분.

### Launch 시 `MLX malloc failed` / `out of memory`

KV cache + weights 가 통합 메모리 한계를 넘음. 해법:
- 낮은 bit quant (`-3bit`, 드물게 `-2bit`).
- 더 작은 모델 (14B → 7B).
- 다른 heavy 앱 종료 (브라우저 탭, Xcode 등).
- `--max-tokens` 를 의도적으로 작게 (e.g. 4096) 해서 KV cache 상한을 줄임.

### 응답이 오긴 오는데 토큰/초가 바닥

- 다른 GPU-heavy 앱이 동시에 도는 중 (Xcode 빌드, 브라우저 WebGL 탭).
- `--temp 0` 같은 결정적 세팅은 영향 없음; 품질 체감만 차이.
- M1 8GB 같은 RAM-tight 머신에서 14B 를 돌리면 swap thrash — 모델 사이즈를
  줄이는 게 유일한 답.

### `Address already in use`

이전 서버 프로세스가 살아있음:

```bash
lsof -i :8080
kill <pid>
```

---

## 보안 / 비용

### Localhost 바인딩 유지

`--host 127.0.0.1` 기본값을 **그대로** 두세요. `0.0.0.0` 으로 열면 같은
Wi-Fi 의 누구든 당신 LLM 을 쓸 수 있습니다 (공식 README 도 "basic security
checks only" 라고 경고).

### 어카운트 과금 사고 방지

harness 는 env 가 섞이면 의도와 다르게 유료 API 를 호출할 수 있습니다.
로컬 전용으로 쓸 땐 API 키를 명시적으로 비우고, base URL 이 localhost 인지
확인하는 래퍼를 쓰는 걸 권장:

```bash
# ~/.zshrc
harness-mlx() {
  if [[ "$OPENAI_BASE_URL" != http://localhost:* \
     && "$OPENAI_BASE_URL" != http://127.0.0.1:* ]]; then
    echo "refusing: OPENAI_BASE_URL must point at localhost" >&2
    return 1
  fi
  unset ANTHROPIC_API_KEY
  HARNESS_REFUSE_API_KEY= harness "$@"
}
```

그리고 `HARNESS_REFUSE_API_KEY=1` 을 전역으로 켜 놓은 뒤 로컬 쓸 때만
위 래퍼로 해제하는 습관이 안전합니다. 자세한 건 [`README.md`](README.md)
§Billing-safety 참고.

### 모델 가중치의 출처

`mlx-community` 는 커뮤니티 org 입니다. 중요한 환경에선 원본 모델의
라이선스와 `mlx-community` 의 변환자 신원을 확인하고 쓰세요. 대부분은
원본 모델의 라이선스 (Apache-2.0, Llama Community, Qwen 라이선스 등) 가
그대로 승계됩니다.

---

## 참고

- 공식 리포: [ml-explore/mlx-lm](https://github.com/ml-explore/mlx-lm)
- 서버 문서: [mlx_lm/SERVER.md](https://github.com/ml-explore/mlx-lm/blob/main/mlx_lm/SERVER.md)
- 퀀트 허브: [huggingface.co/mlx-community](https://huggingface.co/mlx-community)
- 툴콜 gap 이슈 (2025): [ml-explore/mlx-lm#1096](https://github.com/ml-explore/mlx-lm/issues/1096)
- 인덱스: [README.md](README.md)
- 형제 문서: [ollama.md](ollama.md) · [vllm.md](vllm.md) ·
  [lm-studio.md](lm-studio.md) · [llama-cpp.md](llama-cpp.md)
