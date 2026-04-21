# Harness ↔ Claude Code 성능 벤치마크

목적: **날조된 숫자 없이** Harness 와 Claude Code 를 동일 조건에서 돌려 비교할 수 있는 재현 가능한 측정 하니스를 제공한다.

결과를 채우기 전까지 루트 `README.md` 의 "실측 성능" 표는 `TBD` 로 비어 있다. 본 하니스를 돌린 뒤 나온 값만 그 표로 옮긴다.

---

## 전제 조건

- 두 바이너리가 모두 `$PATH` 에 있어야 한다.
  - `harness` — 이 레포에서 `cargo install --path crates/harness-cli --force`
  - `claude` — Claude Code CLI (공식 설치 문서 참고)
- 동일 크레덴셜: `ANTHROPIC_API_KEY` 를 export 하거나 두 도구 모두 동일한 OAuth 계정으로 로그인.
- `jq`, `hyperfine`(옵션), GNU `time`(macOS 는 `gtime`) 이 있으면 더 정밀한 측정이 가능.

---

## 디렉토리 구조

```
bench/
├── README.md            # 이 문서
├── run.sh               # 벤치 실행 드라이버
├── harness.sh           # Harness 한 번 실행 + 타임/토큰 로깅
├── claude.sh            # Claude Code 한 번 실행 + 타임/토큰 로깅
├── prompts/             # 입력 프롬프트 픽스처 (재현성용)
│   ├── cold_start.txt
│   ├── glob_scan.txt
│   ├── signature_extract.txt
│   └── grep_summary.txt
└── results/             # 실행 로그 + 집계 CSV/MD 가 쌓이는 곳
    └── .gitkeep
```

---

## 실행 방법

```bash
# 모든 프롬프트 × 두 도구를 3 회씩 실행
cd bench/
./run.sh --trials 3

# 특정 프롬프트만
./run.sh --prompt glob_scan --trials 5

# Harness 만
./run.sh --tool harness --trials 3
```

`results/<timestamp>.csv` 와 `results/<timestamp>.md` 가 쓰여진다. CSV 는 각 trial 의 raw 측정값(wall_ms, exit_code, stderr_lines, token_in, token_out), MD 는 prompt×tool 평균/중앙값/표준편차 표.

---

## 측정 지표

| 필드 | 의미 | 수집 방식 |
|---|---|---|
| `wall_ms` | 턴 전체 벽시계 시간 (초기화 포함) | `gdate +%s%3N` 이전 - 이후 |
| `first_byte_ms` | 첫 stdout/stderr 바이트까지 | `stdbuf` + 라인 모니터 |
| `exit_code` | 0=정상, 130=SIGINT, 기타=실패 | `$?` |
| `stderr_lines` | 툴 호출 카운트 근사 | `⏺ ` 프리픽스 라인 수 |
| `tokens_in`, `tokens_out` | usage 블록 | stderr `[usage]` 파싱 (둘 다 그런 로깅 있음) |
| `assistant_bytes` | 최종 답변 길이 | stdout wc -c |

---

## 결과 형식 (예)

`results/<timestamp>.md` 에 아래 형식으로 자동 생성된다.

```markdown
# Bench run 2026-04-20T14:03:11+09:00

- 머신: macOS 24.3.0 / M2 Pro / 16GB
- 네트워크: 유선, api.anthropic.com p50 RTT 38ms
- 모델: claude-opus-4-7 (양쪽 동일)
- Trials per cell: 3

## cold_start.txt — "hi 라고만 답해"

| 도구 | wall_ms (median) | first_byte_ms | exit | tokens_in | tokens_out |
|---|---|---|---|---|---|
| harness | 612 | 284 | 0 | 48 | 3 |
| claude  | 1980 | 912 | 0 | 51 | 3 |
```

이 표를 그대로 루트 `README.md` 의 "실측 성능" 섹션으로 옮기면 된다.

---

## 주의

- **숫자를 손으로 적지 마라.** 재현 불가능한 수치는 날조와 동일한 취급.
- 모델·네트워크·시간대가 다르면 다른 파일로 커밋해서 조건을 비교 가능하게 남겨라.
- 단일 trial 은 의미 없음 — 최소 3, 분산이 크면 10 회 이상.
- 프롬프트가 네트워크에 나가므로 `prompts/` 에 **기밀 코드를 절대 넣지 말 것**.
