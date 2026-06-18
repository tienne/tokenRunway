# 🛬 Token Runway

AI 코딩 어시스턴트 세션 소진 경보 앱 — **"지금 세션 runway 20% 남았습니다"**

Claude Code·Codex·Gemini를 쓰다 갑자기 한도에 걸려 작업이 끊기는 경험을 막는 게 목표.
"얼마나 썼어?"가 아니라 **"얼마나 남았어?"** 에 집중한다.

## 차별점

기존 사용량 모니터링 도구가 percent **스냅샷**(지금 몇 %)만 보여준다면,
Token Runway는 **시계열**을 분석해 소진 속도와 ETA까지 만든다:

> **Claude Code · Max 5x** — 23% 남음, 이 페이스면 1h 47m 후 소진

## 기능

- **멀티툴 잔여율** — Claude Code / Codex / Gemini를 한 화면에
- **플랜 자동 인식** — 설정 없이 Keychain·로그에서 구독 등급 배지 (Max 5x, Plus 등)
- **소진 속도 · ETA** — 최근 사용 추세로 "언제 끊길지" 예측
- **리셋 시각 · 주간 잔여율** — 5시간/주간 윈도우별
- **임계치 경보** — 잔여 20% 이하 도달 시 네이티브 알림 (창 닫혀도 동작)
- **메뉴바 상주** — 경량 트레이 앱, 자동 로그인 불필요

## 지원 도구

| 도구 | 잔여율 출처 | 단위 | 윈도우 | 신뢰도 |
|------|-----------|------|--------|--------|
| **Claude Code** | Keychain OAuth → `api/oauth/usage` | 토큰 | 5h + 주간 | 정확 (공식) |
| **Codex** | JSONL `rate_limits` (로컬) | 토큰 | 5h + 주간 | 정확 (공식) |
| **Gemini** | 로그 요청 수 ÷ 1000 (자정 리셋) | 요청 | 일간 | **추정** (무료티어 가정) |

> 핵심: Claude Code·Codex는 공식 사용률을 직접 받고, Gemini는 로컬에 사용량 데이터가
> 없어 AgentBar 방식(일일 1000 요청 가정)으로 추정한다. 추정 카드는 note로 구분 표시.

## 기술 스택

- **Tauri 2** (Rust 코어 + React 19 / TypeScript UI) — 경량 메뉴바 상주 앱
- HTTP: `ureq` (OAuth 사용량 조회), 시계열: `chrono`
- 알림: `tauri-plugin-notification`

## 아키텍처

```
메뉴바 Tray ──클릭──> Popover UI (React)
   │ "🛬 23%"            │ invoke get_runway (30s 폴링)
   │                     ▼
   │             ┌──────────────────────────────────────┐
   │             │  Rust 코어                             │
   └─ 백그라운드  │  ├ providers/   UsageProvider trait    │
      경보 루프 ──│  │   ├ claude_code  OAuth + JSONL      │
      (60s)      │  │   ├ codex        JSONL rate_limits  │
                 │  │   └ gemini       로그 요청 수        │
                 │  ├ runway       RunwayEngine (속도·ETA)│
                 │  └ AlertManager 임계치 → 네이티브 알림  │
                 └──────────────────────────────────────┘
```

새 도구(Cursor, Copilot 등)는 `providers/`에 `UsageProvider` 구현을 추가하고
`lib.rs`의 provider 목록에 등록하면 된다. 자세한 규약은 [`CLAUDE.md`](./CLAUDE.md) 참고.

## 개발

```bash
pnpm install
pnpm tauri dev      # 개발 실행 (메뉴바 + 창)
pnpm tauri build    # 프로덕션 빌드

# 검증
pnpm exec tsc --noEmit          # TypeScript 타입 체크
cd src-tauri && cargo check     # Rust 컴파일 체크
```

Node 버전은 `.nvmrc`(22.21.1)에 고정 — `nvm use`로 전환.

## 아이콘

`design/app-icon.svg`(컬러)·`design/tray-icon.svg`(단색 트레이)가 소스.
수정 후 `pnpm tauri icon design/app-icon.svg`로 전체 세트 재생성.

## 릴리스

`v*` 태그를 푸시하면 GitHub Actions가 macOS universal `.dmg`를 빌드해 **Release 초안**을 만들어요:

```bash
# 버전 태그 → 자동 빌드·릴리스
git tag v0.1.0
git push origin v0.1.0
# → Actions가 빌드 후 Release(draft) 생성 → 검토 후 publish
```

> 현재는 **미서명** 빌드라 받는 사람이 Gatekeeper 경고를 봐요. Apple Developer 가입 후
> 서명·공증 secret(`APPLE_*`)을 등록하면 `.github/workflows/release.yml`에서 활성화됩니다.

## 상태

MVP 핵심 기능 구현 완료 (멀티툴 잔여율·플랜 배지·ETA·임계치 경보).
남은 작업은 [`CLAUDE.md`](./CLAUDE.md) → 로드맵 참고.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
