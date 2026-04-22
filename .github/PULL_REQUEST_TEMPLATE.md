## Summary

<!-- 이 PR 이 *왜* 필요한지 한 두 문단. *무엇* 을 바꿨는지는 diff 가 말함. -->

## 관련 이슈 / 링크

Closes #<!-- issue number -->

## 변경 유형

- [ ] Bug fix
- [ ] New feature
- [ ] Breaking change (사용자 또는 API 에 영향)
- [ ] Refactor (동작 변경 없음)
- [ ] Docs / test / chore only

## Test plan

PR 올리기 전 통과시켜 주세요:

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] 필요하면 피처 조합 검증:
  - [ ] `cargo test --workspace --features tui`
  - [ ] `cargo test --workspace --features claude-code-oauth`

추가로 수동으로 검증한 시나리오가 있으면 여기 적어 주세요.

## Breaking changes

<!-- 사용자 워크플로를 깨거나 public API 를 바꾸는 경우에만. 없으면 "N/A". -->

## 추가 노트
