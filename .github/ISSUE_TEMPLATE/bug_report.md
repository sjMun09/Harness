---
name: Bug report
about: 재현 가능한 버그 리포트
title: "[bug] "
labels: bug
assignees: ''
---

## 설명

<!-- 무엇이 어떻게 안 되는지 한 문장 -->

## 재현 단계

```bash
# 어떤 명령을 쳤는지
harness ...
```

## 기대 vs 실제

**기대**:
**실제**:

## 환경

- harness 버전: <!-- `harness --version` 또는 커밋 SHA -->
- OS / arch: <!-- 예: macOS 15.2 / arm64, Ubuntu 24.04 / x86_64 -->
- Rust: <!-- `cargo --version` -->
- 빌드 피처: <!-- 예: default / --features tui / --features claude-code-oauth -->
- 모델: <!-- 예: claude-opus-4-7, openai/qwen2.5-coder:14b -->

## 세션 로그 (선택)

세션 JSONL 을 첨부하면 재현이 쉽습니다 (`$XDG_STATE_HOME/harness/sessions/`).

- [ ] 첨부 전 API 키·OAuth 토큰·개인 식별 정보를 **redact** 했습니다.

## 추가 컨텍스트
