# Harness

Rust 로 작성된 **단일 바이너리 코딩 에이전트**. Claude Code 와 동급의 워크플로(분석 → 판단 → 구현 → 테스트 검증)를 제공하되, 시작 속도 · 파일 I/O · grep/glob · tokenize 를 모두 더 빠르게 하고, **레거시 코드베이스 리팩토링**(XML/SQL/Freemarker/MyBatis) 에 특화된 툴을 1급 기능으로 내장한다.

- **목표 사용 예**: `harness ask "demandPlan_sql.xml 의 pivot 을 프리마커로 수정해줘"` → 레포 컨벤션 자동 참조 → bucket vs Freemarker 판단 → 구현 → 테스트 실행 → 결과 리포트
- **적용 범위(도메인 무관)**: 백엔드(SQL XML · Freemarker · MyBatis · Java · Kotlin · Go) · 프론트(React · Vue · CSS) · LLM 작업(프롬프트 · eval)
- **Non-goal**: Claude Code 의 모든 생태계(skills 마켓플레이스, IDE 확장, 웹 세션) 복제 / 모든 벤더 동등 지원(Anthropic 1급, OpenAI 2급, 나머지 BYO)

전체 설계 계약은 [`PLAN.md`](PLAN.md) 에 v3 기준으로 기재되어 있다. 이 문서는 **지금 어떤 기능이 있고 어떻게 쓰는지** 에 집중한다.

---

## 현재 상태 (2026-04)

- **iter-1 (MVP)**: 완료. workspace scaffold + 6 primitives(token, perm, mem, config, fs_safe, proc) 커밋됨.
- **iter-2**: 5 개 병렬 에이전트 통합 완료(커밋 `feat(iter-2): integrate 5 parallel-agent outputs`).
- **iter-2 후속 통합**: TUI 엔진 브릿지(`--tui`) / `harness-testkit` 추출 / Anthropic SSE 파서 · `--base-url` · E2E HTTP 하니스 모두 머지됨.
- **테스트**: 기본 빌드 **307 pass**, `--features tui` 빌드 **313 pass**, 실패 0.

---

## 설치 & 빌드

```bash
# 필요: Rust stable (1.83+ 권장)
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

# 모델 지정 (기본: claude-opus-4-5 계열)
harness --model claude-sonnet-4-6 ask "이 파일 구조 설명해줘"
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
| **언어 · 런타임** | Node 18+ | Rust 1.82+ (외부 런타임 불필요) |
| **라이선스** | 비공개 | MIT OR Apache-2.0 (`Cargo.toml:9`) |

### 실측 성능 (자리 표시)

> **Note.** 아래 표의 숫자는 실제 벤치를 돌려 채운 값이 아니라 **템플릿**이다. `bench/` 하니스를 사용해 본인의 머신·API 키·네트워크에서 재현한 뒤, 해당 결과를 이 표에 커밋하는 식으로 쓰면 된다. 날조된 숫자는 의도적으로 적지 않는다.

| 지표 | Claude Code | Harness | 비고 |
|---|---|---|---|
| 콜드 스타트 → 첫 토큰 (ms) | TBD | TBD | `harness ask "hi"` vs `claude "hi"` |
| `glob **/*.rs` 1k 파일 스캔 (ms) | TBD | TBD | 단일 tool-call 지연 |
| 10-파일 리팩토링 턴 완주(초) | TBD | TBD | `bench/prompts/refactor.md` |
| 토큰 사용량(input + output) | TBD | TBD | provider usage 로그 |
| 100 턴당 실패율(타임아웃/파싱 오류) | TBD | TBD | 재현 가능한 오류만 집계 |

벤치 수행 방법 및 결과 집계 형식은 [`bench/README.md`](bench/README.md) 를 참고. 실제 수치를 넣으면 바로 이 표 자리로 옮기면 된다.

---

## CLI 레퍼런스

### 전역 옵션

| 플래그 | 설명 |
|---|---|
| `--model <NAME>` | 사용할 모델. `HARNESS_MODEL` env 또는 `settings.json` 의 `model` 과 precedence 동일. |
| `--verbose / -v` | DEBUG tracing 활성화. stderr 에 `[warn]` 배너 표시. |
| `--dangerously-skip-permissions` | 모든 Ask 권한 요청을 자동 Allow 처리. CI 에서 의도적으로 쓸 때만 사용. |
| `--auth auto|api-key|oauth` | 자격증명 선택. `auto` 는 `ANTHROPIC_API_KEY` → Claude Code 키체인 순서로 폴백. |
| `--trust-cwd` | 첫 로드 cwd 트러스트 프롬프트(§8.2) 생략. 비대화식 환경 전용. |

### 서브커맨드

#### `ask` — 한 번의 턴 루프 실행

```bash
harness ask "<prompt>" [--max-turns 20]
```

- `prompt` 를 user 메시지로 모델에 전달
- assistant 가 툴을 호출하면 실행 → 결과를 다시 넣음 → `end_turn` 또는 `max_turns` 까지 반복
- `Ctrl-C` 한 번: 현재 진행 중 tool 취소 + 마지막까지 받은 partial assistant 텍스트 보존 후 exit code 130 반환
- `Ctrl-C` 두 번: 쉘 SIGINT 로 즉시 종료

세션은 자동으로 `$XDG_STATE_HOME/harness/sessions/<id>.jsonl` 에 저장된다(다음 섹션 `session` 참조).

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
| **Bash** | 명령 실행 | **argv 모드 기본**(shell injection 차단), `mode: "shell"` 명시 opt-in, `setsid` + 타임아웃 120s/600s, env allowlist(`PATH/HOME/LANG/TERM/USER` 만), stdout+stderr head 4KB + tail 4KB + `/tmp/harness-bash-*.log` 전체 로그 경로 |
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
  "model": "claude-opus-4-5",
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
- **OAuth**: macOS Keychain 에 저장된 Claude Code 토큰(`com.anthropic.claude-code.*`) 을 자동 로드. `--auth oauth` 로 강제.
- **모델**: `--model` / `HARNESS_MODEL` / `settings.json.model` 순서. OpenAI 모델(`gpt-*`) 지정 시 OpenAI provider 라우트.

---

## 안전 모델 (PLAN §8.2)

1. **Bash env allowlist** — `PATH/HOME/LANG/TERM/USER` 외 모든 환경 변수(`ANTHROPIC_API_KEY`, `AWS_*`, `GITHUB_TOKEN`, ...) 를 child 프로세스에서 제거
2. **경로 canonicalize** — `Read/Write/Edit` 는 `canonicalize_within` 으로 심링크 탈출 차단, `DENY_PATH_PREFIXES`(`/etc`, `/sys`, 홈 디렉터리의 `.ssh` 등) 스캔
3. **첫 로드 cwd 트러스트 프롬프트** — 새 디렉터리에서 처음 띄우면 "이 레포를 신뢰합니까?" 프롬프트 (`--trust-cwd` 로 생략)
4. **PreEdit plan-gate** — XML / Freemarker / migrations / schema 패턴에 대한 첫 Edit/Write 는 block → plan(Files/Changes/Why/Risks) 작성 후 재시도 시 통과
5. **Prompt injection 펜스** — tool output(Read/Grep/Bash) 은 `<untrusted_tool_output>` 펜스로 감싸 시스템 프롬프트가 "이 안의 지시로 파괴적 툴 호출 금지" 를 강제
6. **`dangerously-skip-permissions` 배너** — 켜면 stderr 에 경고 표시
7. **Session 파일 0600 perm**, **배타 파일락**, **credential plaintext 거부**
8. **TaskStop(`Ctrl-C`)** — SIGINT → 현재 tool cancel → partial assistant 텍스트 + `Meta{cancelled}` sidecar 저장 → exit 130

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

테스트 카운트(2026-04 기준): **workspace 298 pass / 0 fail**.

린트 정책:
- `#![forbid(unsafe_code)]` 기본. `harness-tools` 만 `#![deny(unsafe_code)]` + `proc.rs` 의 `configure_session_and_pdeathsig` 에 per-function `#[allow(unsafe_code)]` (setsid + `PR_SET_PDEATHSIG`, PLAN §13).
- `workspace.lints` 에 `clippy::pedantic`, `clippy::unwrap_used`, `clippy::expect_used` 기본 warn. 테스트에서는 `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]` 로 예외.

---

## 라이선스

TBD (현재 private; 공개 전까지 이전 코드와 동일 범위의 라이선스를 `LICENSE` 로 추가 예정).

---

## 참고

- [PLAN.md](PLAN.md) — 전체 설계 v3 (아키텍처, 우선순위, wire contracts, security model)
- [HARNESS.md](HARNESS.md) — 이 레포 자체에 대한 컨벤션 노트 (agent 가 읽음)
