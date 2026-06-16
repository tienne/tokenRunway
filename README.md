# 🛬 Token Runway

AI 코딩 어시스턴트 세션 소진 경보 앱 — **"지금 세션 runway 20% 남았습니다"**

Claude Code 등을 쓰다 갑자기 한도에 걸려 작업이 끊기는 경험을 막는 게 목표.
"얼마나 썼어?"가 아니라 **"얼마나 남았어?"** 에 집중한다.

## 차별점

기존 사용량 모니터링 도구가 percent **스냅샷**(지금 몇 %)만 보여준다면,
Token Runway는 **시계열**을 분석해 소진 속도와 ETA까지 만든다:

> "23% 남음, 이 페이스면 1h 47m 후 소진"

## 기술 스택

- **Tauri 2** (Rust 코어 + React/TS UI) — 경량 메뉴바 상주 앱
- 데이터: Claude Code transcript JSONL (`~/.claude/projects`) 시계열 파싱
- 한도 산정: Keychain OAuth API (예정)

## 아키텍처

```
메뉴바 Tray ──클릭──> Popover UI (React)
                          │ invoke
                          ▼
                  Rust 코어
                  ├ providers/   도구별 UsageProvider trait
                  │   ├ claude_code  ~/.claude JSONL 파서 (P0, 토큰·5h)
                  │   ├ codex        ~/.codex JSONL 파서 (P1, 토큰·5h)
                  │   └ gemini       ~/.gemini 로그 파서 (P2, 요청수·24h)
                  ├ runway       RunwayEngine (5h 윈도우·속도·ETA)
                  └ tray         메뉴바 상주
```

새 도구(Codex, Cursor 등)는 `providers/`에 `UsageProvider` 구현을 추가하고
`lib.rs`의 provider 목록에 등록하면 된다.

## 개발

```bash
pnpm install
pnpm tauri dev      # 개발 실행 (메뉴바 + 창)
pnpm tauri build    # 프로덕션 빌드
```

## 상태

방향성 확정 + scaffold 단계. 자세한 기획은 KB `docs/token-runway.md` 참고.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
