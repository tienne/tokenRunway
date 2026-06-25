# CLAUDE.md — Token Runway 개발 가이드

AI 코딩 어시스턴트(Claude Code/Codex/Gemini)의 **세션 토큰 잔여량**을 모니터링하고
임계치 도달 시 경보하는 macOS 메뉴바 앱. Tauri 2 (Rust 코어 + React/TS UI).

핵심 철학: **"얼마나 남았어?"** — OAuth/로컬 데이터로 공식 잔여율을 받고(분모),
JSONL 시계열로 소진 속도·ETA를 더한다(차별점). 사용자 로그인/설정 불필요.

## 디렉토리 구조

```
src-tauri/src/
├ lib.rs            앱 진입점: provider 등록, get_runway/get_stats command, tray, AlertManager
├ runway.rs         RunwayEngine — 샘플/공식사용률 → RunwayStatus 계산
├ rollup.rs         일별 사용량 롤업 영속화(rollup.json) — 30일+ 히스토리용
└ providers/
   ├ mod.rs         UsageProvider trait + 공용 타입 + find_recent_jsonl 헬퍼
   ├ claude_code.rs Keychain OAuth + ~/.claude JSONL
   ├ codex.rs       ~/.codex JSONL (token_count + rate_limits)
   ├ gemini.rs      ~/.gemini 로그 요청 수
   └ antigravity.rs ~/.gemini/antigravity-cli 전송 로그
src/                React UI (App.tsx 대시보드)
design/             아이콘 SVG 소스
```

## 데이터 흐름

1. UI가 30초마다 `get_runway` invoke → `compute_all()` → 각 provider별 `runway::compute`
2. `runway::compute`는 `collect_samples`(시계열) + `official_usage`(공식 사용률)를 조합
3. 백그라운드 스레드(60초)가 `compute_all` + `check_alerts`로 임계치 경보 (창 닫혀도 동작)

## UsageProvider trait 규약 (`providers/mod.rs`)

```rust
trait UsageProvider {
    fn tool_name(&self) -> &'static str;        // 표시명
    fn available(&self) -> bool;                // 데이터 소스 존재 여부
    fn collect_samples(&self, since_ms) -> Vec<UsageSample>;  // 시계열 (속도·ETA용)
    fn unit(&self) -> &'static str { "tokens" }      // "tokens" | "requests"
    fn window_secs(&self) -> i64 { 5*3600 }          // 누적 윈도우
    fn official_usage(&self) -> Option<OfficialUsage> { None }  // 공식 잔여율(우선)
}
```

- **`official_usage`가 있으면** percent는 그 값(`100 - utilization`)을 쓰고, `window_usage`/
  공식 utilization으로 implied limit을 역산해 ETA를 구한다. (Anthropic은 절대 토큰 한도를
  공개하지 않으므로 역산이 유일한 방법)
- **없으면** `default_limit`(현재 전부 None) 기반 — percent/ETA 미계산.

## 새 provider 추가 (4단계)

1. `providers/<tool>.rs` 생성, `UsageProvider` 구현
2. `providers/mod.rs`에 `pub mod <tool>;`
3. `lib.rs`의 `providers()` vec에 `Box::new(<Tool>Provider::new())` 추가
4. JSONL 디렉토리면 `find_recent_jsonl(root, since_ms)` 헬퍼 재사용 (mtime 필터 내장)

## 도구별 데이터 소스 상세

### Claude Code (`claude_code.rs`) — 정확
- **시계열**: `~/.claude/projects/<프로젝트>/<세션>.jsonl`, assistant 메시지의
  `message.usage` (input+output+cache 합산), `timestamp`
- **공식 잔여율**: Keychain `Claude Code-credentials`(macOS `security` CLI) →
  `claudeAiOauth.accessToken`으로 `GET https://api.anthropic.com/api/oauth/usage` 호출
  - **필수 헤더**: `Authorization: Bearer`, `anthropic-beta: oauth-2025-04-20`,
    `User-Agent: claude-code/<version>` ← **없으면 즉시 영구 429**
  - **180초 캐싱 필수** — 이 엔드포인트는 공격적 rate limit. `USAGE_CACHE` 전역.
  - 응답: `five_hour.utilization`, `seven_day.utilization`, `resets_at`
- **플랜**: 같은 credentials의 `rateLimitTier`(`default_claude_max_5x` → "Max 5x")

### Codex (`codex.rs`) — 정확, 더 쉬움
- **시계열**: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, `event_msg`/`token_count`의
  `info.last_token_usage.total_tokens`
- **공식 잔여율**: 같은 JSONL의 `payload.rate_limits` (네트워크 불필요!)
  - `primary`(5h, used_percent/resets_at), `secondary`(주간), `plan_type`("plus")
  - `resets_at`은 epoch seconds → RFC3339 변환
  - 최근 24h 파일에서 가장 신선한 rate_limits 선택

### Gemini (`gemini.rs`) — 추정
- **시계열**: `~/.gemini/tmp/<프로젝트>/logs.json`(JSON 배열), `type=="user"` 메시지 카운트
- **공식 잔여율 없음** — 로컬에 토큰/한도 데이터 미기록. AgentBar 방식 차용:
  오늘 자정(로컬) 이후 요청 수 ÷ `DAILY_REQUEST_LIMIT`(1000) = 추정 사용률
  - 단위 `requests`, 윈도우 일간(24h), 추정이므로 note로 명시
  - 플랜 배지 없음 (무료티어 가정)

### Antigravity (`antigravity.rs`) — 로컬 요청 추세
- **시계열**: `~/.gemini/antigravity-cli/log/cli-*.log`의
  `Sending user message to conversation` 로그 카운트
- 단위 `requests`, 윈도우 일간(24h)
- **잔여율 없음** — 절대 quota/credit 한도가 로컬 파일이나 공개 API로
  제공되지 않아 임의 추정하지 않음

### 향후 provider (데이터 소스 스펙 — 미구현)

이 개발 환경엔 데이터/계정이 없어 검증 불가. 실제 사용 환경에서 구현할 것.
스펙은 AgentBar 소스(`scari/AgentBar`)에서 확보.

- **Cursor** — SQLite + REST, 월간 요청
  - DB: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`에서 인증 추출
  - API: `https://www.cursor.com/api/usage` → `modelUsages[].maxRequestUsage`
  - Rust: `rusqlite`(SQLite) + JWT 디코드 + `ureq` 필요. 한도는 플랜별 월간 추정.
- **Copilot** — gh CLI/Keychain 토큰 + GitHub API, 월간 requests
  - 토큰: `gh auth token` 우선, 폴백 Keychain(copilot account)
  - `GET https://api.github.com/copilot_internal/user` → 권한/플랜(`copilot_plan`,
    `access_type_sku`). **사용량(used/limit)은 이 엔드포인트에 없음** — 별도 조사 필요.
  - 주의: 계정에 Copilot 권한 없으면 `access_type_sku: "no_access"`.

## 핵심 모듈

- **RunwayEngine** (`runway.rs`): `BURN_WINDOW_MIN`(15분) 기울기로 소진 속도(단위/분),
  공식 사용률 우선 → percent/ETA/리셋/주간/플랜 채움
- **AlertManager** (`lib.rs`): 세 가지 경보, 각 도구별 1회 발사 + 회복 시 재무장.
  - 소진 경보 — 잔여율 ≤ 임계치
  - 예상 소진 경보 — ETA ≤ `eta_alert_minutes`
  - 리셋 임박 경보 — 리셋까지 ≤ `reset_alert_minutes` **이고** 잔여 > 임계치
    (곧 사라질 토큰이 많을 때 "지금 활용" 알림 — 소진 경보의 반대 방향)
- **트레이 위험 표시** (`update_tray_title`): 잔여 ≤ 임계치 시 빨간 아이콘으로 교체
  (`TRAY_DANGER` AtomicBool로 상태 변화 시에만 교체)

## 빌드 / 검증

```bash
pnpm exec tsc --noEmit          # TS 타입 체크
cd src-tauri && cargo check     # Rust 컴파일
pnpm tauri dev                  # 실제 실행 (메뉴바 + 알림 권한 다이얼로그)
```

## Analytics (PostHog, opt-in)

- `analytics.rs` — opt-in(`settings.analytics_enabled`)일 때만 PostHog capture로 전송
- 키는 빌드 시 `POSTHOG_KEY` env로 주입 (레포 미포함, 없으면 완전 비활성)
- **절대 전송 금지**: 토큰 수치·잔여율 값·프로젝트명·메시지 내용. 행동 메타만
- 이벤트: `app_launched{version,tool_count}`, `alert_fired{kind}`,
  `settings_opened`, `history_opened`. 익명 `anon_id`로 구분
- 새 이벤트 추가 시 properties를 화이트리스트로 엄격히 (값 유출 주의)

## 주의사항 (회귀 방지)

- **익명 통계는 opt-in·내용 무전송** — properties에 토큰/잔여율 값 절대 금지
- **Claude Code transcript는 message.id로 dedup 필수** — 세션 재개·worktree·압축으로
  같은 메시지가 여러 번 기록됨. dedup 안 하면 토큰·메시지가 ~2배 부풀려짐
  (`collect_samples`의 `seen` HashSet). 다른 provider도 중복 의심되면 동일 적용.
- **OAuth 폴링은 180초 미만 금지** — User-Agent 누락/과빈도 폴링 시 영구 429
- 토큰 등 시크릿은 로그/커밋에 절대 노출 금지 (Keychain 직접 읽기)
- `official_usage` 우선 — 로컬 토큰 합산보다 공식 사용률이 정확
- 새 provider의 `window_secs`/`unit`이 다르면 UI는 자동 대응 (라벨 동적)

## 커밋 컨벤션

`type(scope): subject` — 예: `feat(usage): ...`, `fix(ui): ...`, `feat(alert): ...`

## 로드맵

- [x] 트레이 타이틀에 실시간 % 표시 — 최저 잔여율 도구 (`update_tray_title`, 60s 갱신)
- [x] 임계치 설정 UI — `settings.rs`(파일 영속화) + ⚙️ 슬라이더
- [x] 폴링 주기 / 도구 on·off 설정 — `poll_seconds`·`disabled_tools`
- [x] ETA 기반 경보 — `eta_alert_minutes` (잔여율 OR ETA 도달 시 알림)
- [x] 다국어 (한국어/English) — 프론트 `src/i18n.ts`, Rust `i18n.rs`(메뉴·알림),
  설정에서 언어 선택(시스템 자동 감지 기본). note는 i18n 키로 전달해 프론트 번역
- [x] 시작 시 자동 실행 — `tauri-plugin-autostart`(LaunchAgent) + 설정 토글
- [x] 방해금지 시간대 — 지정 시간(자정 넘김 처리)엔 경보 무음 (`is_quiet_now`)
- [x] 추세 스파크라인 — 윈도우 12구간 사용량 미니 막대 (`sparkline`, RunwayEngine)
- [x] 견고성/에러 가시성 — OAuth 실패 사유 표시(토큰만료/rate limit/미로그인,
  `status_note` i18n 키), 마지막 갱신 시각("방금/N분 전")
- [x] 히스토리 — 통계 전용 창(`get_stats`, HistoryView, 트레이 메뉴·📅). 기간 토글
  7/30/90/전체 + 요약 카드·모델별 분해·시간대 패턴·주간 비교. 7일은 막대,
  그 이상은 영역+꺾은선 추세 차트(호버/클릭 툴팁)
- [x] 히스토리 영속화 — `rollup.rs`가 일별 요약(사용량·메시지·비용·모델별·시간대별)을
  `<config_dir>/token-runway/rollup.json`에 누적. 백그라운드(60초)가 어제까지 확정분
  증분 저장(첫 회만 백필 ~180일). `get_stats`는 과거=롤업, 오늘만 실시간 파싱해 병합.
  원본 JSONL이 사라져도 과거 유지 → 30일 이상 조회 가능. (이전의 "영속저장 X" 결정 대체)
- [x] 비용 환산 — model별 단가로 일별 API 환산 비용($) (Claude; Codex/Gemini 0)
- [x] 효율 인사이트 — 캐시 적중률 + 코칭 신호(캐시 재생성 과다·요청당 토큰 과다, `insight` 키)
- [x] 소진 속도 추세 화살표 — 가속(↑)/감속(↓)/일정(→), '소진 속도' 라벨 옆
- [x] 자동 업데이트 — `tauri-plugin-updater`(minisign 서명, 애플 공증과 별개). 시작 시
  확인 → 알림, 트레이 "업데이트 확인..."에서 설치·재시작. endpoint=GitHub Releases
  `latest.json`. 공개키는 `tauri.conf.json`, 개인키는 `~/.tauri/token-runway.key`(repo 밖,
  CI는 `TAURI_SIGNING_PRIVATE_KEY` secret). **남은 것: Apple 서명·공증(`APPLE_*` secret)**
- [ ] Cursor / Copilot provider — 데이터 소스 스펙 확보(위 참고), 검증 환경에서 구현 필요
- [ ] 비-macOS Keychain 지원 (`keyring` crate)
