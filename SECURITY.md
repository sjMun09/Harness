# Security Policy

## Supported Versions

pre-1.0. 보안 수정은 `main` 에만 적용됩니다. 태그된 릴리즈가 생기면
최신 minor 만 지원합니다.

## 취약점 리포트 (Reporting a Vulnerability)

**공개 이슈에 올리지 마세요.** 대신:

- **우선**: GitHub Security Advisory 로 비공개 리포트
  https://github.com/sjMun09/Harness/security/advisories/new
- 위 방법이 어려우면 레포 오너(`@sjMun09`) 에게 GitHub 메시지 또는
  커밋 author 이메일 (`git log` 로 확인 가능) 로 연락.

리포트에 포함해 주세요:
- 취약점 요약 + 재현 단계
- 영향 범위 (어떤 버전, 어떤 OS, 어떤 피처 조합)
- 가능하면 PoC

응답 시간 목표: 72 시간 내 1 차 확인. 심각도 높은 건은 더 빠르게.

## 범위

**in scope**:
- `harness-perm` 의 allow/ask/deny 문법 우회
- `Bash` 툴의 argv 모드 우회 또는 shell-injection
- `Read`/`Write`/`Edit` 의 경로 canonicalize 우회 (`..`, 심링크,
  `DENY_PATH_PREFIXES` 우회)
- 세션 JSONL 파일의 credential 유출
- `HARNESS_REFUSE_API_KEY=1` 락 우회
- `plan_gate` 우회
- prompt injection 펜스 (`<untrusted_tool_output>`) 우회로 모델이
  권한 없는 툴을 호출하게 유도
- OAuth 키체인 토큰 / API 키의 메모리 노출, 로그 유출

**out of scope**:
- Anthropic / OpenAI / 로컬 LLM 서버 자체의 문제
- Anthropic Max/Pro ToS 해석 이슈 — ToS 책임은 사용자 측
- 모델 출력 품질 (환각, 잘못된 수정) — 이건 모델 문제
- 사용자가 `--dangerously-skip-permissions` 를 의도적으로 켠 경우의
  손상

## 의존성 감사 (known gap)

현재 `cargo audit` / `cargo deny` CI 는 없습니다. 의존성 취약점은
수동으로 관찰하고 있으며, 리포트 받으면 수정 PR 을 엽니다. 이 gap 을
메우는 기여 PR 환영.

## 공개 정책

fix 가 머지되고 사용자에게 업데이트 시간이 주어진 뒤 (보통 7–30 일)
advisory 공개. 리포터 이름은 요청하면 credit, 원하지 않으면 익명 처리.
