# Harness 에 기여하기

PR / 이슈 환영. 작게, 구체적으로, 테스트와 함께.

## 빌드

```bash
# Rust 1.90+ (rust-toolchain.toml 기준)
git clone https://github.com/sjMun09/Harness.git
cd Harness
cargo build --workspace
```

옵션 피처:
- `--features tui` — ratatui 기반 TUI
- `--features claude-code-oauth` — Claude Code 의 macOS 키체인 OAuth 토큰
  재사용 경로. ToS 고려사항은 README §"OAuth 재사용 — TOS 주의" 참고.
  기본 off.

## 테스트 · 린트

PR 전 아래 네 개를 통과시킵니다. CI 에서도 같은 걸 검사합니다
(`.github/workflows/ci.yml`).

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

피처 조합 변경이 있으면 해당 피처로도 한 번 더 돌립니다.

## 커밋 메시지

Conventional Commits 를 씁니다 (`feat:`, `fix:`, `chore:`, `docs:`,
`refactor:`, `test:`, `style:`, `perf:`). 본문은 *왜* 를 적고, *무엇* 은
diff 가 말하게 둡니다.

예:
```
fix(provider): retry SSE reconnect on transient 502

prod 에서 Anthropic 측 게이트웨이 502 가 짧게 튀는 경우가 있어
턴 중간에 에이전트가 죽는 현상 관찰. 1 회 재연결 시도 추가.
```

## PR 프로세스

1. `main` 에서 브랜치 분기 — 이름 예: `feat/your-feature`, `fix/bug-xyz`.
2. 변경 + 테스트 + 문서 업데이트 (해당된다면).
3. PR 오픈 — 템플릿 (`.github/PULL_REQUEST_TEMPLATE.md`) 이 체크리스트를
   제공합니다.
4. CI 녹색 + 리뷰 1 명 이상 → 머지.

## 디자인 문서

- [`PLAN.md`](PLAN.md) — 아키텍처 · wire contracts · security model (v3).
- [`HARNESS.md`](HARNESS.md) — 이 레포의 컨벤션 노트 (에이전트가 읽음).
- [`bench/README.md`](bench/README.md) — 성능 측정 방법론.

큰 구조 변경 (public API, security model, provider contract) 을 제안할 때는
코드 전에 이슈로 논의하는 걸 선호합니다.

## 라이선스

기여물은 명시적 진술 없이 MIT OR Apache-2.0 으로 기여된 것으로 간주됩니다
(Apache-2.0 §5). 레포 루트의 `LICENSE-MIT` / `LICENSE-APACHE` 참조.

## 보안 이슈

보안 취약점은 공개 이슈가 아닌 GitHub Security Advisories 로 비공개 리포트
하세요. 자세한 건 [`SECURITY.md`](SECURITY.md).
