# Harness × LM Studio

> **LM Studio 를 백엔드로 써서 `harness ask` / `harness session` 을 로컬 모델로 돌리는 런북.**
> 터미널에 익숙하지 않아도 되는, **GUI 우선** 로컬 LLM 런타임입니다.
> Ollama 가 "`ollama pull` 로 돌리는 CLI 우선" 도구라면, LM Studio 는
> "Discover 탭에서 클릭으로 받는 데스크톱 앱" 쪽입니다.

Harness 의 OpenAI 프로바이더 (`crates/harness-cli/src/main.rs:899` `is_openai_model`) 는
모델 이름이 `openai/`, `gpt-`, `o1`, `o3`, `o4` 로 시작하면 OpenAI-호환 경로로 라우트하고,
앞의 `openai/` 접두어는 떼고 `${OPENAI_BASE_URL}/chat/completions` 로 POST 를 보냅니다.
LM Studio 의 로컬 서버가 바로 이 "OpenAI-호환" 규약을 따르므로, 두 줄의 환경변수 + `--model`
한 줄이면 붙습니다.

---

## 0. TL;DR

```bash
# 1) LM Studio 앱 설치 후 Discover 탭에서 Qwen2.5-Coder-14B-Instruct-GGUF 다운로드
# 2) 상단 모델 드롭다운에서 "Load" → Developer 탭 → "Start Server"
#    (또는 터미널에서: lms server start)
export OPENAI_BASE_URL=http://localhost:1234/v1
export OPENAI_API_KEY=lm-studio    # LM Studio 는 값 자체를 검사하지 않음. 빈 문자열만 Harness 가 거부.
harness ask --model openai/qwen2.5-coder-14b-instruct "이 레포의 TODO 를 모아줘"
```

`openai/` 접두어는 **Harness 전용 라우팅 신호** 입니다 — 실제 요청 본문의 `model` 필드에는
`qwen2.5-coder-14b-instruct` 만 들어갑니다.

---

## 1. 언제 LM Studio 를 선택하나

| 결 | LM Studio | Ollama | 원격 Anthropic/OpenAI |
|---|---|---|---|
| 설치 방식 | GUI 인스톨러 (`.dmg` / `.exe` / `.AppImage`) | Homebrew · curl 스크립트 | 키 발급 |
| 모델 선택 | Discover 탭 → 검색 → 클릭 | `ollama pull <name>` | 모델 카탈로그 |
| Quantization 선택 | **드롭다운에서 Q4_K_M / Q5_K_M / Q8 / MLX 직접 고름** | 프로필 하나로 고정 | 해당 없음 |
| 서버 가동 | GUI 토글 or `lms server start` | 데몬이 백그라운드 상주 | 해당 없음 |
| 비용 | 로컬 전기세 · VRAM | 로컬 전기세 · VRAM | API 토큰 과금 |
| 네트워크 프라이버시 | 데이터 로컬 | 데이터 로컬 | 프로바이더에 전송 |
| 단점 | 앱 자체는 closed-source (Apache-2 인 `lms` CLI 는 오픈) | 변화량 많음 · GUI 없음 | 네트워크 · 과금 |

**LM Studio 를 고를 타이밍.**

- "터미널에 `ollama pull foo:Q4_K_M` 치는 게 낯설다. Quantization 이 뭔지 비교하면서 고르고 싶다."
- Apple Silicon 에서 **MLX 백엔드** 를 써서 토큰/초를 더 짜내고 싶다 (§9 참조).
- Ollama 를 이미 써봤는데, 모델 파일 관리가 `~/.ollama/models/` 로 투명하지 않은 게 아쉬웠다.
  LM Studio 는 `~/.lmstudio/models/` 아래에 HuggingFace 구조 그대로 내려놓는다.

**피할 타이밍.**

- 서버 · 헤드리스 · 컨테이너에서만 돌린다 — GUI 가 부담. `lms` CLI 만으로도 가능은 하나,
  daemon 화 · systemd unit 예시는 Ollama 쪽이 훨씬 얕다.
- 툴 콜링 필수이고 모델 선택 자유도가 낮다 — §6 참조 (모델 템플릿에 tool 토큰이 있어야 함).

---

## 2. 하드웨어 현실 체크

대략적인 VRAM / 통합 메모리 요구량. Q4_K_M (4비트) 기준. Q5_K_M · Q8 는 각각 +25% · +100% 로 잡으세요.

| 모델 크기 | 최소 | 추천 | 예시 |
|---|---|---|---|
| 7–8B | 6 GB | 8 GB | Qwen2.5-Coder-7B, Llama-3.1-8B |
| 13–14B | 10 GB | 16 GB | Qwen2.5-Coder-14B |
| 32–34B | 20 GB | 24 GB | Qwen2.5-Coder-32B |
| 70–72B | 40 GB | 48 GB+ | Llama-3.3-70B |

Apple Silicon 은 통합 메모리에서 빼쓰므로 M-시리즈 맥북 Pro 36–48 GB 권장. NVIDIA 는 VRAM —
시스템 RAM 으로 spill 되면 토큰/초가 10× 이상 떨어짐. 첫 토큰 지연은 prompt length 에 비례:
14B Q4_K_M / M2 Pro / 3k 프롬프트에서 2–4 초 정도.

---

## 3. 설치

### 3.1 데스크톱 앱

공식 다운로드 페이지: <https://lmstudio.ai>

- **macOS** — `.dmg` 를 받아 Applications 로 드래그. Apple Silicon 과 Intel 빌드가 분리되어 있으므로 본인 칩셋에 맞는 걸 받습니다. M-시리즈면 반드시 Apple Silicon 빌드를 받아야 MLX 가 켜집니다.
- **Windows** — `.exe` 인스톨러. WSL 이 아니라 네이티브 Windows 빌드입니다.
- **Linux** — `.AppImage`. 실행 권한 (`chmod +x LM-Studio-*.AppImage`) 후 더블클릭 또는 터미널 실행. 배포판 특정 `.deb` / `.rpm` 은 현재 제공되지 않음.

### 3.2 `lms` CLI 부트스트랩

LM Studio 는 0.2.22 부터 `lms` 라는 공식 CLI 를 앱 번들 안에 같이 깔아둡니다.
PATH 에 잡으려면 한 번 부트스트랩 해야 합니다.

- 공식 방법 (플랫폼별):
  - macOS / Linux: `~/.lmstudio/bin/lms bootstrap`
  - Windows: `cmd /c %USERPROFILE%/.lmstudio/bin/lms.exe bootstrap`
- 또는 Node 환경이 있으면: `npx lmstudio install-cli`

부트스트랩 후 **새 터미널** 을 열고 `lms --help` 가 나오면 성공입니다.
이 단계 없이도 앱의 "Developer" 탭으로 서버를 돌릴 수 있지만, headless 박스
(예: 원격 워크스테이션 SSH 세션) 에서는 `lms` 가 사실상 필수입니다.

---

## 4. 모델 받기

### 4.1 어떤 모델을 받을 것인가

Harness 는 툴 콜링에 크게 기댑니다 (14 개 내장 툴을 assistant 가 `tool_use` 블록으로 호출).
따라서 **툴-aware chat template 이 있는 모델** 을 골라야 의미가 있습니다. 안정적인 조합:

- **Qwen2.5-Coder-14B-Instruct (Q4_K_M)** — 추천 시작점. 14B / 10 GB.
- Qwen2.5-Coder-7B-Instruct (Q4_K_M) — 저사양 (6 GB).
- Qwen2.5-Coder-32B-Instruct (Q4_K_M) — 24 GB 급 장비.
- DeepSeek-Coder-V2-Lite-Instruct (Q4_K_M) — MoE 16B / 활성 2.4B.
- Llama-3.1-8B-Instruct (Q4_K_M) — 일반 대화 폴백.

`lmstudio-community/` 네임스페이스는 LM Studio 팀이 직접 재-퀀타이즈해 올린 리포라
Discover 결과 상단에 잡힙니다. `bartowski/`, `unsloth/` 도 호환.

### 4.2 GUI 플로우

1. 앱 좌측 사이드바 **Discover** (돋보기 아이콘) 클릭.
2. 검색창에 `qwen2.5-coder 14b` 입력.
3. 결과 카드에서 **Q4_K_M** quant 의 "Download" 버튼 클릭.
4. 진행바가 끝나면 좌측 **My Models** 에 나타남.

### 4.3 CLI 플로우

```bash
# 단건 다운로드 (리포 전체가 아니라 특정 quant 하나만)
lms get lmstudio-community/Qwen2.5-Coder-14B-Instruct-GGUF

# 설치된 모델 목록
lms ls

# 특정 모델을 메모리에 로드 (서버가 참조할 대상 지정)
lms load qwen2.5-coder-14b-instruct
```

`lms get` 은 대화형으로 quant 를 물어보기도 합니다. 비대화식으로 돌려야 하면
`--quant Q4_K_M` 같은 플래그를 문서에서 확인하여 덧붙이세요.

---

## 5. 서버 기동

### 5.1 GUI

1. 좌측 사이드바 **Developer** (`</>` 아이콘) 클릭.
2. 상단 **Local Server** 패널의 "Select a model to load" 에서 모델 선택.
3. **Start Server** 토글 ON.
4. 우측에 `Running on http://localhost:1234` 배너가 뜨면 성공.

### 5.2 CLI

```bash
# 기본 (포트 1234) 기동
lms server start

# 다른 포트
lms server start --port 11434

# CORS 허용 (웹 브라우저에서 직접 붙이려는 경우. harness 는 로컬 프로세스라 OFF 로 충분)
lms server start --cors

# 상태 확인
lms server status

# 정지
lms server stop
```

포트 미지정 시 **직전에 쓰던 포트를 기억** 합니다 (최초 실행 시 1234).
만약 1234 가 이미 점유되어 있으면 과거 버전에서는 자동으로 1235 등으로 shift 하는 동작도
보고된 바 있으므로 (lms CLI GitHub Issue #80 참조), `lms server status` 로 실제 바인딩된
포트를 재확인하는 습관을 들이세요.

### 5.3 로드 상태 확인

"모델 다운로드 != 로드" 입니다. Harness 가 요청을 보내기 전에 반드시 **로드** 가 되어 있어야 합니다.

```bash
# 현재 로드된 모델
lms ps

# /v1/models 로 HTTP 로 확인
curl -s http://localhost:1234/v1/models | jq '.data[].id'
```

`.data[].id` 에 문자열이 나오면 그것이 `--model openai/<이 문자열>` 에 넣을 ID 입니다.

---

## 6. Harness 설정

### 6.1 환경변수

```bash
# 필수 2 종
export OPENAI_BASE_URL=http://localhost:1234/v1
export OPENAI_API_KEY=lm-studio   # 값은 무엇이든 OK. 단, 빈 문자열/공백 전용은 Harness 가 거부 (is_openai_model 호출부의 env_has 검사).

# 선택: 세션마다 같은 모델 쓰려면 미리 박아둠
export HARNESS_MODEL=openai/qwen2.5-coder-14b-instruct
```

### 6.2 실행

```bash
# 단발 질의
harness ask --model openai/qwen2.5-coder-14b-instruct "hi"

# 로컬 모델로 레포 탐색
harness ask --model openai/qwen2.5-coder-14b-instruct \
  "이 레포에서 TODO 를 모아서 파일:라인 기준으로 리포트해줘"

# 세션 이어가기
harness session list
harness session resume a1b2c3 "위 결과 중 3 번 TODO 를 구현해줘" \
  --max-turns 30
```

### 6.3 모델 이름 매칭에 대한 주의

LM Studio 는 `/v1/chat/completions` 요청의 `model` 필드를 **느슨하게** 매칭합니다.
로드된 모델이 하나뿐이라면 `model` 을 빈 문자열로 보내도 그걸 쓰는 경향이 있고,
`qwen2.5-coder-14b` 처럼 일부만 써도 통과합니다. 하지만 여러 모델을 동시에 로드해두고
라우팅을 기대한다면 `lms ps` / `curl /v1/models` 로 확인한 **정확한 ID** 를 쓰세요.
Harness 는 `openai/` 접두어만 제거하고 나머지는 그대로 바디에 넣기 때문에, ID 가
정확하면 항상 정확하게 라우트됩니다.

### 6.4 `.harness/settings.json` 으로 고정하기

레포별로 LM Studio 를 강제하려면 `model` 필드를 `"openai/qwen2.5-coder-14b-instruct"` 로
박아두고, `OPENAI_BASE_URL` / `OPENAI_API_KEY` 는 env 로만 잡습니다 — `settings.json` 에
plaintext secret 을 넣는 건 Harness 가 거부합니다 (README §8.2).

---

## 7. 툴 콜링

LM Studio 0.3.6 (2024-12) 부터 **OpenAI-호환 Tool Use API** 가 정식 지원됩니다.
`/v1/chat/completions` 요청의 `tools` / `tool_choice` 를 해석하고, 모델 출력에서
함수 호출을 파싱해 `choices[0].message.tool_calls` 필드에 넣어 돌려줍니다. 이 동작이
Harness 턴 루프가 기대하는 계약과 정확히 맞습니다.

다만 **모델 의존** 이라는 점이 함정입니다.

- 성공 사례: Qwen2.5-Coder (7B/14B/32B), Llama-3.1-Instruct (8B/70B), Mistral-Large-Instruct-2407, Hermes-3 계열.
- 실패 사례: 베이스 모델 (non-instruct), chat template 에 tool 토큰이 정의되지 않은 파인튜닝.

확인 방법:

```bash
# 1) 한 줄짜리 스모크: assistant 가 Read 를 부르는 프롬프트
harness ask --model openai/qwen2.5-coder-14b-instruct \
  "현재 디렉토리의 파일 목록을 보여줘"

# stderr 에 ⏺ Glob(...) 또는 ⏺ Bash(ls) 같은 tool 호출 라인이 떠야 정상.
# 안 뜨고 텍스트로 "ls 를 실행해 보세요" 같은 답만 오면 → 모델이 tool 을 안 썼거나,
# LM Studio 버전이 낮아 tool 파싱이 없는 것.
```

오래된 LM Studio (0.3.5 이하) 는 `tools` 파라미터를 **조용히 무시** 합니다 — 요청은
200 으로 떨어지지만 `tool_calls` 가 비어 있어 Harness 의 턴 루프가 "아무것도 안 함" 으로
종료됩니다. 이 경우 LM Studio 를 **0.3.6 이상** 으로 업데이트하세요.

### 7.1 Tool-aware 여부 사전 점검

curl 로 `tools` 배열을 직접 보내 `choices[0].message.tool_calls` 가 돌아오는지 확인.
예시 페이로드:

```json
{
  "model": "qwen2.5-coder-14b-instruct",
  "messages": [{"role":"user","content":"call the ping tool"}],
  "tools": [{"type":"function","function":{"name":"ping","description":"returns pong","parameters":{"type":"object","properties":{}}}}],
  "tool_choice": "auto"
}
```

`tool_calls` 가 null / 누락이면 모델 혹은 LM Studio 버전 문제입니다.

---

## 8. 검증 (smoke test)

LM Studio 가 뜨고 Harness 가 붙기까지의 단계를 3 단으로 쪼개서 검증합니다.

### 8.1 서버 살아있는지

```bash
curl -sf http://localhost:1234/v1/models | jq '.data[].id'
# 출력 예: "qwen2.5-coder-14b-instruct"
```

비면 "모델 로드 안 됨", 200 안 돌아오면 "서버 다운". §10 트러블슈팅으로.

### 8.2 OpenAI-호환 chat 최소 호출

```bash
curl -s http://localhost:1234/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder-14b-instruct",
    "messages": [{"role":"user","content":"hi"}]
  }' | jq '.choices[0].message.content'
```

짧은 인사 한 줄이 돌아오면 OK.

### 8.3 Harness 로 한 번

```bash
export OPENAI_BASE_URL=http://localhost:1234/v1
export OPENAI_API_KEY=lm-studio
harness ask --model openai/qwen2.5-coder-14b-instruct "ping?"
```

stdout 으로 한 줄짜리 답이 나오면 파이프라인 전체가 이어진 것.
그 다음 툴 콜링 단(§7) 으로 넘어갑니다.

---

## 9. MLX 백엔드 vs. llama.cpp 백엔드 (Apple Silicon 한정)

LM Studio 는 Apple Silicon 에서 **두 런타임** 을 제공합니다. 같은 GGUF 파일이라도
어떤 런타임으로 돌릴지 모델별로 따로 고를 수 있습니다.

| 축 | llama.cpp | MLX |
|---|---|---|
| 호환성 | GGUF 전부 | GGUF + MLX-전용 포맷 |
| 토큰/초 (generation) | 기준 | **일반적으로 빠름**, 특히 큰 모델에서 격차 벌어짐 |
| TTFT (prompt fill) | 비슷 | 비슷 — prompt length 지배 |
| Continuous batching | 지원 | LM Studio 0.4.2 (2026-02) 부터 지원. 그 이전 (0.4.0 / 0.4.1) 은 순차 처리 |
| 플랫폼 | 크로스플랫폼 | **Apple Silicon 전용** |
| Fine-tune | 추론 전용 | `mlx-lm` 으로 LoRA/QLoRA 가능 (앱 밖에서) |

**Harness 관점에서의 추천.**

- Apple Silicon M1/M2/M3/M4 → **MLX** 로 고르세요. Harness 의 인터랙티브 턴 루프는
  토큰 stream 이 빠를수록 체감이 좋습니다.
- 모델이 MLX 로 안 뜬다 (LM Studio 가 "MLX not available for this model" 경고) →
  llama.cpp 로 폴백. 모델 리포에 `-MLX` suffix 가 붙은 별도 리포가 있는 경우가 많습니다.
- Intel Mac · Windows · Linux 는 선택권이 없습니다 — llama.cpp 가 유일.

### 9.1 런타임 바꾸기

GUI: **My Models** → 모델 옆 톱니바퀴 → "Runtime" 드롭다운.
앱 전역 런타임 매니저: macOS 에서 **⌘⇧R**, Windows/Linux 에서 **Ctrl+Shift+R** 로 엽니다.

---

## 10. 트러블슈팅

증상별로 가장 흔한 원인부터.

### 10.1 `connection refused` / Harness 가 "network error" 로 실패

1. 서버가 안 떠 있음. GUI 의 "Start Server" 토글 또는 `lms server start`.
2. 포트가 다름. `lms server status` 로 실제 바인딩된 포트 확인 후
   `OPENAI_BASE_URL=http://localhost:<port>/v1` 로 갱신. 과거 LM Studio 버전은
   1234 가 점유되어 있으면 1235 등으로 자동 shift 하기도 했습니다.
3. 방화벽 / Little Snitch / Windows Defender Firewall 이 `localhost` 트래픽을 막고 있음
   (드물지만 엔터프라이즈 노트북에서 발생).

### 10.2 `{"error": "model not found"}`

- 다운로드 != 로드. Discover 탭에서 내려받기만 했다면 아직 메모리에 안 올라가 있습니다.
  GUI 상단 드롭다운에서 **Load** 또는 `lms load <model-id>`.
- `model` 필드 ID 가 실제 로드된 ID 와 다름. `curl /v1/models` 로 정확한 ID 재확인.
- 한 모델만 로드된 경우 LM Studio 는 ID 불일치에도 관대하지만, 여러 모델이 로드되어
  있으면 엄격해집니다.

### 10.3 Tool 호출이 아예 안 일어남 (assistant 가 말로만 답함)

1. **LM Studio 가 0.3.5 이하** — 업데이트. Tool Use API 는 0.3.6 부터.
2. **모델이 tool-aware 하지 않음** — Qwen2.5-Coder / Llama-3.1-Instruct / Mistral-Large
   계열로 교체. 베이스 (non-instruct) 모델은 모두 탈락.
3. **Quant 가 너무 공격적** — Q2_K 같은 2비트 양자화는 tool 토큰 생성 품질을 무너뜨려
   Harness 가 tool_call 로 파싱하지 못하는 결과를 냅니다. Q4_K_M 이상 권장.
4. 검증: §7.1 의 curl `tools` 스모크 돌려서 서버단에서 tool_calls 가 나오는지 직접 확인.

### 10.4 Apple Silicon 에서 첫 토큰이 오래 걸림

- MLX/Metal 셰이더가 **cold start 시 JIT 컴파일** 됩니다. 모델 로드 후 첫 요청 1–2 회는
  느리고, 이후는 정상 속도가 납니다.
- 모델 로드 자체도 메모리 맵핑 + 필요 시 decompress 가 들어가 수 초 걸립니다.
- 대응: 쓰기 전에 `curl` 한 방으로 워밍업. Harness 세션 시작 직전에 "ping" 을 한 번
  돌리는 셸 alias 를 두는 것도 방법.

### 10.5 응답이 중간에 잘림 / `finish_reason: length`

- `max_tokens` 기본값이 작게 잡혀 있을 수 있음. LM Studio Developer 탭 →
  Settings → "Max tokens" 조정 (기본값은 모델의 `context_length` 와 별개).
- 또는 컨텍스트 길이 자체를 확장해야 함 (동 탭의 "Context Length").
  14B 모델에서 16k → 32k 로 늘리면 VRAM 사용량이 늘어나므로 OOM 주의.

### 10.6 기타

- `HARNESS_REFUSE_API_KEY=1` 로 차단됨 — 의도한 안전 기능. §11 참조.
- `lms: command not found` — 부트스트랩 (§3.2) 안 했거나 새 터미널 안 연 것.
- 로그 확인 — 앱: Developer 탭 콘솔. CLI: `lms log stream`.

---

## 11. 보안 · 비용 · 운영 팁

- **데이터는 로컬에 남음.** `OPENAI_BASE_URL=http://localhost:1234/v1` 인 한 Harness 가
  보내는 prompt · 툴 결과 · diff 는 같은 머신 안에서 끝납니다. `lms server start --cors`
  또는 `--host 0.0.0.0` 을 켰다면 LAN 노출. 의도 없으면 기본값 유지.
- **원격 키 경로 차단.** `export HARNESS_REFUSE_API_KEY=1` 로 Anthropic / OpenAI 원격
  경로를 통째로 꺼둘 수 있습니다. 로컬 base_url 만 허용 — 공용 랩탑에서 유용.
- **localhost 강제 wrapper.** 필요하면 `case "$OPENAI_BASE_URL" in http://localhost:*|
  http://127.0.0.1:*) harness "$@" ;; *) return 2 ;; esac` 같은 셸 함수로 감싸기.
- **자동 시작 끄기.** `Settings → General → Launch at login` 해제 후, 필요할 때만
  `lms server start`.
- **모델 저장 경로.** 기본 `~/.lmstudio/models/`. 외장 SSD 로 바꾸려면
  `Settings → Model Directory`. 이사 중엔 앱 종료.

---

## 12. 체크리스트 & 참고

1. LM Studio 앱 설치 후 실행 → (선택) `~/.lmstudio/bin/lms bootstrap`.
2. Discover 탭에서 **Qwen2.5-Coder-14B-Instruct-GGUF (Q4_K_M)** 다운로드.
3. Apple Silicon 이면 Runtime 을 **MLX** 로.
4. 모델 **Load** → Developer 탭 **Start Server** (또는 `lms server start`).
5. `curl http://localhost:1234/v1/models` 로 ID 확인.
6. `OPENAI_BASE_URL=http://localhost:1234/v1`, `OPENAI_API_KEY=lm-studio`, (선택) `HARNESS_REFUSE_API_KEY=1`.
7. `harness ask --model openai/<id> "현재 디렉토리 파일 목록"` → stderr 에 `⏺ Glob(...)` 확인.

Harness 의 `is_openai_model` 라우팅은 모델 네임만 보고 Anthropic/OpenAI-호환을 결정하므로,
LM Studio · Ollama · vLLM · llama.cpp-server · OpenRouter 어느 것이든 `OPENAI_BASE_URL` 만
바꿔서 같은 방식으로 붙습니다.

### 참고 링크

- LM Studio 홈: <https://lmstudio.ai>
- `lms` CLI 문서: <https://lmstudio.ai/docs/cli>
- `lms server start` 레퍼런스: <https://lmstudio.ai/docs/cli/serve/server-start>
- OpenAI 호환 엔드포인트: <https://lmstudio.ai/docs/developer/openai-compat>
- Tool Use 문서: <https://lmstudio.ai/docs/developer/openai-compat/tools>
- LM Studio 0.3.6 릴리스 노트 (Tool Use 도입): <https://lmstudio.ai/blog/lmstudio-v0.3.6>
- LM Studio 0.3.4 릴리스 노트 (MLX 백엔드 도입): <https://lmstudio.ai/blog/lmstudio-v0.3.4>
- `lms` CLI 소스: <https://github.com/lmstudio-ai/lms>

Harness 측 근거: `crates/harness-cli/src/main.rs:899` (`is_openai_model`),
`crates/harness-cli/src/main.rs:892-895` (`--base-url` / `OPENAI_BASE_URL` 적용).
