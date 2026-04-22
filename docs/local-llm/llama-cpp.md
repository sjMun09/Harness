# llama.cpp 로 Harness 돌리기

> **대상 독자.** Harness 를 **로컬 llama.cpp 서버** 에 붙여서 쓰고 싶은 사람.
> GGUF 파일을 직접 고르고, 양자화 티어를 직접 고르고, Metal / CUDA / Vulkan
> 백엔드 중 뭘 쓸지 직접 정하고 싶은 사람. Ollama 처럼 모델 레지스트리에
> 끌려다니는 게 싫은 사람.

Harness 는 **OpenAI 호환 API** 를 말할 줄 아는 백엔드면 받는다.
`crates/harness-cli/src/main.rs:899` 의 `is_openai_model` 이
`openai/` · `gpt-` · `o1` · `o3` · `o4` 로 시작하는 모델명을 OpenAI 라우트로
보내고, `OPENAI_BASE_URL` 에 `/v1/chat/completions` 를 붙여 호출한다.
llama.cpp 는 그 계약을 만족하는 가장 bare-metal 한 서버다.

---

## 0. TL;DR — 한 화면 요약

```bash
# 1) 설치 (macOS)
brew install llama.cpp

# 2) 모델 (GGUF) 받기
pip install -U "huggingface_hub[cli]"
huggingface-cli download bartowski/Qwen2.5-Coder-14B-Instruct-GGUF \
  Qwen2.5-Coder-14B-Instruct-Q4_K_M.gguf --local-dir ~/models

# 3) 서버 기동 (Metal, 127.0.0.1:8080, tool-calling 정상화)
llama-server \
  -m ~/models/Qwen2.5-Coder-14B-Instruct-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -ngl 999 -c 16384 \
  --jinja \
  --api-key sk-local-harness

# 4) Harness 연결
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
export OPENAI_API_KEY=sk-local-harness
harness ask --model openai/qwen2.5-coder-14b "이 레포 구조 설명해줘"
```

이 네 단계로 Harness 의 tool-calling (`Read`, `Glob`, `Grep`, `Edit`, `Bash`)
까지 동작한다. 이하 섹션은 이유 · 튜닝 · 실패 시 대처.

---

## 1. 왜 llama.cpp 인가

| 기준 | Ollama | LM Studio | vLLM | **llama.cpp** |
|---|---|---|---|---|
| 배포 | 데몬 + 레지스트리 | GUI + 내장 서버 | Python 서버 | 단일 C++ 바이너리 |
| 하드웨어 | CPU · GPU | CPU · GPU | GPU 전제 | CPU · Metal · CUDA · ROCm · Vulkan |
| 모델 지정 | 레지스트리 태그 | GUI 선택 | HF repo 이름 | **GGUF 파일 경로 직접** |
| 양자화 선택 | 태그 (`:q4_K_M`) | drop-down | FP16/BF16 | **GGUF 파일명 명시** (Q4_K_M, Q5_K_M, IQ2_XXS, ...) |
| tool-calling | 내장 템플릿 | 지원 | 지원 | `--jinja` 로 GGUF 임베드 템플릿 |

**llama.cpp 를 고르는 이유:**

1. **양자화 티어를 직접 고른다.** Ollama 레지스트리는 인기 티어만 푸쉬하는데,
   IQ2_XXS / IQ3_XS 같은 low-bit 양자화는 직접 GGUF 받는 게 확실하다.
2. **하드웨어가 평범할 때.** Metal · CPU · Vulkan 이 1 급.
3. **의존성 최소화.** 단일 바이너리 + GGUF 파일.

이 문서는 Harness ↔ llama.cpp 붙이기에 집중한다. llama.cpp 자체의 깊은 튜닝은
공식 README 참조.

---

## 2. 설치

### 2.1 macOS (Apple Silicon · Metal)

```bash
brew install llama.cpp
llama-server --version
```

Homebrew 빌드는 Metal 이 기본 ON — `-ngl 999` 로 GPU offload 바로 된다.

### 2.2 Linux · NVIDIA (CUDA)

소스 빌드 권장 (바이너리 배포판의 CUDA 아키텍처 미스매치가 잦음):

```bash
git clone https://github.com/ggml-org/llama.cpp
cd llama.cpp
cmake -B build -DGGML_CUDA=ON -DCMAKE_BUILD_TYPE=Release
cmake --build build --config Release -j
export PATH="$PWD/build/bin:$PATH"
```

### 2.3 Linux · AMD / Intel / CPU

```bash
cmake -B build -DGGML_HIP=ON -DAMDGPU_TARGETS=gfx1100    # ROCm
cmake -B build -DGGML_VULKAN=ON                         # Vulkan
cmake -B build -DCMAKE_BUILD_TYPE=Release               # CPU only
```

이 문서의 플래그 (`--jinja`, `--flash-attn`, `--cache-type-k`) 는 2024 후반
이후 버전에서 안정화됐다. 오래된 Homebrew 캐시가 있으면 `brew upgrade
llama.cpp` 로 올려두자.

---

## 3. 모델 받기 (GGUF)

llama.cpp 는 **GGUF** 포맷만 먹는다. HuggingFace 에서 `*-GGUF` 레포 찾아서
원하는 양자화 파일 하나만 받는 게 표준.

### 3.1 huggingface-cli

```bash
pip install -U "huggingface_hub[cli]"
huggingface-cli download bartowski/Qwen2.5-Coder-14B-Instruct-GGUF \
  Qwen2.5-Coder-14B-Instruct-Q4_K_M.gguf \
  --local-dir ~/models \
  --local-dir-use-symlinks False
```

- 첫 인자는 **repo**, 두 번째는 **그 repo 안의 파일명**. 파일명 생략하면
  repo 전체(수십 GB) 를 받으니 반드시 지정.
- `bartowski` · `TheBloke` · `lmstudio-community` 가 대표 배포자.
- gated repo 는 먼저 `huggingface-cli login` + HF 웹에서 라이선스 수락.

### 3.2 양자화 티어 가이드

| 접미사 | 크기 (14B 기준) | 언제 쓰나 |
|---|---|---|
| `Q8_0` | ~15 GB | FP16 근접. RAM 넉넉하고 품질 최대 원할 때 |
| `Q6_K` | ~12 GB | 거의 FP16. Q8 안 들어갈 때 |
| `Q5_K_M` | ~10 GB | Q4 대비 +10% 품질 / +20% 크기 |
| **`Q4_K_M`** | **~8.4 GB** | **대부분 여기서 시작** |
| `Q4_K_S` | ~8.0 GB | Q4_K_M 이 간당간당할 때 |
| `Q3_K_M` | ~6.8 GB | 품질 눈에 띄게 하락 |
| `IQ3_XS` / `IQ2_XXS` | 4–6 GB | "어떻게든 넣고 싶다" 절박할 때만 |

**권장 경로**: `Q4_K_M` 부터 시작. 툴 콜 자주 놓치거나 instruction following
이 흔들리면 `Q5_K_M`. VRAM 모자라면 `Q4_K_S` → `Q3_K_M`.

### 3.3 에이전트 용도 추천 모델

- **Qwen2.5-Coder-14B-Instruct** — 코드 툴 잘 따름. Q4_K_M 이 ~8.5 GB.
- **Qwen2.5-Coder-32B-Instruct** — 품질↑. Q4_K_M 이 ~20 GB (24 GB 카드 대응).
- **Llama-3.1-8B-Instruct** — 더 작고, tool-calling 템플릿 안정.
- **Mistral-Nemo-Instruct-12B** — 128k context.

llama.cpp `--jinja` 가 공식 지원하는 템플릿: **Llama 3.1/3.2/3.3,
Functionary v3.1/v3.2, Qwen 2.5 계열, Mistral Nemo, Firefunction v2,
Hermes 2/3, Command R7B, DeepSeek R1**. 이 목록 밖도 generic fallback 으로는
돌지만 tool-calling 정확도가 떨어질 수 있다.

---

## 4. 서버 기동 (`llama-server`)

### 4.1 권장 시작점

```bash
llama-server \
  -m ~/models/Qwen2.5-Coder-14B-Instruct-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -ngl 999 \
  -c 16384 \
  --jinja \
  --api-key sk-local-harness
```

### 4.2 각 플래그 의미

| 플래그 | 의미 |
|---|---|
| `-m <path>` | GGUF 파일. 하나만 로드. |
| `--host 127.0.0.1` | 바인딩 주소. llama-server 기본값 = `127.0.0.1`. `0.0.0.0` 은 LAN 노출 — §10. |
| `--port 8080` | 리슨 포트. 기본 8080. |
| `-ngl 999` | GPU 오프로드 레이어 수. `999` = "fit 되는 만큼 전부". 남으면 CPU 로 흘림. |
| `-c 16384` | context window 토큰 수. 크면 KV cache 가 RAM 을 먹음. |
| `--jinja` | **GGUF 임베드 Jinja 챗 템플릿 사용.** tool-calling 쓰려면 사실상 필수. 안 걸면 generic 템플릿으로 포맷해서 `<tool_call>` 마커가 round-trip 안 됨. |
| `--api-key <token>` | Bearer 인증. 미지정이면 아무 토큰이나 통과. Harness 는 빈 키 거부. |
| `--alias <name>` | `/v1/models` 응답의 id. 생략 시 GGUF 경로가 id. |

### 4.3 기동 확인

```bash
curl -s http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer sk-local-harness" | jq .
```

llama.cpp 는 모델을 **한 개만** 로드하므로 `data` 배열은 항상 길이 1.

---

## 5. Harness 에 연결

Harness 의 OpenAI 라우팅 규칙 (`crates/harness-cli/src/main.rs:899`):

- `openai/`, `gpt-`, `o1`, `o3`, `o4` 로 시작하면 OpenAI 경로.
- `OPENAI_BASE_URL` + `/v1/chat/completions` 로 호출.
- 요청 직전 모델명의 선행 `openai/` 는 제거.
- `OPENAI_API_KEY` 가 빈 문자열이면 거부.

### 5.1 env 설정

```bash
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
export OPENAI_API_KEY=sk-local-harness   # llama-server --api-key 와 동일
```

`--api-key` 안 걸었으면 아무 non-empty 문자열 (`sk-dummy` 등) 이면 된다.

### 5.2 호출

```bash
harness ask --model openai/qwen2.5-coder-14b "이 레포 구조 설명해줘"
```

`qwen2.5-coder-14b` 부분은 **라벨일 뿐**. llama.cpp 는 요청의 `model` 필드를
무시하고 로드된 유일한 GGUF 를 쓴다. 로그에서 알아볼 수 있는 이름만 넣자.

### 5.3 기본 모델 고정

매번 `--model` 치기 귀찮으면:

```bash
export HARNESS_MODEL=openai/qwen2.5-coder-14b
# 또는 ~/.config/harness/settings.json 의 "model": "openai/qwen2.5-coder-14b"
```

---

## 6. 툴 콜링

Harness 의 14 개 내장 툴이 돌려면 서버가 OpenAI `tools` / `tool_calls` 필드를
올바르게 round-trip 해야 한다.

### 6.1 `--jinja` 가 핵심

llama.cpp 의 챗 템플릿 경로는 둘:

1. **(기본)** generic OpenAI-compat 템플릿 — 단순 대화는 돼도, Qwen 의
   `<tool_call>` 이나 Llama-3 의 `<|python_tag|>` 같은 모델 고유 tool 마커를
   제대로 만들지 못함.
2. **`--jinja`** — GGUF 메타데이터에 임베드된 Jinja 템플릿 사용. Qwen2.5 /
   Llama-3.x / Mistral Nemo / Hermes 2–3 / Functionary / Firefunction /
   Command R7B / DeepSeek R1 공식 지원.

**즉, tool-calling 을 쓰려면 `--jinja` 로 띄워야 한다.** 없으면 모델은 툴을
호출했다고 생각하는데 클라이언트는 텍스트로 받는다.

### 6.2 smoke test

```bash
harness ask --model openai/qwen2.5-coder-14b \
  "read the first 10 lines of README.md and summarize them"
```

stderr 에 이런 라인이 떠야 정상:

```
⏺ Read(README.md)
  ↳ ok: 10 lines
```

안 뜨고 맨텍스트만 나오면 tool-calling 실패. 체크 순서:

1. `--jinja` 걸고 띄웠나? → 서버 기동 로그의 `chat template` 확인.
2. curl 로 `tools` 필드 round-trip 되는지 직접 확인 (§7.3).
3. GGUF 가 tool-capable instruct 변형인가? (base 모델은 `--chat-template-file`
   로 템플릿을 따로 공급해야 함.)

---

## 7. 검증 루틴

Harness 붙이기 전에 **llama-server 단독으로** OpenAI 계약 검증.

### 7.1 `/v1/models`

```bash
curl -s http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer sk-local-harness" | jq .
```

`data` 배열 길이 항상 1. id 는 GGUF 경로 또는 `--alias`.

### 7.2 `/v1/chat/completions` 단발

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-local-harness" \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen","messages":[{"role":"user","content":"say hi"}]}' | jq .
```

`choices[0].message.content` 에 문장이 와야 한다.

### 7.3 `/v1/chat/completions` + `tools`

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer sk-local-harness" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"qwen",
    "messages":[{"role":"user","content":"weather in Seoul?"}],
    "tools":[{"type":"function","function":{
      "name":"get_weather",
      "description":"Get current weather for a city",
      "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}],
    "tool_choice":"auto"
  }' | jq '.choices[0].message'
```

결과 `message` 에 `tool_calls` 배열이 있어야 한다. `content` 만 있고
`tool_calls` 없으면 `--jinja` 누락 또는 tool-capable 모델이 아님.

---

## 8. 성능 튜닝

| 플래그 | 효과 | 메모 |
|---|---|---|
| `-ngl <N>` | GPU 오프로드 레이어 | `999` = fit 되는 만큼. VRAM 부족 시 낮춰라 (`-ngl 32` 등) |
| `-c <N>` | context window | 8k / 16k / 32k+. 모델 trained context 초과 금지 |
| `-t <N>` | CPU 쓰레드 (생성) | 일부 레이어가 CPU 에 있을 때만 의미. 실물 코어 수 |
| `--threads-batch <N>` | CPU 쓰레드 (프롬프트) | 프롬프트 길 때 중요 |
| `-b <N>` / `-ub <N>` | 배치 / 마이크로배치 | 긴 프롬프트일 때 `-b 4096 -ub 512` 근처 |
| `--flash-attn` | Flash Attention | Metal / Ampere+ 에서 큰 속도 향상. 기본 OFF |
| `--cache-type-k q8_0 --cache-type-v q8_0` | KV cache 양자화 | 긴 컨텍스트 메모리 ~50% 절약. 허용값: `f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1` |

### 현실적 조합 예시

**MacBook Pro M3 Max 36 GB · Qwen2.5-Coder-14B Q4_K_M**

```bash
llama-server -m ~/models/Qwen2.5-Coder-14B-Instruct-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -ngl 999 -c 16384 \
  --flash-attn \
  --jinja --api-key sk-local-harness
```

**RTX 3090 24 GB · Qwen2.5-Coder-32B Q4_K_M**

```bash
llama-server -m ~/models/Qwen2.5-Coder-32B-Instruct-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -ngl 999 -c 8192 \
  --flash-attn \
  --cache-type-k q8_0 --cache-type-v q8_0 \
  --jinja --api-key sk-local-harness
```

**CPU only · Llama-3.1-8B Q4_K_M**

```bash
llama-server -m ~/models/Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -ngl 0 -c 4096 -t 8 \
  --jinja --api-key sk-local-harness
```

---

## 9. 트러블슈팅

### 9.1 `failed to load model` / `gguf_init_from_file failed`

경로 오타 (절대경로 써라), 또는 다운로드 중단된 파일 (`.incomplete` 확인, sha
비교). llama.cpp 버전이 오래돼 최신 GGUF 포맷을 모르면 `brew upgrade
llama.cpp` / 소스 재빌드.

### 9.2 툴 콜 응답이 맨텍스트로 나온다

`--jinja` 누락 또는 GGUF 임베드 템플릿이 tool-capable 이 아님. 서버 기동
로그에서 `chat template` 라인 확인. "generic" / "chatml" 로 fallback 되어
있으면 네이티브 템플릿 미인식. 해결: `--chat-template-file
/path/to/custom.jinja` 로 템플릿 공급, 또는 §3.3 / §6.1 지원 목록의 instruct
변형 GGUF 로 교체.

### 9.3 기동 시 CUDA / Metal OOM

- `-ngl` 낮추기 (예: `-ngl 32`) → 안 들어가는 레이어는 CPU 로.
- `-c` 줄이기 (예: `-c 4096`) → KV cache 감소.
- `--cache-type-k q8_0 --cache-type-v q8_0` → KV cache 양자화.
- 그래도 안 되면 한 양자화 티어 아래로 (Q4_K_M → Q4_K_S → Q3_K_M).
- 다른 GPU 프로세스가 VRAM 잡고 있는지 `nvidia-smi` / `nvtop` 로 확인.

### 9.4 첫 토큰까지 30–60s — hang 처럼 보임

거의 항상 **prompt eval** 이 CPU 에 갇힌 증상. `-ngl` 이 너무 작아서 프롬프트
처리 레이어가 CPU 에 있음. 해결: `-ngl` 올리기, `--flash-attn`, 프롬프트 자체
줄이기. 단발 `harness ask` 는 매번 full prompt eval 을 탄다 — 긴 시스템
프롬프트가 있으면 `harness session resume` 으로 이어가면 KV cache 재사용.

### 9.5 긴 컨텍스트에서 출력이 쓰레기로 변함

모델의 trained context 한계 초과. `-c` 를 공식값 이하로 (Llama-3.1 = 128k,
Qwen2.5 = 128k, Mistral Nemo = 128k, Llama-3.0 = 8k). RoPE scaling 이 적용된
GGUF 는 repo 설명에 명시.

### 9.6 Harness `OPENAI_API_KEY is empty`

env 에 빈 문자열 / export 안 됨. `echo "$OPENAI_API_KEY"` 확인. 또한
`OPENAI_BASE_URL` 끝에 `/v1` 까지만 있는지 — Harness 가 뒤에
`/chat/completions` 를 이어 붙인다.

### 9.7 `401 Unauthorized`

llama-server `--api-key` 값과 클라이언트 키 불일치. 로그에 `Invalid API Key`
찍히면 확정.

### 9.8 포트 충돌

`lsof -i :8080`. `--port 8081` 로 바꾸고 `OPENAI_BASE_URL` 도 동일하게 수정.

---

## 10. 보안 / 비용

- **로컬호스트 바인딩 (`127.0.0.1`) 은 같은 박스 안에서만 도달 가능.** 기본
  바인딩이며 그대로 두자. 네트워크 노출이 필요하면 최소한 `--api-key` 걸고
  앞단에 TLS 리버스 프록시.
- **토큰 비용 0, 전기세만.** 하지만 품질 / 지연 비용은 있다 — Opus 급이 30초
  에 끝낼 리팩토링을 로컬 14B 가 5분 걸리거나 실패할 수 있음.
- Harness 의 `HARNESS_REFUSE_API_KEY=1` 을 켜면 OpenAI 경로 전체 차단.
  반대로 여기서는 OpenAI 경로를 **쓰고 싶으니**, "모든 OpenAI 요청은
  localhost 로만" 을 보장하려면 셸 wrapper 로 가드:

```bash
# ~/.zshrc
harness-local() {
  case "$OPENAI_BASE_URL" in
    http://127.0.0.1:*|http://localhost:*) harness "$@" ;;
    *) echo "refusing: OPENAI_BASE_URL is not local: $OPENAI_BASE_URL" >&2; return 1 ;;
  esac
}
```

- `settings.json` 권한 규칙은 로컬 모델이라고 느슨하게 가지 말 것. 로컬
  모델이 실수로 `rm -rf` 를 부르는 건 Opus 가 부르는 것과 동일한 피해다.
  `Bash(rm **)` deny 는 그대로 유지.

---

## 부록 A. 상주 서비스

매번 기동 귀찮으면 launchd (macOS) / systemd (Linux) 로 상주시킨다. `§4.1` 의
`llama-server ...` 커맨드를 그대로 `ExecStart` / `ProgramArguments` 에 넣으면
된다.

- **macOS**: `~/Library/LaunchAgents/com.user.llama-server.plist` 에
  `ProgramArguments` 배열 + `RunAtLoad`/`KeepAlive` true → `launchctl load`.
- **Linux**: `/etc/systemd/system/llama-server.service` 에 `[Service]
  ExecStart=... Restart=on-failure` → `systemctl enable --now llama-server`.

로그는 macOS 는 `StandardOutPath` / `StandardErrorPath`, Linux 는
`journalctl -u llama-server -f`.

## 부록 B. 참고 링크

- llama.cpp: https://github.com/ggml-org/llama.cpp
- 서버 README: https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md
- function calling: https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md
- GGUF 배포자: https://huggingface.co/bartowski
- Harness README: [../../README.md](../../README.md)
- Harness OpenAI 라우팅: `crates/harness-cli/src/main.rs:899` (`is_openai_model`)
