# Changelog

이 프로젝트의 모든 주요 변경 사항은 이 파일에 기록됩니다.

본 문서의 형식은 [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/)을 따르며,
버전 체계는 [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)을 준수합니다.

## [Unreleased]

### Added

- README 설치 섹션에 `cargo install --git` / 릴리스 tarball / 로컬 빌드 3종 경로 명시.

### Changed

- **배포 방식 결정**: crates.io publish 는 하지 않음. 이유: `harness` / `harness-cli` 이름이 이미 기존 벤치마킹 크레이트 (wenyuzhao/harness) 에 점유돼 전환 불가. 대신 GitHub Releases tarball + `cargo install --git` 경로 공식 지원.

## [0.1.0] - 2026-04-22

첫 퍼블릭 릴리스. 9-크레이트 Rust 워크스페이스 기반의 single-shot 품질 지향 에이전트 하니스를 OSS로 공개합니다.

### Added

- **OAuth 피처 게이트**: Claude Code OAuth 재사용 경로를 옵트인 피처 플래그(`claude-code-oauth`, 기본 비활성)로 분리하여 기본 빌드는 API 키 경로만 노출.
- **OAuth-first Auto 라우팅**: `HARNESS_REFUSE_API_KEY` 빌링 락 환경 변수와 함께 Auto 모드에서 OAuth를 우선 시도하도록 순서 재정렬.
- **OAuth provider 전송 경로**: Anthropic beta 헤더 + system prefix를 자동 주입하여 OAuth 모드에서 정상 호출 가능.
- **로컬 LLM 1급 지원**: OpenAI 호환 엔드포인트를 통한 Ollama / vLLM / LM Studio / llama.cpp / MLX 런타임 연동 및 키 해석 테스트.
- **레거시 백엔드 친화 템플릿**: 언어별 `settings.json` 프리셋(Java, Python, TypeScript, plpgsql, generic)과 DB write 이중 확인 훅 제공.
- **`.harnessignore` 지원**: 내장 glob/grep 워커가 워크스페이스 ignore 규칙을 존중.
- **bash 환경 allowlist 확장**: 다중 언어 툴체인(Maven, Gradle, npm 등)을 인식하도록 환경 변수 통과 허용 범위 확장.
- **TUI 모드**: `--tui` 플래그로 ratatui 기반 인터랙티브 화면을 `TuiEngineDriver` 위에서 실행.
- **벤치 하니스**: Harness ↔ Claude Code 비교 측정용 실행 가능 벤치마크와 JSON 스키마 러너, README 계약 명세.
- **`--metrics-json` 라이터**: ask 턴 루프의 토큰/지연 시간/툴 호출 메트릭을 JSON 파일로 출력.
- **stdin 프롬프트**: `ask` 와 세션 resume 경로에서 표준 입력으로 프롬프트 주입 지원.
- **Anthropic SSE 스트리밍**: provider 레이어에 streaming 응답 처리와 e2e 테스트, 숨김 `--base-url` 플래그 추가.
- **`harness-testkit` 크레이트**: `MockProvider` 를 별도 크레이트로 분리하여 다운스트림 테스트 재사용.
- **OSS 거버넌스 문서**: CODE_OF_CONDUCT, CONTRIBUTING, SECURITY 등 공개 리포지토리 운영을 위한 기본 문서 정비.
- **3-Layer 브랜치 보호**: 템플릿에 commit-msg 패스트패스 + Actions 알람 + 인프라/클라우드 게이트로 구성된 PR-only 강제 스택 추가.

### Changed

- **OAuth 게이트 도입에 따른 테스트 카운트 갱신** 및 기본 피처 셋 정리.
- **벤치 fixture 명명 정리**: refactor 픽스처를 `signature_extract` 로 리네임하여 의도를 명시.
- **CI 그린화**: pedantic / `unwrap_used` / `expect_used` 린트를 데모트하여 fmt + clippy 를 통과 가능한 기준선으로 조정.
- **템플릿에서 조직 고유 명칭 제거**: OSS 공개에 앞서 사내 식별자를 제너릭 표현으로 치환.
- **README 개편**: Rust 채택 근거 표, PATH/OAuth/daemon 안내, CI/라이선스 배지, OAuth 폴백 피처 게이트 설명 추가.

### Fixed

- **권한 평가 순서**: `deny` 규칙을 `ask_cache` 보다 먼저 평가하여, 캐시된 allow 가 사후 deny 로 취소되도록 수정.
- **PR-only 강제 race 완화**: GitHub API 인덱싱 지연을 흡수하기 위한 merge-PR 조회 재시도 및 commit-msg 패스트패스 하이브리드 도입.
- **벤치 `harness.sh` 실행 경로** 및 README 의 CLI identity 표기 수정.

### Security

- **API 키 빌링 락**: `HARNESS_REFUSE_API_KEY=1` 설정 시 API 키 경로 사용을 거부하여, OAuth 만 사용해야 하는 환경에서 우발적 과금 경로를 차단.
- **OAuth 와이어 세부정보 README 에서 redact**: 토큰/헤더 형식 등 민감 디테일을 공개 문서에서 제거.

[Unreleased]: https://github.com/sjMun09/Harness/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sjMun09/Harness/releases/tag/v0.1.0
