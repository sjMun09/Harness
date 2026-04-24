# `HOME` 노출 — Bash 툴의 자식 프로세스

## 위협 모델

`Bash` 툴 (`crates/harness-tools/src/bash.rs`) 은 자식 프로세스에 scrub
된 env 를 전달하지만, `DEFAULT_ENV_ALLOW` 에는 `HOME` 이 포함되어
있다. 그래서 모델이 발급한 `Bash(cat ~/.ssh/id_rsa)` 같은 명령은 실제
사용자 홈 아래의 credential 파일을 읽을 수 있다.

`fs_safe` 는 `HOME_SENSITIVE_PREFIXES` (`.ssh/`, `.aws/`, `.kube/` 등)
로 `Read`/`Write`/`Edit` 경로를 차단하지만, 일단 shell 이 실행되고
나면 POSIX 파일 접근이라 fs_safe 를 거치지 않는다. Bash 툴의 Ask/Allow
gate + `Bash(<shlex-prefix>)` 규칙이 1 차 방어선이고, HOME 노출은 그
방어선이 뚫렸을 때 피해 범위를 결정한다.

## 고려한 옵션

1. **HOME 완전 제거** — git/npm/cargo/gh/pyenv/rustup 등 사실상 모든
   개발 툴체인이 global config 를 읽으려 `~/.gitconfig`, `~/.cargo/`,
   `~/.npmrc` 등에 의존한다. UX 비용 너무 큼. 기각.
2. **항상 sandbox HOME** — 옵션 1 과 같은 breakage. `git commit` 이
   author.email 을 못 읽고, `cargo build` 가 registry index 를 못 찾고,
   `gh auth status` 가 무의미해진다. 기각.
3. **문서화만** — Ask/Allow gate 가 1 차 방어선이라는 논리는 맞지만,
   defense-in-depth 를 전혀 제공하지 않는다. 불충분.
4. **opt-in sandbox HOME** — `BashInput` 에 `sandbox_home: bool` 플래그
   추가. `true` 면 per-call 로 tempdir 을 만들어 `HOME` (+ XDG 디렉터리)
   를 덮어쓴다. 기본값 off 라 기존 UX 유지, 보안 민감 호출에서만 opt-in.

## 선택한 방안: 4 — opt-in sandbox HOME

근거: `allow_shell_mode` (커밋 `ee3bb94`) 와 같은 패턴. 기본 UX 를
깨지 않으면서, 모델이 명시적으로 `sandbox_home: true` 를 선언한 호출은
HOME 관련 env (`HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`,
`XDG_DATA_HOME`, `XDG_STATE_HOME`) 가 빈 tempdir 을 가리키게 된다. 자식
프로세스가 dotfile / credential 을 찾지 못하고 깨끗이 실패한다.

## 범위 제한

- **foreground 만 지원.** `run_in_background: true` 와 `sandbox_home: true`
  는 상충 (tempdir 수명을 bg drainer 와 묶어야 해서 diff 가 커짐) —
  Validation 에러로 거부한다. bg 는 장기 실행 케이스라 sandbox 요구가
  거의 없다.
- env 변수만 덮어쓴다. bind-mount / pivot_root 같은 커널 레벨 격리는
  하지 않는다 — `cat /Users/<me>/.ssh/id_rsa` 처럼 절대경로로 접근하면
  여전히 읽힌다. 이건 Ask gate 에서 막아야 한다.

## 사용법

```json
{
  "command": "npm install",
  "sandbox_home": true
}
```

조합: `sandbox_home` 은 argv / shell 모드 모두에서 동작한다.
