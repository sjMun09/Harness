# Harness

> **Harness 는 터미널에서 쓰는 코딩 에이전트 CLI 입니다.**
> iTerm / zsh 같은 터미널 *안에서* `harness ask "..."` 라고 치면,
> LLM 기반 에이전트가 파일을 읽고 · 검색하고 · 편집하고 · 테스트까지 돌려서
> 결과를 보고합니다.

가장 가까운 비교 대상은 Anthropic 의 **Claude Code** (`claude` 명령) 입니다.
Harness 는 같은 워크플로 (분석 → 판단 → 구현 → 테스트 검증) 를 제공하되,
두 가지 목표로 다시 쓴 Rust 재구현입니다.

1. **Rust 단일 정적 바이너리** — 시작 속도, 파일 I/O, grep / glob, tokenize 가 네이티브 속도. Node 런타임 부트스트랩 비용이 없습니다.
2. **레거시 백엔드 리팩토링 1 급 지원** — XML · SQL · Freemarker · MyBatis 매퍼 체인 분석, 동적 SQL 동치성 검증, 템플릿 렌더 비교를 내장 툴로 제공합니다.

설계 세부는 [`PLAN.md`](PLAN.md) (v3 기준) 에 있고, 이 문서는
**지금 어떤 기능이 있고 어떻게 쓰는지** 에 집중합니다.

---

## 이거, 정확히 뭔가요?

자주 생기는 오해부터 먼저 정리합니다.

**Harness 는 이런 게 _아닙니다_.**

- ❌ 터미널 에뮬레이터가 아닙니다 (iTerm · Alacritty 같은 것 X).
- ❌ 쉘이 아닙니다 (bash · zsh 같은 것 X).
- ❌ 터미널 멀티플렉서가 아닙니다 (tmux · zellij · cmux 같은 것 X).

**Harness 는 _CLI 프로그램_ 입니다.**

여러분이 평소 쓰는 터미널 (iTerm + zsh 든, VSCode 내장 터미널이든) 안에서
`harness ask "..."` 라고 치면, 에이전트가 현재 디렉토리의 코드베이스를
**분석 → 수정 → 검증** 해 줍니다.

**Claude Code (`claude`) 와 같은 카테고리인가요?**

네, 직접 대체재입니다. 같은 자리에 놓고 써도 되는 도구입니다.
설계·스코프 차이는 아래 [Claude Code 와의 비교](#claude-code-와의-비교) 섹션 참고.

---

## 만든 계기

1. **레거시 코드베이스를 Claude Code 로 다루기가 불편했습니다.**
   대형 SQL-XML · Freemarker · MyBatis 매퍼 체인을 리팩토링하려면
   include 그래프 추적, 동적 SQL 분기 동치성 검증, 렌더 결과 비교 같은
   도메인 특화 툴이 필요한데, 범용 에이전트는 매번 ad-hoc grep + `cat` +
   수작업으로 그 간극을 메워야 했습니다.

2. **콜드 스타트가 느렸습니다.**
   Node 기반 CLI 는 한 줄 질의에도 수 초의 부트스트랩이 발생합니다.
   짧은 질의의 왕복 비용을 줄이려면 런타임 부트스트랩이 없어야 했습니다.

3. **단일 바이너리 + 내부 구조 소유권.**
   권한 시스템 · plan-gate · 롤백 · 백그라운드 bash · 컨벤션 주입 훅 —
   전부 기존 Claude Code 의 계약(권한 문법, 훅 이벤트, `settings.json` 포맷) 을
   최대한 호환하면서도 제 환경에 맞게 조일 수 있어야 했습니다.

그래서 외부 인터페이스는 Claude Code 와 최대한 호환으로 두고,
내부만 Rust 로 새로 썼습니다.

---

## 왜 Rust 로 만들었나 (솔직한 근거)

Rust 재구현이 **실제로 개선하는 축** 과 **category error (껍데기를 바꿔도 안 바뀌는 축)** 를 구분합니다. "Rust 니까 빠르다" 는 진술은 막연해서, 항목별로 솔직하게 정리.

| 항목 | Rust로 재구현 시 개선? | 왜 |
|---|---|---|
| 토큰 사용량 | ❌ 거의 없음 | 토큰은 모델(Opus 4.7)이 쓴다. 껍데기를 바꿔도 모델이 같으면 토큰은 같다. 시스템 프롬프트/툴 스키마 verbosity 에서 마진 차이만 있음. |
| 메모리 RSS | ✅ 있음 | Node + deps = ~100-200MB vs Rust 정적 바이너리 <20MB. 단건엔 체감 없고, 동시 다수 에이전트 띄울 때 차이. |
| Cold start (프로세스 생성) | ✅ 가장 큼 | Node 부트: ~300-1000ms vs Rust: <50ms. `ask "hi"` 같은 단발 호출에서 직접 체감. 긴 대화 세션에선 1회성이라 무의미. |
| Throughput | ❌ 없음 | 병목은 모델 API SSE. Rust든 Node든 같은 속도로 대기. |
| 파일 I/O (Read/Glob/Grep) | ✅ 있음 | mmap, 네이티브 `ignore` + `globset` + `grep-searcher`. 큰 레포에서 차이. |
| 배포 | ✅ 있음 | 단일 정적 바이너리. `npm install` 지옥, Node 버전 mismatch 없음. |
| 제어권(권한 grammar/plan-gate/롤백) | ✅ 큼 | 내부 계약 소유. 이게 진짜 만든 이유. |

정직한 결론: 속도 이득은 "cold start + 파일 툴" 두 축이 전부다. 토큰·throughput 은 모델이 결정하는 축이라 harness 가 바꿀 수 있는 게 아니다. 만든 진짜 이유는 "느려서" 보다 **레거시 XML/SQL/Freemarker 리팩토링용 도메인 툴을 1급으로 내장하고, 권한 모델을 직접 설계하고 싶어서**. 속도는 부산물.

---

## 목표 사용 예

```bash
harness ask "demandPlan_sql.xml 의 pivot 을 프리마커로 수정해줘"
```

위 한 줄이 다음 흐름으로 동작합니다.

- 레포 컨벤션(HARNESS.md · sibling 파일) 자동 참조
- bucket pattern vs Freemarker 분기 판단
- 실제 수정 → `DiffExec` 으로 before / after 렌더 비교
- 테스트 실행 → 결과 리포트

적용 범위는 도메인 무관 — 백엔드 (SQL XML · Freemarker · MyBatis · Java · Kotlin · Go),
프론트 (React · Vue · CSS), LLM 작업 (프롬프트 · eval) 모두 가능합니다.

**Non-goal**: Claude Code 의 모든 생태계를 복제하지 않습니다
(skills 마켓플레이스 · IDE 확장 · 웹 세션 X, MCP 서버 미지원).
벤더는 Anthropic 1 급, OpenAI 2 급, 나머지는 BYO 입니다.

---

## 현재 상태 (2026-04)

- **iter-1 (MVP)**: 완료. workspace scaffold + 6 primitives(token, perm, mem, config, fs_safe, proc) 커밋됨.
- **iter-2**: 5 개 병렬 에이전트 통합 완료(커밋 `feat(iter-2): integrate 5 parallel-agent outputs`).
- **iter-2 후속 통합**: TUI 엔진 브릿지(`--tui`) / `harness-testkit` 추출 / Anthropic SSE 파서 · `--base-url` · E2E HTTP 하니스 모두 머지됨.
- **테스트** (2026-04 기준): 기본 빌드 **328 pass**, `--features tui` 빌드 **334 pass**, 실패 0.

---

## 설치 & 빌드

```bash
# 필요: Rust stable (1.90+ — rust-toolchain.toml 기준)
git clone https://github.com/sjMun09/Harness.git
cd Harness
cargo build --release
# 바이너리: ./target/release/harness
```

TUI 기능을 켜서 빌드하려면:

```bash
cargo build --release -p harness-cli --features tui
```

워크스페이스 전체 테스트:

```bash
cargo test --workspace
```

---

## 빠른 시작

```bash
# Anthropic API key 로 로그인
export ANTHROPIC_API_KEY=sk-ant-...

# 한 번의 질문-응답 턴
harness ask "이 레포에 있는 TODO 를 모아서 리포트해줘"

# 모델 지정 (기본: claude-opus-4-7)
harness --model claude-sonnet-4-6 ask "이 파일 구조 설명해줘"

# 프롬프트를 파일로 관리 (길어서 셸 한 줄에 안 들어갈 때)
harness ask "$(cat prompts/task.md)"

# TUI 로 실행 (대화창 + 툴 패널) — 빌드 시 --features tui 필요
harness --tui ask "이 레포 구조 설명해줘"

# 이전 세션 이어가기
harness session list                # 최근 세션 id 확인
harness session resume a1b2c3 "위 분석 기반으로 수정까지 진행해줘"

# Claude Code 에서 쓰던 권한 설정 가져오기 (allow → ask 로 자동 다운그레이드)
harness config import
```

예상 출력(라인 모드):

```
⏺ Glob(**/*.rs)
  ↳ ok: 64 files
⏺ Grep(TODO|FIXME)
  ↳ ok: 12 matches in 7 files
TODO 목록
1. crates/harness-core/src/engine.rs:412 ...
```

툴 호출은 stderr 에 `⏺ Tool(args)` / `↳ ok|err: <summary>` 로 찍히고, 최종 assistant 텍스트만 stdout 으로 나온다. 그래서 `harness ask ... > out.txt` 로 깨끗하게 답만 캡처할 수 있다.

### harness 는 언제 도는가

자주 오해되는 부분이라 명시.

- harness 는 데몬이 아니다. 백그라운드 상주 프로세스 없음.
- `harness ask "..."` 를 치는 **그 순간에만** 프로세스가 뜨고, 모델 호출 + 툴 실행 후 종료.
- Claude Code (`claude`) 와 동등한 카테고리의 CLI. 같은 자리에 놓고 쓴다.

### PATH 등록

`cargo build --release` 로 만든 바이너리는 `./target/release/harness` 에 있어서 기본 PATH 에 없다. 아래 중 택일.

```bash
# 영구
echo 'export PATH="$HOME/Desktop/Harness/target/release:$PATH"' >> ~/.zshrc

# 또는 시스템 cargo bin 으로 설치
cargo install --path crates/harness-cli
```

### Claude Max OAuth 자동 폴백

`ANTHROPIC_API_KEY` 없어도 `claude` 로 로그인돼 있으면 harness 가 macOS 키체인(`Claude Code-credentials`) 의 OAuth 토큰을 자동으로 씀. 즉 Max 구독자는 별도 키 불필요. 강제 선택은 `--auth oauth` / `--auth api-key`.

**우선순위 (기본 `auto` 모드)** — OAuth 우선, API 키는 폴백. `ANTHROPIC_API_KEY` 가 쉘 env 에 남아있어도 OAuth 가 정상이면 그쪽으로 가고 과금 API 는 타지 않음.

```bash
# 1) OAuth 키체인에서 토큰 로드 시도 → 성공 시 이걸로 감 (과금 X)
# 2) 실패 & ANTHROPIC_API_KEY 있음 → API 키 폴백 (과금 O)
# 3) 실패 & API 키도 없음 → 에러
```

### 과금 차단 락 — `HARNESS_REFUSE_API_KEY=1`

"절대 과금 안 됨" 을 보장하고 싶으면 쉘 설정에 아래를 추가:

```bash
export HARNESS_REFUSE_API_KEY=1
```

이 락이 걸려 있으면:
- `--auth api-key` 명시해도 거부 (clear 에러로 빠짐).
- `auto` 모드에서 OAuth 실패해도 API 키 폴백 안 함.
- OpenAI 모델 (`gpt-*`/`o1`/`o3`/`openai/...`) 도 거부 — 이쪽도 과금 경로라.
- OAuth 만 허용. Max/Pro 구독 범위 안에서만 동작.

해제하려면 해당 한 번만: `HARNESS_REFUSE_API_KEY= harness ask "..."` 또는 쉘 rc 에서 제거.

### OAuth 재사용 — TOS 주의

OAuth 경로는 Claude Code 의 기존 인증을 재사용합니다. Anthropic 의 Max/Pro 이용약관 범위를 넘을 수 있으므로 사용 여부는 사용자 판단입니다. 현재 Anthropic 의 rate-limit 정책으로 인해 실무적으로 API 키 경로나 로컬 LLM 을 권장합니다.

### 로컬 LLM 으로 돌리기 (Ollama / vLLM / LM Studio / llama.cpp / MLX)

harness 는 OpenAI 호환 엔드포인트를 갈아끼울 수 있으므로 대부분의 로컬 LLM 런타임이 드랍인 대체 가능. 모델 이름을 `openai/<id>` 로 주면 OpenAI 프로바이더로 라우팅되고, 실제 요청은 `{base_url}/v1/chat/completions` 로 감.

**가장 간단한 예시 (Ollama + 인자 한 줄):**

```bash
# 1회 스폰만 필요 — 이후엔 CLI 플래그로 끝
ollama serve &
ollama pull qwen2.5-coder:14b

harness ask \
  --model openai/qwen2.5-coder:14b \
  --base-url http://localhost:11434/v1 \
  "이 레포 설명해줘"
```

`--base-url` 이 `localhost` / `127.0.0.1` / `::1` 로 resolve 되면:
- `OPENAI_API_KEY` 환경변수 **없어도 됨**. 있으면 그대로 쓰고, 없으면 플레이스홀더 `Bearer local` 로 전송 — 로컬 런타임들은 어차피 bearer 를 검증하지 않음.
- **`HARNESS_REFUSE_API_KEY=1` 락이 걸려 있어도 통과**. 로컬 추론은 과금 경로가 아님 — 락은 외부 API 로 흘러나가는 트래픽만 차단.

환경변수로 고정하고 싶으면 `export OPENAI_BASE_URL=http://localhost:11434/v1` 해둬도 동일하게 동작. 플래그가 env 보다 우선.

런타임별 세부 셋업 (포트·설치·툴콜 이슈·트러블슈팅) 은 `docs/local-llm/` 에 각각 정리되어 있음:

| 런타임 | 플랫폼 | 기본 포트 | 문서 |
|---|---|---|---|
| Ollama | mac/linux/win | 11434 | [docs/local-llm/ollama.md](docs/local-llm/ollama.md) |
| vLLM | linux+CUDA (주로) | 8000 | [docs/local-llm/vllm.md](docs/local-llm/vllm.md) |
| LM Studio | mac/linux/win GUI | 1234 | [docs/local-llm/lm-studio.md](docs/local-llm/lm-studio.md) |
| llama.cpp | 어디서나 (CPU/Metal/CUDA) | 8080 | [docs/local-llm/llama-cpp.md](docs/local-llm/llama-cpp.md) |
| MLX | Apple Silicon 전용 | 8080 | [docs/local-llm/mlx.md](docs/local-llm/mlx.md) |

오버뷰 + 런타임 선택 가이드는 [`docs/local-llm/README.md`](docs/local-llm/README.md).

**툴콜 주의** — harness 의 에이전트 루프는 모델이 제대로 된 `tool_calls` JSON 을 내뱉어야 굴러가는데, 오픈소스 모델의 툴콜 품질은 모델 + 런타임 + chat template 조합에 따라 들쭉날쭉함. 복잡한 태스크 전에 반드시 `harness ask "list files in ."` 같은 간단한 쿼리로 툴 호출이 실제로 발생하는지 스모크 테스트할 것.

---

## Claude Code 와의 비교

Harness 는 Claude Code 를 대체하려는 게 아니라 **특정 사용 결을 더 좋게** 하려는 도구다. 아래 표는 설계·스코프 차이이며, **실측 성능 수치는 아래 "벤치마크" 섹션에서 `bench/` 하니스를 돌려 직접 채우는 것**을 원칙으로 한다 — 아직 숫자는 의도적으로 비워 두었다.

### 설계 · 스코프

| 축 | Claude Code | Harness |
|---|---|---|
| **배포 형태** | Node 번들 (npm 전역 설치) | Rust 단일 정적 바이너리 (`cargo install`) |
| **시작 모델** | REPL/대화 중심 (세션이 길게 살아있음) | **단발성 턴**(`ask`) 우선, 세션 resume 은 명시적 (`session resume`) |
| **1 차 타겟** | 범용 개발(웹/앱/데이터/리서치) | **레거시 백엔드 리팩토링**(XML/SQL/Freemarker/MyBatis) 1 급 |
| **멀티 벤더** | Anthropic 1 급, OpenAI 별도 경로 | Anthropic 1 급, OpenAI 2 급(같은 `--model` 라우팅), 나머지 BYO |
| **툴 확장** | MCP 서버, skills 마켓플레이스 | 내장 14 종 + `Subagent` 툴, MCP 미지원 |
| **권한 모델** | `settings.json` allow/deny + 세션 permission | 동일 grammar + `HARNESS.md` + plan-gate(계획 위반 차단) |
| **승인 UI** | Ask 시 TUI 모달 | 라인 모드 stderr 프롬프트 / `--tui` 시 ratatui 모달 |
| **세션 저장** | 자체 포맷 | JSONL 스트리밍, `harness session show/list/resume` 재생 가능 |
| **취소 시맨틱** | Ctrl-C 후 재개 유도 | Ctrl-C → 부분 assistant 텍스트 보존 + `SessionExit::Cancelled` 마커 |
| **백그라운드 Bash** | 지원 | 지원 (`run_in_background` → `BashOutput`/`KillShell`, setsid + `PR_SET_PDEATHSIG`) |
| **롤백** | 없음 (git 수동) | `harness-tools::Transaction` — 턴 단위 스테이징, 실패 시 자동 되돌림 |
| **SSE 프레임 캡** | 내부 구현 | 1 MiB 하드 캡(프로바이더 DoS 방어) |
| **언어 · 런타임** | Node 18+ | Rust 1.90+ (외부 런타임 불필요) |
| **라이선스** | 비공개 | MIT OR Apache-2.0 (`Cargo.toml:9`) |

### 실측 성능

> **Disclaimer.**
> - 실측 숫자는 `bench/run.sh` 를 직접 돌려 채운다. 돌리지 않은 값은 공개하지 않는다. 날조 금지.
> - 측정 불가해서 삭제된 지표 (이유): glob 1k 스캔 (LLM 추론과 분리 불가), 10-파일 리팩토링 (fixture 레포 + 테스트 oracle 필요), 100-턴 실패율 (n>=100 필요, 실패 정의 부재).
> - n<10 은 지연 주장에 통계적으로 부적합. `run.sh` 기본값 n=20.

벤치 수행 방법 및 결과 집계 형식은 [`bench/README.md`](bench/README.md) 를 참고.

---

## CLI 레퍼런스

### 전체 문법

```
harness  [전역옵션]  <서브커맨드>  [서브옵션]  "프롬프트"
```

- **`harness`** — 바이너리 이름. 필수.
- **전역옵션** — 모든 서브커맨드에서 동작. 내부적으로 clap `global = true` 라서 서브커맨드 앞/뒤 어디에 와도 됩니다 (`harness --model X ask "..."` 와 `harness ask "..." --model X` 둘 다 가능).
- **서브커맨드** — `ask` / `session` / `config` 중 하나. 필수.
- **서브옵션** — 해당 서브커맨드 전용 옵션(예: `--max-turns`). 반드시 서브커맨드 뒤에.
- **프롬프트** — 여러분이 시키고 싶은 자연어 지시. 한 단어짜리 "hi" 부터, 여러 줄짜리 `"demandPlan.xml 의 pivot 을 Freemarker 로 바꾸고 테스트까지 통과시켜"` 같은 구체적 요구까지 전부 가능. 공백 포함 시 반드시 따옴표로 감쌉니다.

### 전역 옵션

| 플래그 | 설명 |
|---|---|
| `--model <NAME>` | 사용할 모델. `HARNESS_MODEL` env 또는 `settings.json` 의 `model` 과 precedence 동일. 기본값 `claude-opus-4-7`. |
| `--auth auto\|api-key\|oauth` | 자격증명 선택. `auto` 는 `ANTHROPIC_API_KEY` → Claude Code 키체인 순서로 폴백. |
| `--tui` *(feature=tui 빌드 시)* | ratatui 기반 TUI 모달로 실행 (라인 모드 stderr 대신). 현재 `ask` 만 지원. |
| `--verbose` / `-v` | DEBUG tracing 활성화. stderr 에 `[warn]` 배너 표시. |
| `--dangerously-skip-permissions` | 모든 Ask 권한 요청을 자동 Allow 처리. CI 에서 의도적으로 쓸 때만 사용. |
| `--trust-cwd` | 첫 로드 cwd 트러스트 프롬프트(§8.2) 생략. 비대화식 환경 전용. |
| `--base-url <URL>` | OpenAI-compatible / Anthropic base URL 오버라이드. 로컬 LLM (Ollama/vLLM/LM Studio/llama.cpp/MLX) 사용 시 이 flag 로 로컬 endpoint 지정 (`http://localhost:PORT/v1`). 자세한 건 `docs/local-llm/` 참조. |

### 서브커맨드

#### `ask` — 한 번의 턴 루프 실행

```bash
harness ask "<prompt>" [--max-turns 20]
harness ask -           < prompt.txt     # stdin 에서 읽기 (hyphen sentinel)
cat prompt.txt | harness ask             # stdin 이 TTY 가 아니면 자동 감지
```

**서브옵션**

| 플래그 | 기본값 | 설명 |
|---|---|---|
| `--max-turns <N>` | 20 | 턴 루프 상한. assistant ↔ tool 왕복 횟수의 한계. |

**동작**

- `prompt` 를 user 메시지로 모델에 전달 (자유 형식 — 짧은 질문부터 여러 줄 지시까지 가능).
- assistant 가 툴을 호출하면 실행 → 결과를 다시 넣음 → `end_turn` 또는 `max_turns` 까지 반복.
- `Ctrl-C` 한 번: 현재 진행 중 tool 취소 + 마지막까지 받은 partial assistant 텍스트 보존 후 exit code 130 반환.
- `Ctrl-C` 두 번: 쉘 SIGINT 로 즉시 종료.

세션은 자동으로 `$XDG_STATE_HOME/harness/sessions/<id>.jsonl` 에 저장됩니다 (다음 섹션 `session` 참조).

#### `session` — 세션 관리

```bash
harness session list                                  # 저장된 세션 목록
harness session show <id>                             # 헤더 + 트랜스크립트 head
harness session resume <id> "<new prompt>" [--max-turns 20]
```

`resume` 은 이전 JSONL 을 전부 메모리에 로드한 뒤 새 prompt 를 user 메시지로 append 하고 그대로 턴 루프를 돈다. 세션 JSONL 은 append-only(`fs4` 배타락, 0600 perm).

#### `config` — 설정 관리

```bash
harness config path                                   # settings.json 경로 출력
harness config show                                   # precedence 병합된 최종 설정 출력
harness config import                                 # Claude Code settings.json 가져오기
```

`config import` 는 Claude Code 의 `~/.claude.json` 을 읽어 해당 설정을 Harness 형식으로 변환한다. 단, 모든 `allow` 규칙은 **안전하게 `ask` 로 다운그레이드** 된다(§8.2). 사용자가 명시적으로 allow 로 재설정해야 한다.

---

## 내장 툴 (14종)

assistant 가 turn 중에 자연어 + tool_use 블록으로 호출할 수 있는 내장 함수들. 각 툴은 Rust `Tool` trait 의 인스턴스이며, argv 모드 Bash · 경로 canonicalize 등 안전장치가 기본 ON.

| 툴 | 역할 | 핵심 특징 |
|---|---|---|
| **Read** | 파일 읽기 | mmap, `cat -n` 포맷, 바이너리 자동 감지 / 거부, 크기 cap(20k 라인) |
| **Write** | 새 파일 쓰기 / 디렉터리 자동 생성 | tempfile + `renameat2(RENAME_NOREPLACE)` (Linux), `0600` perm |
| **Edit** | 정확 치환 | unique 검증 + `replace_all` 옵션 + unified diff 반환. 위험 경로(XML/ftl/migrations/schema)는 plan-gate 통과 후 2회차만 허용 |
| **Bash** | 명령 실행 | **argv 모드 기본**(shell injection 차단), `mode: "shell"` 명시 opt-in, `setsid` + 타임아웃 120s/600s, env allowlist(핵심 shell/XDG/ssh-agent/git-identity/language-toolchain 약 40종 — 시크릿 값이 들어가는 변수는 배제, `bash.rs::DEFAULT_ENV_ALLOW` 참조), stdout+stderr head 4KB + tail 4KB + `/tmp/harness-bash-*.log` 전체 로그 경로 |
| **BashOutput** | 백그라운드 작업 polling | `Bash(run_in_background=true)` 로 띄운 shell 의 신규 output 증분 drain. 선택적 regex `filter` 로 라인 필터. ring buffer(head 4KB + tail 4KB + consumer cursor) |
| **KillShell** | 백그라운드 작업 종료 | SIGTERM → 2s → SIGKILL 에스컬레이션, `PR_SET_PDEATHSIG`(Linux) |
| **Glob** | 파일 패턴 검색 | `ignore`(gitignore 존중) + `globset` |
| **Grep** | 콘텐츠 검색 | `grep-searcher` + `grep-regex`, `content/files/count` 3 모드 |
| **ImportTrace** | 의존 체인 추적 | MyBatis `<include>` / Freemarker `<#import>` 의 transitive 그래프. depth cap 32 + cycle detection + missing-ref warn+stub |
| **MyBatisDynamicParser** | 동적 SQL 동치성 검증 | 분기 수 일치 + normalized condition set 비교. before / after mapper 리팩토링의 **필요조건** 검사 |
| **DiffExec** | 렌더드 텍스트 diff | Freemarker 렌더 결과 / SQL 비교. mode=`sql` 은 comment/whitespace/keyword case 무시. "rendered text check, not semantic execution" 배너 |
| **Test** | 테스트 러너 통합 | `cargo test` / `mvn test` / `pytest` / `vitest` / `jest` / `playwright` / 커스텀. 출력 head+tail + full log 경로. 실패 파싱, 재시도 cap 3 |
| **Rollback** | 멀티파일 revert | 세션 전체 transaction — 편집된 모든 파일을 한번에 스냅샷 시점으로 되돌림. 새로 생성된 파일은 삭제. 여러 번 호출 안전 |
| **Subagent** | 서브 에이전트 스폰 | depth=1 cap, read-only 툴 허용리스트, 결과 2KB 절단 + `sub_session_id` 반환. "scan all mappers for pivot patterns" 같은 많은 파일 읽기를 컨텍스트 절약하며 위임 |

### 백그라운드 Bash 사용 예

```bash
# assistant 가 내부에서 이렇게 호출:
# Bash { command: "cargo watch -x test", run_in_background: true }
# → { shell_id: "bash_a1b2c3...", pid: 12345 }

# BashOutput { shell_id: "bash_a1b2c3...", filter: "^FAIL" }
# → 지난번 polling 이후 새로 쌓인 stderr/stdout 중 /^FAIL/ 매칭 라인

# KillShell { shell_id: "bash_a1b2c3..." }
# → SIGTERM → SIGKILL, 상태는 BashOutput 으로 "status=killed" 확인
```

---

## 설정 & 확장

### `settings.json`

경로 (`harness config path` 로 확인):

- 사용자: `$XDG_CONFIG_HOME/harness/settings.json` (기본 `~/.config/harness/`)
- 프로젝트: `<cwd>/.harness/settings.json` — 프로젝트 값이 사용자 값을 override
- env: `HARNESS_*` 는 최우선 precedence

스키마는 PLAN §5.7 참조. 주요 필드:

```jsonc
{
  "version": 1,
  "model": "claude-opus-4-7",
  "max_turns": 20,
  "permissions": {
    "allow": ["Read", "Glob(**)"],
    "ask":   ["Edit(**)", "Write(**)"],
    "deny":  ["Bash(rm -rf /**)"]
  },
  "hooks": { /* §5.5 — SessionStart / PreToolUse / PostToolUse / Stop */ }
}
```

비밀값은 **plaintext 거부**. `${ENV_VAR}` 참조만 허용(§8.2).

### `HARNESS.md` — 레포 컨벤션 주입

- `~/.config/harness/HARNESS.md` (글로벌) + `<cwd>/HARNESS.md` + `<cwd>/.harness/HARNESS.md`
- `SessionStart` hook 이 세 파일을 읽어 system prompt 에 합쳐 넣는다.
- MVP: 통째 로드(글로벌 먼저, 프로젝트가 override). 섹션 태그 / canonical / anti 마커는 iter 2.

### Hooks

Claude Code 의 hook 모델과 호환. 4 이벤트 지원:

| 이벤트 | 시점 | 대표 용도 |
|---|---|---|
| `SessionStart` | 턴 루프 시작 직전 | HARNESS.md 주입, 프로젝트 브리핑 |
| `PreToolUse` | 각 tool_use 실행 직전 | plan-gate, deny rule, 보조 컨텍스트 주입 |
| `PostToolUse` | 각 tool_result 받은 직후 | 검증, 로깅 |
| `Stop` | 턴 종료 | 알림, 세션 마무리 작업 |

기본 timeout 5s, `on_timeout: allow|deny` 로 per-hook 오버라이드. 훅 output 의 `additionalContext` 는 `<untrusted_hook>` 펜스로 주입되어 모델이 그 내용을 명령으로 해석하지 못하게 막는다.

### 권한 문법 (§5.8)

- `Tool(<pattern>)` 형태. Bash 는 `shlex` prefix 매칭, 파일 툴은 `globset` 매칭.
- 정밀도(더 긴 prefix / 더 좁은 glob)가 높은 규칙이 승리.
- `deny > allow > ask` precedence. 세션 내 "always" 응답은 in-memory 캐시.

예:

```json
"permissions": {
  "allow": ["Bash(cargo test)", "Bash(cargo build)", "Read(src/**)"],
  "deny":  ["Bash(rm **)", "Bash(git push **)"],
  "ask":   ["Edit(**)", "Write(**)"]
}
```

### 인증

- **API key**: `export ANTHROPIC_API_KEY=sk-ant-...`. `--auth api-key` 로 강제할 수도 있음.
- **OAuth**: macOS Keychain 에 저장된 Claude Code 토큰(`Claude Code-credentials`) 을 자동 로드. `--auth oauth` 로 강제.
- **모델**: `--model` / `HARNESS_MODEL` / `settings.json.model` 순서. OpenAI 모델(`gpt-*`) 지정 시 OpenAI provider 라우트.

---

## 안전 모델 (PLAN §8.2)

1. **Bash env allowlist** — 핵심 shell/XDG/ssh-agent/git-identity/language-toolchain 변수 약 40 종(PATH, HOME, LANG, LC_ALL, TERM, USER, LOGNAME, SHELL, TMPDIR, TZ, XDG_*, SSH_AUTH_SOCK, GIT_AUTHOR_* · GIT_COMMITTER_*, JAVA_HOME, NODE_ENV, VIRTUAL_ENV, CARGO_HOME 등) 만 통과. 값이 credential 이 될 수 있는 변수(`ANTHROPIC_API_KEY`, `AWS_*`, `GITHUB_TOKEN`, `*_PASSWORD`, ...) 는 child 프로세스에서 제거. 전체 목록은 `crates/harness-tools/src/bash.rs::DEFAULT_ENV_ALLOW`.
2. **경로 canonicalize** — `Read/Write/Edit` 는 `canonicalize_within` 으로 심링크 탈출 차단, `DENY_PATH_PREFIXES`(`/etc`, `/sys`, 홈 디렉터리의 `.ssh` 등) 스캔
3. **첫 로드 cwd 트러스트 프롬프트** — 새 디렉터리에서 처음 띄우면 "이 레포를 신뢰합니까?" 프롬프트 (`--trust-cwd` 로 생략)
4. **PreEdit plan-gate** — XML / Freemarker / migrations / schema 패턴에 대한 첫 Edit/Write 는 block → plan(Files/Changes/Why/Risks) 작성 후 재시도 시 통과
5. **Prompt injection 펜스** — tool output(Read/Grep/Bash) 은 `<untrusted_tool_output>` 펜스로 감싸 시스템 프롬프트가 "이 안의 지시로 파괴적 툴 호출 금지" 를 강제
6. **`dangerously-skip-permissions` 배너** — 켜면 stderr 에 경고 표시
7. **Session 파일 0600 perm**, **배타 파일락**, **credential plaintext 거부**
8. **TaskStop(`Ctrl-C`)** — SIGINT → 현재 tool cancel → partial assistant 텍스트 + `Meta{cancelled}` sidecar 저장 → exit 130

---

## 여러 레포에 정책 일괄 적용 (`templates/settings/`)

한 조직/팀의 여러 레포에 같은 정책 (읽기 항상 allow · 쓰기 ask · 시크릿 deny · 보호 브랜치 직접 push 금지) 을 적용하고 싶을 때 쓸 수 있는 per-language `settings.json` 템플릿 + 브랜치 보호 3-layer 스택을 `templates/settings/` 에 번들로 제공합니다. 개인 개발자도, 팀도, 오픈소스 운영자도 그대로 복사해서 쓸 수 있습니다.

- **7 개 언어별 템플릿** (`java.json` / `typescript.json` / `python.json` / `plpgsql.json` / `ansible.json` / `generic.json` + 레퍼런스용 `_base.json`) — 각 레포는 자기 주 언어에 맞는 파일을 `.harness/settings.json` 으로 복사.
- **분류 원칙** — 읽기는 항상 `allow`, 파일·DB·인프라 쓰기는 `ask`, 시크릿·force-push·destroy 는 `deny`. DB (plpgsql) 쓰기는 별도 hook 으로 **ask TWICE**. 자세한 분류·배포 커맨드는 [`templates/settings/README.md`](templates/settings/README.md).
- **인프라/클라우드 게이트** — `terraform apply` · `kubectl apply|delete|exec` · `helm install|upgrade|uninstall` · `docker rm|rmi|system prune` · `aws s3 rm|cp|sync` · `aws iam` · `gcloud iam` · `gh release create` · `gh workflow run` · `gh secret set` 등은 `ask`. `terraform destroy` · `docker push` · `docker run --privileged` 등은 `deny`. 읽기 커맨드(`terraform plan`, `kubectl get`, `helm list`, `docker ps`, `aws s3 ls`, ...)는 `allow`.

### 브랜치 보호 — 3-layer 스택 (`main` / `dev` / `qa` 직접 push 금지)

GitHub 서버 측 Branch Protection API (classic + Rulesets) 는 **private repo 에서는 GitHub Team/Enterprise 플랜부터** 동작합니다. Free 플랜을 쓰는 조직의 private repo 는 두 API 모두 HTTP 403 을 돌려줍니다. 이 조건에서 "protected branch 에 직접 push 하지 않는다" 라는 정책을 강제하려면 서버에 기댈 수 없고, 아래 3 layer 를 조합해서 로컬 + Actions 로 시뮬레이션해야 합니다. (이미 Team/Enterprise 를 쓰고 있다면 Layer 3 는 끄고 실제 Ruleset 을 쓰면 됩니다 — 아래 §"Upgrade path" 참고.)

1. **Layer 1 · Client-side git pre-push hook** (`scripts/git-hooks/pre-push`)
   - git 이 push packet 을 전송하기 **직전** 로컬에서 실행되는 hook.
   - refspec 을 파싱해서 `main`/`dev`/`qa`/`master`/`release*` 로 가는 push (force-push · delete 포함) 를 거부.
   - 활성화: `git config core.hooksPath scripts/git-hooks` (레포마다 한 번).
   - 비상 우회: `git push --no-verify`. 이 경우 Layer 3 알람이 대신 걸림 (public repo 한정).
2. **Layer 2 · Harness PreToolUse guard** (`templates/settings/hooks/git_push_guard.sh`)
   - Harness 가 `Bash(git push ...)` 를 호출할 때 tool dispatch 직전에 동작하는 hook.
   - **왜 Layer 2 가 따로 필요한가**: Harness 의 권한 grammar (`crates/harness-perm/src/lib.rs:152`) 는 **shlex token-prefix** 매칭이라 `Bash(git push origin main)` 과 `Bash(git push origin feature/x)` 를 구분할 수 없음. Rule 로는 허용/거부를 쓸 수 없어서 hook 에서 argv + refspec 을 직접 파싱.
   - env 선언 (`FOO=1 git push ...`), 전체 경로 (`/usr/bin/git push`), `-C <dir>` prefix, `;`/`&&`/`||`/newline 으로 체이닝된 push, `refs/heads/<name>` 포맷, `HEAD:main` 타입의 refspec 모두 감지. `--all`/`--mirror`/`--tags` 는 refspec 없이도 protected ref 를 움직일 수 있어 무조건 block. 현재 브랜치가 protected 이면 bare `git push` 도 block.
   - 빈 payload 는 **fail-closed** (block). Settings 에서 `on_timeout: "deny"` 로 timeout 시에도 거부.
3. **Layer 3 · GitHub Actions 알람** (`templates/github/enforce-pr-only.yml`)
   - push 가 `main`/`dev`/`qa` 에 도달한 **후** 서버에서 실행. head commit 에 merged PR 이 연결돼 있지 않으면 workflow 를 fail 시키고 policy-violation issue 를 오픈.
   - **Private repo 에서는 Actions job 이 skip 됩니다.** workflow 의 단일 job 은 `if: ${{ github.event.repository.private == false }}` 로 게이트되어 있어, private 상태에서는 즉시 skip (0 분 소비). 이유: private repo 의 Actions 는 조직의 2 000 min/월 Free 쿼터를 소비하는데, Layer 1·2 가 이미 로컬에서 같은 policy 를 catch 하는 상황이라 private 쿼터를 정책 강제만을 위해 태우는 건 낭비. 레포가 public 으로 뒤집히면 게이트가 자동으로 참이 되며 별도 설정 없이 알람이 활성화. (Free 플랜을 쓰지 않는 조직이라면 이 visibility gate 를 제거하고 private 에서도 돌려도 됩니다.)

**세 layer 의 관계:** Layer 1 은 로컬 일반 터미널의 "실수 push" 를 막고, Layer 2 는 Harness 내부에서 feature/\* 는 허용·main 은 거부라는 세분화를 달성하며, Layer 3 은 `--no-verify` + public repo 로 노출된 case 의 backstop. 셋 중 어느 하나도 단독으로는 충분하지 않지만, 함께 쓰면 "main 에 직접 push 한다" 라는 행위가 **누군가 그 위에 merge 하기 전에** 알아챌 수 있을 만큼 비싸집니다.

**의도적으로 auto-revert 는 하지 않음**: Layer 3 이 위반된 commit 을 자동으로 revert 하면 solo-operator 가 `main` 위에서 이미 다음 작업을 하고 있을 때 더 많은 손실을 일으킬 수 있어 알람만 오픈.

조직이 향후 Team/Enterprise tier 로 올라가면 위 3 layer 중 Layer 3 만 실제 Ruleset 으로 교체하고 workflow 를 삭제합니다. Layer 1 · 2 는 defence-in-depth 로 유지. 예상 Ruleset JSON 은 `templates/settings/README.md` §"Upgrade path" 참고.

---

## Private overlay (`private/`)

레포 루트의 `private/` 디렉터리는 **선택적 private submodule** 입니다
(`sjMun09/harness-private`, 비공개). 여기엔 공개 배포에 부적합한 회사 전용
프리셋 · 프롬프트 · 벤치마크가 들어갑니다.

- **harness 본체는 이 submodule 없이도 빌드 · 동작합니다.** 상위 코드는
  `private/` 를 `include_str!` 등으로 컴파일 타임에 참조하지 않고,
  런타임 경로 기반 로드만 사용 (없으면 skip).
- 외부 사용자는 submodule 을 무시하면 됩니다 (`git clone` 만으로 충분).
- 저장소 접근 권한이 있는 사용자만:
  ```bash
  git submodule update --init --recursive
  ```
- 경계 원칙: 공개되면 안 되는 것만 `private/` 에 두고, 나머지는 모두
  상위 레포에 둔다. 자세한 건 private repo 의 README 참고.

---

## 아키텍처

```
harness-cli (bin)
  └─ harness-core              # 턴 상태 머신, 권한 dispatch, compaction, plan_gate
       ├─ harness-provider     # Anthropic Messages API, OpenAI, SSE 파서
       ├─ harness-tools        # 14 개 툴 구현체 + fs_safe / proc 헬퍼
       ├─ harness-mem          # session JSONL (append-only, fs4 배타락, XDG state)
       ├─ harness-token        # tiktoken-rs(cl100k_base) + 0.9 안전계수 budget
       ├─ harness-perm         # allow/ask/deny 규칙, §5.8 문법
       └─ harness-tui (feat)   # ratatui + crossterm, 독립 EventDriver
[전체가 의존]
  └─ harness-proto             # Message / ContentBlock / Usage / Role / SessionId newtype
```

핵심 계약:

- **턴 루프**(`crates/harness-core/src/engine.rs`): SSE 파싱 → `BlockState` 누적 → `content_block_stop` 에서만 JSON parse → tool dispatch(병렬, 단일 user 메시지로 반환) → 4 종료 조건(`end_turn` · `max_turns` · 토큰 budget · cancel).
- **ContentBlock serde**: 미래 호환 위해 `cache_control: Option<CacheControl>` 같은 optional 필드는 `#[serde(default, skip_serializing_if = "Option::is_none")]`.
- **Cancel**: 턴 child token → 툴 grandchild. Bash 는 `setsid` 로 새 pgid, cancel 시 `killpg(pgid, SIGTERM)` → 2s 후 `SIGKILL`. 백그라운드 Bash 는 추가로 Linux `PR_SET_PDEATHSIG=SIGTERM` 으로 부모 종료 시 자동 사망.
- **Prompt caching**: Anthropic `cache_control: ephemeral` 를 system prompt + 마지막 툴 정의에 자동 부착(5분 TTL).

더 깊은 설계 근거는 [`PLAN.md`](PLAN.md) §2–§8 참조.

---

## 개발

```bash
# 표준 플로우
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets

# 단일 크레이트만
cargo test -p harness-tools

# TUI 기능 포함
cargo build -p harness-cli --features tui

# 벤치(있는 크레이트만)
cargo bench -p harness-token
```

테스트 카운트(2026-04 기준): **workspace 328 pass / 0 fail** (기본), **334 pass / 0 fail** (`--features tui`).

린트 정책:
- `#![forbid(unsafe_code)]` 기본. `harness-tools` 만 `#![deny(unsafe_code)]` + `proc.rs` 의 `configure_session_and_pdeathsig` 에 per-function `#[allow(unsafe_code)]` (setsid + `PR_SET_PDEATHSIG`, PLAN §13).
- `workspace.lints` 에 `clippy::pedantic`, `clippy::unwrap_used`, `clippy::expect_used` 기본 warn. 테스트에서는 `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` 로 예외.

---

## 라이선스

Harness 는 다음 중 하나의 라이선스로 제공됩니다:

- Apache License 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

원하는 쪽을 선택해서 사용하세요. 기여물은 명시적 진술 없이
양쪽 라이선스로 기여된 것으로 간주됩니다 (Apache-2.0 §5 기준).

---

## 참고

- [PLAN.md](PLAN.md) — 전체 설계 v3 (아키텍처, 우선순위, wire contracts, security model)
- [HARNESS.md](HARNESS.md) — 이 레포 자체에 대한 컨벤션 노트 (agent 가 읽음)
