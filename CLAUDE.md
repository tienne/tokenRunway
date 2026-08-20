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
├ activity.rs       에이전트 활동 감지(도는 중/막 끝남/쉬는 중) — pet 반응용
├ rollup.rs         일별 사용량·사용률 롤업 영속화(rollup.json) — 30일+ 히스토리용
├ atomicfile.rs     영속 파일 원자적 쓰기(tmp→rename) + 손상 파일 보존
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
   - **`get_runway`/`get_stats`는 async + `spawn_blocking` 필수** — 동기 command로 두면
     파싱·OAuth가 메인 이벤트 루프에서 돌아 팝오버가 프리즈된다
2. `runway::compute`는 `collect_samples`(시계열) + `official_usage`(공식 사용률)를 조합
3. 백그라운드 스레드(60초)가 `compute_all` + `check_alerts`로 임계치 경보 (창 닫혀도 동작),
   이어서 `record_utilizations`(사용률 이력) + `rollup::update`(일별 사용량)

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
  - **윈도우 경계를 `resets_at`에 맞춰야 한다** — 도구의 5시간 윈도우는 롤링이 아니라
    리셋 시각 기준 고정 구간(`[resets_at - window, resets_at]`)이다. 롤링 합계로 나누면
    경계가 어긋난 만큼 분자가 부풀어 한도가 과대 추정되고 ETA가 낙관적으로 나온다.
    `runway::compute`의 `win_start`와 `lib.rs::estimate_5h_limit`이 같은 규칙을 쓴다.
  - 로컬 샘플이 없어도(다른 기기에서 사용) `pace_eta_minutes`가 윈도우 경과 대비
    소진 페이스로 ETA를 추정한다.
  - `is_estimate`가 true면 우리가 한도를 가정해 만든 값이다(Gemini). 트레이 자동 선택에서
    공식 데이터에 밀리고, 사용률 이력에도 기록하지 않는다.
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

### Grok (`grok.rs`) — 로컬 토큰 추세 (best-effort)
- **시계열**: `~/.grok/sessions/<workspace>/<session-id>/updates.jsonl`
  (JSON-RPC `session/update` NDJSON). `GROK_HOME` 환경변수로 루트 override.
  - `_meta.totalTokens`는 세션 안에서 **단조 증가하는 카운터**(실제 의미는 아래 ⚠️)
    → 사용자 메시지(`sessionUpdate == "user_message_chunk"`)를 턴 경계로,
    턴 내 최댓값 − 턴 시작 직전 값 = 그 턴 증분을 샘플 1건으로. 감소/반복은 무시
  - 첫 사용자 메시지 이전 구간(세션 셋업분)은 턴으로 세지 않고 baseline으로만 쓴다 —
    세면 첫 턴 소비가 이중 계상된다. 단 사용자 메시지가 없는 파일에선 그대로 내보낸다
  - 필드 경로 폴백: `params(.update)._meta.totalTokens`, `agentTimestampMs`,
    `modelId` 등. 모델은 형제 `summary.json`의 `current_model_id`로 폴백
  - 단위 `tokens`, 윈도우 일간(24h)
- **공식 잔여율**: `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`
  (CLI의 `/usage`가 쓰는 것과 같은 경로). 로컬 파일엔 쿼터가 없어서 서버를 봐야 한다
  — 2026-08-06 확인: `auth.json`=OIDC 토큰·계정, `signals.json`=컨텍스트 창·세션 통계,
  `models_cache.json`=`compactions_remaining`, `logs/unified.jsonl`=`remaining_in_queue`.
  - 헤더: `Authorization: Bearer`(auth.json의 `key`), `x-grok-client-identifier: grok-shell`,
    `x-grok-client-mode: billing`, `x-grok-client-version`(`.metadata_version`)
  - 응답: `config.creditUsagePercent`(그대로 utilization), `config.currentPeriod`
    (`type`/`start`/`end`), `onDemandCap`·`onDemandUsed`·`prepaidBalance`,
    `productUsage[]`(제품별 %), `subscription_tier`(있으면 플랜 배지)
  - **주기는 `type` 문자열이 아니라 `end - start`로 잰다** — 관측값은
    `USAGE_PERIOD_TYPE_WEEKLY`(정확히 7일)지만 종류가 늘어도 그대로 맞는다.
    `window_secs()`가 이 값을 돌려주므로 Grok만 윈도우가 동적이다
  - **크레딧은 통합 지갑** — `creditUsagePercent`에 Imagine·Chat이 쓴 몫이 다 들어간다
    (실측: 전체 90% 중 Imagine 89%, GrokBuild 1%). 코딩만 보려면 `productUsage`가
    있지만, 바닥나면 코딩도 못 쓰므로 전체를 잔여율로 쓴다
  - 단기(5시간) 윈도우가 없어 `OfficialUsage`의 5h·7d 슬롯에 같은 값을 넣는다.
    UI는 `window_hours ≥ 168`이면 주간 잔여율 줄을 숨겨 중복을 없앤다
  - 180초 캐시·10초 타임아웃·`FETCH_GUARD`는 Claude provider와 같은 규칙
  - 세션 단위 사용량은 ACP 메서드 `x.ai/session/usage`로도 온다(미사용)
- ⚠️ **`totalTokens`는 컨텍스트 점유량으로 보인다** — 실측에서 관측 최댓값이
  `signals.json`의 `contextTokensUsed`와 정확히 일치했다(49,083). 그렇다면 턴 증분은
  "새로 쌓인 컨텍스트량"이지 실제 API 청구 토큰이 아니다 — 매 턴 전체 컨텍스트를
  다시 보내므로 실제 소비는 더 크다. Claude의 `message.usage`와 의미가 달라
  도구 간 절대 비교는 하지 말 것. 압축(compaction)이 일어나면 값이 줄어드는데
  파서는 감소를 무시하므로 그 구간 소비가 누락된다
  (`totalTokensBeforeCompaction` 보정 미구현).
- 2026-08-06 실데이터 검증 완료 — 필드 경로가 실제와 일치했다
  (`params._meta.totalTokens` 23건, `params._meta.agentTimestampMs` 42건,
  `sessionUpdate == "user_message_chunk"`, `params.update._meta.modelId`).
  스펙 출처는 공개 파서(tokscale `sessions/grok.rs`)

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

- **RunwayEngine** (`runway.rs`): 공식 사용률 우선 → percent/ETA/리셋/주간/플랜 채움
  - **소진 속도는 EWMA** — 최근 60분을 5분 버킷으로 나눠 반감기 20분으로 지수가중.
    단순 평균은 10분만 쉬어도 0으로 떨어져 ETA가 사라졌다 나타났다 했다.
  - **`verdict`** — "소진이 먼저냐 리셋이 먼저냐"를 미리 판정한 한 줄 결론. 사용자가
    ETA와 리셋 시각을 머리로 빼지 않아도 되게 한다. level: `danger`(리셋 전 소진) /
    `good`(리셋까지 여유) / `warn`(리셋 시각 미상)
  - **`seven_day_eta_minutes`** — 주간 한도 소진 페이스. Max 사용자의 실제 병목은
    5시간이 아니라 주간인 경우가 많다. 공식 사용률만으로 계산해 추가 파싱이 없다.
  - **`weekly_days`** — 주간 한도를 날짜별로 쪼갠 소진 분해(`weekly_days()`).
    "주간 47% 남음" 한 줄로는 어느 날 몰아 썼는지 알 수 없어서 붙였다.
    - 절대 토큰 한도가 비공개라 **일별 토큰을 윈도우 총합 대비 공식 사용률로 안분**한다:
      `그날 % = 그날 토큰 ÷ 윈도우 총 토큰 × 공식 사용률`. 이러면 일별 %의 합이 항상
      공식 사용률과 같아 같은 카드 안의 두 숫자가 어긋나지 않는다. 다른 기기에서 쓴
      분량은 로컬에 없지만 비율로 흡수되고, 그만큼 **개별 날짜 값은 근사**다.
    - 슬롯은 윈도우 `[resets_at - 7d, resets_at)`가 걸친 날짜 전부 —
      리셋이 자정이면 7칸, 하루 중간이면 양끝이 부분일이라 8칸이다.
    - 일별 사용량은 롤업(과거) + 오늘 실시간 분으로 얻어 **추가 JSONL 파싱이 없다**.
- **활동 감지** (`activity.rs`): pet이 "지금 도는 중 / 막 끝남 / 쉬는 중"에 반응하게 하는 축.
  잔여율과 별개다 — 잔여율은 30초 폴링이면 되지만 활동은 1.5초여야 반응이 살아 있다.
  그래서 `get_activity`는 네트워크도 전체 파싱도 없이 **도구별 최근 파일 하나의 꼬리
  256KB만** 읽는다. 트리 탐색은 `SCAN_TTL`(10초) 주기로만 하고 그 사이엔 찾아둔 경로를 재사용
  (실측: 1,300개·2GB `~/.claude/projects` 탐색 한 번 80ms, 탐색 없는 폴링은 사실상 공짜).
  - **Codex**: `event_msg`의 `task_started`/`task_complete` — 턴 경계가 명시적
  - **Grok**: `sessionUpdate == "turn_completed"`. `hook_execution`·`session_recap`은 턴이
    끝난 뒤에도 붙어서 판정에서 뺀다
  - **Claude Code**: 마지막 `assistant`/`user` 엔트리로 추론 — `stop_reason == "tool_use"`면
    툴 실행 중, `user`면 응답 대기, 그 밖의 `assistant`면 턴 종료.
    `attachment`·`system`·`ai-title` 등 부수 엔트리는 건너뛴다
  - **Gemini**: `~/.gemini/tmp/<프로젝트>/chats/session-*.jsonl`의 마지막 엔트리
    (`user`/`gemini`/`error`). 다섯 도구 중 유일하게 **실패까지** 구분된다.
    provider가 읽는 `logs.json`은 사용자 메시지만 있어 활동 감지에 못 쓴다
  - **Antigravity**: 완료 이벤트가 없어 `streamGenerateContent` 신선도로만 추론.
    `JustDone`을 만들지 않는다 — 끝난 시점을 모르는데 손을 흔들면 엉뚱한 타이밍에 흔든다
  - 여러 도구가 동시에 돌면 프론트 `leadActivity`가 **가장 최근에 움직인 하나**를 고른다
    (pet은 한 마리라서). 매핑: `working`→`running` 포즈로 제자리,
    `justDone`→`waving` + "다 됐어요" 말풍선, `idle`→기존 배회
  - 작업 중에도 `STROLL_GAP`(30~60초)마다 한 번씩 `STROLL_MS`(5~9초) 짧게 산책한다 —
    에이전트를 하루종일 돌리면 제자리 정지만으로는 pet이 얼어 있는 것처럼 보인다.
    산책 중엔 방향별 걷기 그림을 쓰고, 도착해도 쉬지 않고 계속 걷는다
  - **한가할 때가 일할 때보다 더 돌아다니면 신호가 거꾸로 간다** — 배회는 한 번 걸을 때
    10초쯤 걸리므로 `REST_MS`를 짧게 두면 idle 쪽 걷는 비중이 작업 중보다 커진다.
    그래서 휴식을 10~25초로 길게 잡아 "쉴 때는 늘어져 있다"에 맞춘다
- **AlertManager** (`lib.rs`): 세 가지 경보, 각 도구별 1회 발사 + 회복 시 재무장.
  - 소진 경보 — 잔여율 ≤ 임계치
  - 예상 소진 경보 — ETA ≤ `eta_alert_minutes` **이고** verdict가 `danger`
    (리셋이 먼저 오면 실제로 바닥나지 않으므로 헛경보를 막는다)
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
- **transcript는 dedup 필수** — 세션 재개·worktree·압축으로 같은 메시지가 여러 번
  기록됨. dedup 안 하면 토큰·메시지가 ~2배 부풀려짐. Claude는 `message.id`,
  Codex는 id가 없어 `(timestamp, total, input, cached)` 조합으로 구분한다.
- **OAuth 폴링은 180초 미만 금지** — User-Agent 누락/과빈도 폴링 시 영구 429
- **네트워크 호출 중 데이터 락 금지** — `fetch_cached`는 캐시 락을 놓고 HTTP를 탄다.
  잡은 채로 타면 응답이 늦을 때 UI 폴링·백그라운드 루프가 전부 매달려 앱이 굳는다.
  중복 갱신은 `FETCH_GUARD.try_lock()`으로 막고, 실패하면 기다리지 않고 직전 값을 쓴다.
  ureq에는 반드시 `.timeout()`을 건다.
- **활동 감지에 mtime을 그대로 쓰면 안 된다** — Antigravity 로그는 언어 서버라 아무것도
  안 할 때도 `quotaRefreshLoop` 같은 줄을 계속 쓴다. mtime만 보면 항상 "작업 중"이 된다.
  마지막 줄과 마지막 요청 줄의 glog 시각차를 재고 거기에 파일 경과를 더한다.
  glog 머리(`I0622 17:55:06`)에는 연도가 없어 절대 시각을 못 만드니 **같은 파일 안의 두 줄
  비교로만** 쓴다.
- **활동 판정엔 staleness 컷이 필수** — 턴 중간에 도구를 종료하면 마지막 줄이 "도는 중"
  모양으로 남아 pet이 영원히 일한다. `STALE_MS`(5분) 넘게 안 자란 파일은 쉬는 것으로 본다.
- **영속 파일은 `atomicfile::write_atomic`으로** — `fs::write` 중 크래시하면 파일이
  깨지고, 롤업은 원본 JSONL이 사라진 뒤의 유일한 히스토리라 복구가 안 된다.
  파싱 실패 시엔 조용히 기본값으로 넘어가지 말고 `preserve_corrupt`로 남긴다.
- **일별 통계는 빈 날을 0으로 채운다** (`date_range`) — 사용 안 한 날이 빠지면
  추세 그래프가 압축돼 왜곡되고, 주간 평균의 분모가 활동일 수가 돼 요금제 추천이
  상향 쪽으로 치우친다.
- **사용률은 관측 시점에만 남길 수 있다** — 과거 JSONL로 되돌려 계산할 수 없다.
  `rollup::update`가 샘플을 재집계할 때 `peak_*_util`을 덮어쓰지 않도록 주의.
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
- [x] 히스토리 영속화 — `rollup.rs`가 일별 요약(사용량·메시지·비용·모델별·시간대별·
  최대 사용률)을 `<config_dir>/token-runway/rollup.json`에 누적. 백그라운드(60초)가
  어제까지 확정분 증분 저장(첫 회만 백필 ~180일). `get_stats`는 과거=롤업, 오늘만
  실시간 파싱해 병합. 원본 JSONL이 사라져도 과거 유지 → 30일 이상 조회 가능.
  사용률만 오늘 엔트리에 실시간 기록되므로 재집계가 덮어쓰지 않게 병합한다.
  (이전의 "영속저장 X" 결정 대체)
- [x] 비용 환산 — model별 단가로 일별 API 환산 비용($) (Claude; Codex/Gemini 0)
- [x] 효율 인사이트 — 캐시 적중률 + 코칭 신호(캐시 재생성 과다·요청당 토큰 과다, `insight` 키)
- [x] 소진 속도 추세 화살표 — 가속(↑)/감속(↓)/일정(→), '소진 속도' 라벨 옆
- [x] 한 줄 결론 — "이 페이스면 리셋 전에 소진 / 리셋까지 여유"를 카드 최상단에
  (`verdict`). ETA 경보도 이 판정을 재사용해 리셋이 먼저일 때 울리지 않는다
- [x] 주간 소진 페이스 — 주간 잔여율 옆에 "이 페이스면 N일 후 소진"
  (`seven_day_eta_minutes`). Max 사용자의 실제 병목이 주간 한도인 경우 대응
- [x] 주간 한도 일별 분해 — 리셋 요일부터 각 날이 주간 한도의 몇 %를 썼는지
  (`weekly_days`). 팝오버는 압축 미니 막대, 히스토리 창은 일별 막대 + 누적 선.
  안분이라 막대 합계가 항상 공식 사용률과 일치한다
- [x] 요금제 하향 추천 안전 가드 — 평균만 보면 "마감 주에 몰아 쓰는" 사용자에게
  하향을 권하게 되므로, 롤업의 주간 사용률 이력에서 최악의 주로 검증
- [x] 자동 업데이트 — `tauri-plugin-updater`(minisign 서명, 애플 공증과 별개). 시작 시
  확인 → 알림, 트레이 "업데이트 확인..."에서 설치·재시작. endpoint=GitHub Releases
  `latest.json`. 공개키는 `tauri.conf.json`, 개인키는 `~/.tauri/token-runway.key`(repo 밖,
  CI는 `TAURI_SIGNING_PRIVATE_KEY` secret). **남은 것: Apple 서명·공증(`APPLE_*` secret)**
- [x] Grok 잔여율 — billing API 연동. 주기가 응답에 따라 달라져 `window_secs()`가
  동적이다. UI 윈도우 라벨도 길이에 맞춰 "세션/일간/주간/월간"으로 바뀐다
- [x] pet이 에이전트 활동에 반응 — 도는 중/막 끝남/쉬는 중을 로그 꼬리로 감지해
  `running`/`waving` 포즈와 "다 됐어요" 말풍선으로 (`activity.rs`, `get_activity`).
  Codex·Grok은 명시적 턴 이벤트, Claude·Gemini는 마지막 엔트리 추론, Antigravity는
  요청 신선도. 커스텀 정지 이미지 4장 번들은 포즈가 없어 말풍선만 뜬다
- [ ] pet 활동 감지 정확도 — Claude Code는 `Stop`/`PreToolUse` 훅으로 상태 파일을
  떨어뜨리면 추측이 없어진다. 사용자 `settings.json`을 건드려야 하니 설정 토글로 얹을 것.
  Antigravity의 완료 시점도 훅 말고는 방법이 없다
- [ ] 주간 ETA에 요일 가중 — `seven_day_eta_minutes`는 경과 시간 대비 선형 페이스라
  평일에 완만하고 주말에 몰아 쓰는 패턴에서 낙관적으로 어긋난다. 2026-08 실측(롤업):
  화 21% → 목 55% → 일 84% → 월 100%가 2주 연속(8/2, 8/10)이라, 목요일까지는 "리셋까지
  여유"로 보였는데 결국 소진했다. 롤업에 요일별 사용량이 이미 있으니 같은 요일의 과거
  소비 비중으로 남은 날을 추정하면 "지난 2주 패턴이면 일요일에 소진"까지 말할 수 있다.
- [ ] Cursor / Copilot provider — 데이터 소스 스펙 확보(위 참고), 검증 환경에서 구현 필요
- [ ] 비-macOS Keychain 지원 (`keyring` crate)
