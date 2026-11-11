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
├ activity.rs       에이전트 활동 감지(도는 중/입력 대기/막 끝남/쉬는 중) — pet 반응 + 세션 목록
├ rollup.rs         일별 사용량·사용률 롤업 영속화(rollup.json) — 30일+ 히스토리용
├ atomicfile.rs     영속 파일 원자적 쓰기(tmp→rename) + 손상 파일 보존
├ inbox.rs          알림함 — trw로 들어온 완료 알림 큐·영속(inbox.json)
├ ipc.rs            trw 소켓 리스너(trw.sock) + 앱 꺼진 사이 스풀(pending/) 소비
├ landing.rs        알림 클릭 시 원래 세션으로 되돌리는 어댑터(Orca/Paseo/경로)
├ notify_os.rs      완료 알림의 OS 알림 발송 + 클릭 응답 수신
├ installer/        trw CLI·스킬 설치(mod.rs) + 사용자 설정 파일에 훅 넣고 빼기
│  ├ template.rs    diff 미리보기·지문 확인·백업·설치 기록(hook-installs.json) 공용 틀
│  ├ codex_hooks.rs Codex hooks.json
│  ├ claude_hooks.rs Claude settings.json의 hooks
│  └ statusline.rs  Claude settings.json의 statusLine
├ notch.rs          노치 양옆 섬 — NSPanel 바꿔치기, 노치 지오메트리, 커서 폴링 클릭 통과
└ providers/
   ├ mod.rs         UsageProvider trait + 공용 타입 + find_recent_jsonl 헬퍼
   ├ claude_code.rs Keychain OAuth + ~/.claude JSONL
   ├ claude_accounts.rs Claude 계정 발견(Keychain 해시 규칙 + 오르카 저장소)
   ├ claude_cli_refresh.rs 토큰 만료 계정을 `claude -p /usage`로 CLI가 갱신하게 함
   ├ last_usage.rs  계정별 마지막으로 받은 사용률 보관(last-usage.json)
   ├ codex.rs       ~/.codex JSONL (token_count + rate_limits)
   ├ gemini.rs      ~/.gemini 로그 요청 수
   └ antigravity.rs ~/.gemini/antigravity-cli 전송 로그
src-tauri/trw/      `trw` CLI (별도 crate — 이유는 주의사항)
src-tauri/skills/   번들로 배포하는 에이전트 스킬(trw-notify)
src/                React UI (App.tsx 대시보드, Notch.tsx 노치 섬, SessionList.tsx 세션 목록)
design/             아이콘 SVG 소스
```

## 데이터 흐름

1. UI가 30초마다 `get_runway` invoke → `compute_all()` → 각 provider별 `runway::compute`
   - **`get_runway`/`get_stats`는 async + `spawn_blocking` 필수** — 동기 command로 두면
     파싱·OAuth가 메인 이벤트 루프에서 돌아 팝오버가 프리즈된다
2. `runway::compute`는 `collect_samples`(시계열) + `official_usage`(공식 사용률)를 조합
3. 백그라운드 스레드(60초)가 `compute_all` + `check_alerts`로 임계치 경보 (창 닫혀도 동작),
   이어서 `record_utilizations`(사용률 이력) + `rollup::update`(일별 사용량)
4. 활동은 별도 축이다 — Claude/Codex 훅이 `trw hook <도구> <이벤트>`로
   `~/.token-runway/hooks/<도구>-<세션>.json`에 한 줄 상태를 남기고, pet과 노치가 1.5초마다
   `get_activity`(도구별 대표)·`get_sessions`(세션별 목록)로 읽는다. 훅이 없거나 낡았으면
   로그 꼬리 추론으로 넘어간다. 세션 줄을 누르면 `land_session`이 상태 파일의 랜딩 정보로
   `landing.rs`를 탄다

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
- **계정이 여럿일 때** (`claude_accounts.rs`) — 한 머신에 Claude 계정이 여러 개 있으면
  전부 찾아 계정마다 잔여율을 받는다. 훑는 곳은 세 군데다.
  - Keychain `Claude Code-credentials[-<sha256(config_dir) 앞 8자>]`.
    `CLAUDE_CONFIG_DIR`로 계정을 나누면 여기 항목이 여러 개 생긴다. 접미사 해시는
    되돌릴 수 없어 후보 디렉토리(`~/.claude*`, `~/.config/*claude*`)를 해싱해 이름을
    만들고 그 이름으로 직접 조회한다. **`security dump-keychain`은 쓰지 않는다** —
    Claude와 무관한 항목까지 키체인 전체를 훑고 출력 형식(hex blob, 이스케이프된
    따옴표)이 바뀌면 서비스가 조용히 빠진다
  - 오르카 `~/Library/Application Support/orca/claude-runtime-auth/system-default-auth.json`
    — 오르카 계정으로 전환되며 밀려난 원래 로그인(`keychainCredentialsJson`) + 라벨
  - 오르카 `claude-accounts/<uuid>/auth/oauth-account.json`(라벨) + Keychain
    `Orca Claude Code Managed Credentials`(토큰, account=`<uuid>`)
  - **오르카는 `CLAUDE_CONFIG_DIR`이 아니라 Keychain 항목을 통째로 바꿔치기**해서
    계정을 전환한다. 활성 계정만 읽으면 나머지 계정이 통째로 안 보인다
  - **최신 오르카는 프로필마다 `CLAUDE_CONFIG_DIR`을 `claude-profiles/<uuid>/home`으로
    잡는다** — 살아 있는 토큰은 그 경로 해시가 붙은 Keychain 항목에만 있다. 위
    `claude-accounts`나 `system-default-auth.json`의 토큰은 갱신이 멈춘 사본이라 며칠
    지나면 만료돼 401이 난다(2026-10 확인: 둘 다 열흘쯤 전에 만료). 그래서 이 home을
    config dir 후보에 넣어 Keychain 소스가 먼저 이기게 한다
  - 계정 구분은 조직 UUID로 한다 — 같은 이메일로 조직 두 곳(Team/Enterprise)에
    속하면 쿼터가 따로 돌기 때문에 이메일로는 못 가른다. 조직을 못 읽으면 토큰 지문
  - **활성 계정은 `~/.claude.json`의 `organizationUuid`와 맞는 하나뿐이다.** Keychain에
    항목이 있다는 것만으로 활성이라고 보면 config dir을 나눈 환경에서 활성이 여럿이
    되는데, 로컬 JSONL(`~/.claude/projects`)의 주인은 그중 하나다. 프로필을 못 읽어도
    홈 프로필로 때우지 않는다 — 다른 config dir의 계정이 홈 조직 UUID를 얻으면 중복
    제거에 삼켜져 목록에서 통째로 사라진다
  - **조직을 못 읽으면 기본 config dir(`~/.claude`) 계정을 활성으로 세운다.** 이 폴백이
    없으면 홈 config가 없거나 `oauthAccount`가 빈 환경에서 대표가 안 서고 카드가 통째로
    빈다 — 계정 기능을 넣기 전에는 토큰만 있으면 동작했으므로 그건 회귀다
  - **알려진 제약** — `CLAUDE_CONFIG_DIR`로 홈이 아닌 config dir에서 작업하면 활성
    판정이 기본 config dir 계정을 가리킨다. 시계열을 `~/.claude/projects` 한 곳에서만
    읽어 활성의 뜻이 그 디렉토리에 묶여 있기 때문이다. 그 값은 카드 헤더와 트레이뿐
    아니라 배터리 레벨, 경보 임계 판정, 사용률 이력, 요금제 사다리까지 간다 — 바로
    아래 항목이 위험 목록으로 열거한 그 소비처들이다. 계정 목록과 계정별 잔여율,
    리셋 시각은 정확하다. 이걸 풀려면 `collect_samples`가 config dir마다 따로 읽어야 한다
  - **계정이 하나뿐이면 조직 비교와 무관하게 그 하나가 활성이다** — 남의 사용률이
    올라올 위험 자체가 없다. 둘 이상일 때는 조직이 다르다고 판정한 계정을 뒤집지 않는다
  - 오르카 쪽 두 소스는 내부 구조라 포맷이 바뀌면 못 읽는다. 실패하면 조용히 건너뛰고
    나머지 계정만 쓴다
  - 사용률 180초 캐시는 계정별로 나눠 잡는다(`USAGE_CACHE`는 계정 id 맵). 계정 목록
    자체는 60초 캐시 — Keychain 훑기를 폴링마다 할 만큼 싸지 않다
  - **안 쓰는 계정은 몇 시간 뒤 401이 난다** — CLI가 그 계정 토큰을 갱신하지 않아서다.
    그래서 두 겹으로 버틴다
    - **마지막 값 보관** (`last_usage.rs`) — 받은 사용률을 계정마다
      `~/.token-runway/last-usage.json`(키 `<도구>#<id>`, 30일 보관)에 남긴다. 401·429로
      못 받으면 이 값을 `AccountUsage.observed_at_ms`와 함께 흐리게 "N시간 전 값"으로
      띄운다. 리셋 시각이 지난 윈도우는 사용률 0으로 넘긴다(`roll_forward`). 이 값은
      대표 계정, 트레이, 경보, 사용률 이력, ETA 어디에도 안 들어간다 — 지금 값이 아니라서다.
      노치 `figureOf`의 폴백도 이 값은 건너뛴다
    - **CLI 갱신** (`claude_cli_refresh.rs`, 설정 `claude_cli_refresh`, 기본 켜짐) — 401이
      난 계정만 그 config dir로 `claude -p /usage`를 백그라운드에서 한 번 돌린다. CLI의
      사용량 조회 경로가 만료 토큰을 먼저 갱신하고 Keychain에 다시 쓴다. 성공하면 계정
      목록 캐시 시각을 되돌리고 그 계정 사용률 캐시를 지워 다음 폴링이 새 토큰으로 묻는다.
      **우리는 refresh token을 쓰지 않는다** — CLI가 쥔 토큰과 어긋나지 않게
      - 플래그: `--no-session-persistence`(없으면 `projects`에 세션 JSONL이 남아 활동
        감지가 도는 세션으로 본다), `--settings '{"disableAllHooks":true}'`(우리 훅까지
        상태 파일을 남긴다), `--strict-mcp-config`(MCP를 안 띄운다). **`--bare`는 못 쓴다** —
        OAuth와 Keychain을 아예 안 읽어 갱신이 안 일어난다
      - 환경: `CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_OAUTH_TOKEN`,
        `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`을 지운다(남으면 API 키 경로를 타거나
        엉뚱한 계정을 갱신한다). 접미사 없는 레거시 Keychain 항목의 계정은
        `CLAUDE_CONFIG_DIR`을 **아예 지운** 채로 띄운다 — `~/.claude`를 명시하면 CLI가 해시가
        붙은 이름을 읽는다(`CliHome::Default`). 오르카 저장소에서만 찾은 계정은 config dir을
        몰라 갱신하지 못한다
      - 한 번에 하나만 돌고(`IN_FLIGHT`) 같은 계정은 30분 안에 다시 띄우지 않는다. 20초
        안에 안 끝나면 죽인다. 출력은 버린다
      - 실측: `/usage`는 모델을 부르지 않는다(`num_turns` 0, 비용 0, 10초 안팎). 갱신 경로는
        CLI 2.1.322 코드로만 확인했다 — 확인할 때 만료된 계정이 없었다. 동시에 도는 세션과의
        갱신 경합은 CLI가 자체 잠금 파일로 푼다(Codex처럼 도는 홈을 건너뛰지 않는 이유)

### Codex (`codex.rs`) — 정확, 더 쉬움
- **시계열**: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, `event_msg`/`token_count`의
  `info.last_token_usage.total_tokens`
- **공식 잔여율**: 사용량 API(아래 계정 항목) 우선, 폴백은 같은 JSONL의 `payload.rate_limits`
  - `primary`(5h, used_percent/resets_at), `secondary`(주간), `plan_type`("plus")
  - `resets_at`은 epoch seconds → RFC3339 변환
  - 계정마다 사용량 API 우선, 실패하면 최근 30일 파일의 가장 신선한 rate_limits (아래 계정 항목)
- **오르카에서 띄운 Codex는 `~/.codex`에 안 쌓인다** — 오르카가 계정마다 `CODEX_HOME`을
  `orca/codex-accounts/<uuid>/home`으로 잡는다. `codex::session_roots`가 `$CODEX_HOME`,
  `~/.codex`, 오르카 계정 홈, `codex-runtime-home`을 모두 훑고 활동 감지도 같은 목록을 쓴다
  (2026-11 확인: `~/.codex/sessions`는 9월 이후 비어 있었다)
- **계정이 여럿이면 홈마다 계정이 다르다** — 홈의 `auth.json`(id_token 본문의 이메일·플랜,
  `tokens.account_id`)로 계정을 찾고, rollout 첫 줄 `session_meta.creator_account_id`로
  파일 주인을 가른다. auth.json은 계정을 바꾸면 덮어써져서 파일에 적힌 값을 먼저 믿는다
  (0.16x 이전 CLI는 이 필드가 없어 홈 계정으로 간다)
  - 활성 계정은 가장 최근에 rate_limits를 남긴 계정이다. Claude와 달리 시계열도 계정별로
    가를 수 있어 카드 계산은 `collect_active_samples`(활성 계정 파일만)를 쓰고, 히스토리
    롤업은 `collect_samples`(전체)를 쓴다
  - 계정마다 윈도우가 달라(5시간/주간) `AccountUsage.window_secs`로 따로 들고 간다
  - **잔여율은 서버 값이 먼저다** — `GET https://chatgpt.com/backend-api/wham/usage`
    (Codex CLI `/status`와 같은 경로, 0.166.0 바이너리 문자열로 확인). 헤더는
    `Authorization: Bearer <auth.json access_token>`, `ChatGPT-Account-Id`,
    `User-Agent: codex_cli_rs/<ver>`. 응답 `rate_limit.primary_window`·`secondary_window`
    (`used_percent`, `limit_window_seconds`, `reset_at`)를 rollout의 `RateLimits` 모양으로
    옮겨 같은 변환을 탄다. 다른 기기·웹 사용분과 오래 안 쓴 계정의 현재 값이 여기서만 나온다
  - 180초 계정별 캐시, 10초 타임아웃, `FETCH_GUARD`는 Claude provider와 같은 규칙.
    같은 계정이 여러 홈에 있으면 `last_refresh`가 가장 늦은 홈의 토큰을 쓴다
  - **토큰을 우리가 갱신하지 않는다** — Codex refresh token은 한 번 쓰면 바뀌어서, 갱신하면
    CLI가 들고 있는 토큰이 죽어 그 계정이 로그아웃된다. 401이면 `error.stale_token`
    (2026-11 확인: 8월 이후 안 쓴 홈의 토큰이 `invalid_jwt`로 401)
  - API가 실패하면 로컬 rollout의 마지막 기록으로 버틴다. 리셋 시각이 지난 기록은
    사용률 0으로 보고, 30일 안에 기록도 없으면 실패 사유를 띄운다
  - **401 계정은 `codex app-server`로 한 번 더 묻는다** (`codex/app_server.rs`, 설정
    `codex_app_server_fallback`, 기본 켜짐). 토큰 갱신을 CLI 자신에게 맡기는 길이라
    우리가 refresh token을 쓰지 않는다. JSON-RPC는 줄 단위이고 `jsonrpc` 필드가 없다 —
    `initialize` → `initialized` 알림 → `account/rateLimits/read`. 응답의 `usedPercent`는
    정수, `resetsAt`은 epoch 초(null 가능), `rateLimitResetCredits.availableCount`가
    남은 무료 리셋 횟수다. 15초 안에 답이 없으면 자식을 죽이고, 실패해도 180초 안에는
    다시 띄우지 않는다
  - **떠 있는 세션 옆에서 app-server를 띄워도 되는지는 판정 불가**(2026-11-08 실험,
    codex-cli 0.166.0, `codex-runtime-home`의 Plus 계정 하나만). 같은 `CODEX_HOME`으로
    세션 A(턴 실행)를 띄운 채 B로 사용량을 묻고 A에서 다음 턴을 돌려봤는데, 그 계정의
    refresh token이 이미 폐기된 상태라 A의 두 턴 모두 `unauthorized`("refresh token was
    revoked")로 실패했고 B도 wham/usage 401을 돌려받았다. 갱신이 한 번도 일어나지 않아
    `auth.json` 해시와 `last_refresh`는 실험 내내 그대로였다 — 갱신 경합으로 세션이
    깨지는지는 확인하지 못했다. 그 계정은 실험 전부터 로그아웃 상태였고 실험이 새로
    로그아웃시킨 것은 없다. 그래서 **최근 `STALE_MS`(activity와 같은 5분) 안에 rollout이
    자란 홈은 건너뛴다** — 도는 세션과 토큰 갱신을 겨루지 않게. refresh token까지 죽은
    계정은 app-server로도 못 살리고 `error.stale_token`이 그대로 남는다(재로그인 필요)

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
  - **Claude Code**: 훅을 깔았으면(`claude_hooks_installed`) 세션마다 훅 상태가 먼저다.
    이벤트 매핑은 공식 문서(https://code.claude.com/docs/en/hooks) 기준 —
    `UserPromptSubmit`·`PreToolUse`·`PostToolUse(Failure)`→working,
    `Notification`(`permission_prompt`·`elicitation_dialog`·`elicitation_url_dialog`·
    `agent_needs_input`)→waiting, `Stop`→done, `StopFailure`→done+실패, `SessionStart`→idle,
    `SessionEnd`→파일 삭제. `SubagentStop`은 끝남이 아니라 설치하지 않는다(서브에이전트가
    끝나도 메인 턴은 돈다). `idle_prompt` 알림은 사람이 입력할 차례라는 뜻일 뿐 막혀 있는 게
    아니라 상태를 바꾸지 않는다. 한 세션이라도 working이면 working, 없으면 waiting, 그다음
    가장 최근 상태다.
    훅이 없거나 낡았으면 JSONL 마지막 `assistant`/`user` 엔트리로 추론 — `stop_reason ==
    "tool_use"`면 툴 실행 중, `user`면 응답 대기, 그 밖의 `assistant`면 턴 종료.
    `attachment`·`system`·`ai-title` 등 부수 엔트리는 건너뛴다
  - **Codex 훅**도 같은 상태 파일·같은 판정 코드(`HOOK_TOOLS`)를 쓴다. 예전 trw가 남긴
    파일은 pid가 없어 신선할 때(5분)만 믿는다
  - **Gemini**: `~/.gemini/tmp/<프로젝트>/chats/session-*.jsonl`의 마지막 엔트리
    (`user`/`gemini`/`error`). 다섯 도구 중 유일하게 **실패까지** 구분된다.
    provider가 읽는 `logs.json`은 사용자 메시지만 있어 활동 감지에 못 쓴다
  - **Antigravity**: 완료 이벤트가 없어 `streamGenerateContent` 신선도로만 추론.
    `JustDone`을 만들지 않는다 — 끝난 시점을 모르는데 손을 흔들면 엉뚱한 타이밍에 흔든다
  - 여러 도구가 동시에 돌면 프론트 `leadActivity`가 **가장 최근에 움직인 하나**를 고른다
    (pet은 한 마리라서). 매핑: `working`→`running` 포즈로 제자리,
    `waiting`→`waving` + "확인이 필요해요" 말풍선(대기하는 동안 계속), `justDone`→`waving` +
    "다 됐어요" 말풍선, `idle`→기존 배회
  - **세션 목록** (`get_sessions` → `SessionList.tsx`, 노치 펼친 화면): 훅이 하나도 안
    깔렸으면 `installed: false`로 목록 자리를 그리지 않는다. 꺼진 도구(`disabled_tools`)와
    훅 토글이 꺼진 도구의 상태 파일은 읽지 않는다. 훅이 없거나 세션이 0개면 `SessionList`가
    `null`을 돌려 `.notch-sessions`가 `:empty`로 접힌다 — 빈 안내 줄이나 래퍼를 넣으면 이 접힘이
    깨진다. 목록은 `.notch-scroll` 안쪽 측정 대상 안에 있어 늘고 줄 때 `notch_fit` 높이에 잡힌다
    입력 대기는 노치 활동 글자와 세션 점 모두 노란색(#ffd60a)에 속 빈 고리를 붙인다. 위험 색
    (#ff453a)은 잔여율 경고 자리라 쓰지 않고, 작업 중(#64d2ff)과 밝기가 비슷해 모양으로도 가른다
  - 작업 중에도 `STROLL_GAP`(30~60초)마다 한 번씩 `STROLL_MS`(5~9초) 짧게 산책한다 —
    에이전트를 하루종일 돌리면 제자리 정지만으로는 pet이 얼어 있는 것처럼 보인다.
    산책 중엔 방향별 걷기 그림을 쓰고, 도착해도 쉬지 않고 계속 걷는다
  - **한가할 때가 일할 때보다 더 돌아다니면 신호가 거꾸로 간다** — 배회는 한 번 걸을 때
    10초쯤 걸리므로 `REST_MS`를 짧게 두면 idle 쪽 걷는 비중이 작업 중보다 커진다.
    그래서 휴식을 10~25초로 길게 잡아 "쉴 때는 늘어져 있다"에 맞춘다
- **작업 완료 알림** (`trw` → `ipc.rs` → `inbox.rs` → `notify_os.rs` → `landing.rs`):
  다른 앱·세션에서 "작업 끝났다"를 받아 알리고, 누르면 그 세션으로 되돌려보낸다.
  - **감지는 세션이 한다** — 앱이 상태 파일을 폴링하지 않는다. 오르카는
    `agent-hooks/last-status.json`에 `state: done`을 다 모아두지만 그건 턴이 끝날
    때마다 찍혀서, 모니터링 루프의 중간 턴과 진짜 완료를 가르지 못한다. 알릴 만한
    시점인지는 에이전트가 판단하고 `trw notify`를 부른다(판단 기준은 `skills/trw-notify`)
  - **랜딩 정보는 `trw`가 자동으로 붙인다** — 오르카는 `ORCA_WORKTREE_ID`·`ORCA_TAB_ID`,
    Paseo는 `PASEO_AGENT_ID`(없으면 cwd로 `~/.paseo/agents/` 역매칭), 그 밖은 cwd.
    `trw where`로 지금 세션이 어떻게 인식되는지 확인한다
  - **터미널 handle은 클릭할 때 다시 찾는다** — 발송 시점 값을 굳히면 터미널이
    재생성됐을 때 어긋난다. 그 탭이 없으면 같은 워크트리의 다른 터미널로, 그것도
    없으면 `orca file open`으로 워크트리까지는 데려간다(터미널을 새로 만들지는 않는다)
  - **읽음은 "눌러서 랜딩했다"는 뜻** — 리스트를 열어보는 것만으로는 안 읽은 게
    남는다. 배지가 "아직 확인 안 한 완료 건수"여야 하기 때문이다. 읽은 항목은 7일
    보관하고, 안 읽은 건 나이와 무관하게 남긴다(놓친 알림이 조용히 사라지지 않게)
  - **완료 알림의 방해금지는 별도 토글**(`completion_alerts_quiet`, 기본 꺼짐) —
    잔여율 경보와 성격이 다르다. 밤에 돌려놓고 자면 막아야 맞지만 밤에 직접 작업
    중이면 막으면 알림을 놓친다
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
bash scripts/build-trw.sh       # trw를 externalBin 자리에 놓기 (--debug로 debug 빌드)
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
- **모니터 배치는 캐시해두면 안 된다** — `pet_areas`를 앱 시작 때만 받으면, 모니터를
  뺐다 꽂았을 때 캐시된 목록엔 그 모니터가 없다. 거기에 pet을 놓아도 `clampToAreas`가
  "가장 가까운" 예전 모니터로 끌어당겨 **모니터 간 이동이 막힌 것처럼 보인다**
  (앱을 껐다 켜야 풀린다). Tauri에 디스플레이 변경 이벤트가 없어서 드롭할 때마다
  다시 받고 `AREA_SYNC_MS`(15초) 주기로도 확인한다. 단 **배치가 실제로 바뀐 때만**
  위치·목표를 다시 잡는다 — 매번 목표를 다시 뽑으면 걷던 걸음이 끊긴다.
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
- **대표 계정은 "가장 적게 남은" 쪽이 아니라 "지금 쓰는" 쪽이고 폴백이 없다** —
  안 쓰는 계정이 바닥나 있으면 트레이에 0%가 뜨는데 정작 작업 중인 세션은 멀쩡해서
  그 숫자만 보고는 상황을 잘못 판단하게 된다. 게다가 로컬 JSONL은 활성 계정 것이라
  대표가 다른 계정이면 소진 속도와 ETA, 주간 일별 분해, 요금제 추천이 전부 근거를
  잃는다. 그래서 **활성 계정을 못 받아도 다른 계정을 대신 세우지 않는다** —
  `representative`가 None을 주고 `status_note`가 사유를 알린다. 다른 계정 값은 계정
  줄에 그대로 남아 정보를 잃지 않는다. 폴백을 다시 넣으려면 로컬 시계열과 계정이
  어긋나는 계산(5시간 ETA, `win_start` 정렬, 주간 일별 분해, `estimate_5h_limit`,
  요금제 사다리)을 전부 다시 막아야 한다
- **JSONL에는 계정 표시가 없다** — 라인을 다 뒤져도 조직·계정 필드가 없다
  (`message.usage.service_tier`뿐). 계정별 시계열을 못 나누니 계정별 소진 속도도 못 잰다.
  그래서 계정 줄의 ETA는 전부 윈도우 경과 대비 페이스 추정(`pace_eta_minutes`)이다
- **사용률 이력에 다른 계정 값을 섞지 말 것** — 카드 값이 곧 활성 계정 값이라(대표
  폴백이 없다) `record_utilizations`가 그대로 기록해도 안전하다. 이 이력은 요금제
  추천이 "최악의 주"를 판정하는 근거라 계정이 섞이면 그대로 왜곡된다
- **경보 키는 계정 수에 따라 `tool`과 `tool#id` 사이를 오간다** — 계정이 둘이었다가
  하나가 되면 남은 `tool#id` 키가 재무장될 기회를 잃어 다음 소진 때 조용히 안 운다.
  `prune_alert_keys`가 키 모양(`tool`이냐 `tool#id`냐)이 바뀐 도구만 정리한다. **모양이
  그대로면 남긴다** — 429나 콜드 스타트로 잠깐 결측된 것뿐인데 키를 지우면
  복구할 때 잔여율이 임계 이하 그대로인데도 경보가 다시 울린다
- **계정 중복 제거는 조직 UUID와 토큰 지문을 함께 본다** — 소스마다 프로필 유무가 달라
  한쪽은 조직 UUID를, 다른 쪽은 토큰 지문을 id로 받는다. 하나만 보면 같은 계정이 두 줄로
  남아 계정이 하나인 사용자에게 배지와 계정 줄이 뜬다
- **노치 섬은 tao 창 클래스를 `TRWNotchPanel`(NSPanel 하위)로 바꿔치기한다** (`notch.rs`) —
  그냥 NSWindow로 두면 메뉴바 위에 못 올라가고 누를 때마다 앱이 활성화돼 앞 앱 포커스를
  빼앗는다. 바꿔치기는 인스턴스 크기가 같아야 안전해서 tao의 `focusable` ivar를 똑같이
  붙이고, 크기가 다르면 전환을 포기하고 창을 닫는다(`panel_class_matches_tao_window_size`
  테스트). NSPanel 기본값인 `hidesOnDeactivate`는 꺼야 한다 — 이 앱은 거의 항상 비활성이다.
  `set_activation_policy`는 부르지 않는다(Dock 배지가 Regular에 묶여 있다)
- **노치 펼침과 클릭 통과는 Rust 스레드가 정한다** — 창은 펼친 크기로 고정해두고 커서가
  섬(펼친 뒤엔 펼친 영역) 안일 때만 `set_ignore_cursor_events(false)`로 바꾼다. 프론트가
  hover를 보고 IPC로 알리면 그 왕복 사이 클릭이 뒤 창으로 샌다. 프론트는 `notch-mode`(collapsed/notice/full)만 받아 그린다.
  노치 없는 화면에서는 창을 만들지 않고, 5초마다 화면 배치를 다시 재서 덮개를 다시 열면 되살린다.
  펼친 창은 최대 크기(`EXPANDED_WIDTH`×`EXPANDED_MAX_HEIGHT`)로 고정하고, 프론트가 잰 내용
  높이를 `notch_fit`으로 받아 섬 높이와 클릭 영역을 그 높이에 맞춘다. 패널은 접혀 있어도
  늘 렌더링해 둔다 — 펼치기 전에 높이를 재서 넘겨 둬야 펼친 순간부터 클릭 영역이 맞는다
- **트레이는 노치 섬이 주 화면에 떠 있을 때만 숨긴다** (`tray_visible`, `tray_shows_figures`) —
  설정 `tray_title_mode`가 `auto`(기본)면 이 경우 아이콘까지 통째로 뺀다(`set_visible(false)`).
  섬이 숫자와 위험 색, 바로가기를 다 들고 있어서다. `always`는 항상 숫자, `icon_only`는
  항상 아이콘만 남긴다. 트레이가 빠지면 메뉴의 업데이트 확인과 종료도 같이 사라지니
  설정 창 맨 아래(`check_update_now`, `quit_app`)에 같은 버튼을 둔다. Dock 우클릭 종료와
  ⌘Q도 그대로 된다. 주 화면은 `NSScreen.screens` 0번(메뉴바가 있는 화면)이다. 외장
  모니터가 주 화면이고 노치 달린 내장 화면이 보조면 노치 섬은 보조 화면에만 뜨고, 눈이
  가는 메뉴바엔 섬이 없으니 숫자를 그대로 둔다. 숫자가 두 군데 겹치는 건 해가 없지만
  어느 쪽에도 안 보이면 경고를 놓치기 때문이다. 화면 판정은 `notch.rs`의 5초 배치 확인이
  `Presence`(Absent/Secondary/Primary)가 바뀔 때만 `refresh_tray_title`을 불러 알린다 —
  트레이 쪽이 따로 폴링하지 않는다. `set_title`과 `set_visible`도 직전과 같은 값이면 부르지 않는다.
  빨간 위험 아이콘은 모드와 무관하게 늘 바뀐다 — 숫자를 숨겨도 위험은 보여야 한다.
  아이콘이 빠진 동안엔 노치 숫자의 위험 색이 그 자리를 맡는다
- **대시보드와 설정, 히스토리, 알림함 창은 펼친 노치 섬 오른쪽에 띄운다** (`notch::place_beside`) —
  트레이가 숨으면 `TrayBottomCenter`가 기댈 아이콘 좌표가 없다. 노치 바로 밑에 두면 펼친 섬이
  창 위를 덮어서 섬 오른쪽 끝 + `BESIDE_GAP`에 붙이고 화면 오른쪽 끝을 넘으면 안쪽으로 당긴다.
  섬은 가운데 정렬이라 왼쪽으로 뒤집을 일은 없다. **숨어 있던 창만 옮긴다** — 이미 떠 있는 창은
  사용자가 옮겨둔 자리일 수 있어서 앞으로만 가져온다. 트레이 클릭과 트레이 메뉴 "열기"는
  예전처럼 아이콘 아래다(`toggle_popover`의 `from_tray`). 노치가 없으면 원래 자리 그대로다.
  새 창은 `.visible(false)`로 만들고 자리를 잡은 뒤 보여준다 — 먼저 보이면 기본 위치에서 한 번 깜빡인다
- **노치 표시는 기본으로 켠다** (`notch_enabled` 기본 true, 예전 설정 파일도 true로 읽음) —
  2026-11-09 NSPanel 게이트(아래 로드맵)를 통과해 포커스를 뺏지 않고 클릭도 통과하는 게
  확인됐다. 노치 없는 화면에서는 창 자체를 만들지 않아 켜 두어도 손해가 없다. 노치 숫자는
  잔여율이 경보 임계치 이하이거나 verdict가 `danger`면 위험 색(#ff453a, 검정 배경 대비
  6.2:1)으로 칠한다 — 트레이 숫자가 숨겨지면 그 자리가 노치뿐이라서다
- **노치 숫자만 대표 계정 폴백을 허용한다** (`Notch.tsx`의 `figureOf`) — 활성 계정이
  429 등으로 값을 못 받으면 값을 받은 다른 계정 중 가장 적게 남은 숫자를 흐리게 띄운다.
  노치는 한눈에 보는 자리라 "—"보다 낫고, 흐림으로 활성 값이 아님을 구분한다. 프론트
  표시에서만 하는 일이라 `representative`(카드 헤더·트레이·경보·이력·요금제 추천)는
  그대로 폴백이 없다. 펼치면 계정이 둘 이상인 도구는 계정별 줄을 함께 보여준다
- **완료 알림 클릭은 `tauri-plugin-notification`으로 못 받는다** — 데스크톱 구현이
  title/body/icon/sound만 넘기고 `action_type_id`를 버린다(`desktop.rs`의 `show()`).
  그래서 그 밑에 깔린 `mac-notification-sys`를 직접 쓴다(notify-rust 의존이라 crate가
  늘지 않는다). `wait_for_click(true)`면 버튼 없이도 클릭이 잡히는데 **응답까지
  블로킹**이라 알림마다 스레드가 묶인다. 사용자가 무시한 알림은 응답이 안 올 수
  있어 `MAX_WAITING`으로 동시 대기 수를 막고, 넘치면 클릭 대기 없이 띄운다
  (알림은 뜨고 랜딩은 알림함에서 하면 된다)
- **Dock 배지를 쓰려면 Accessory로 뜰 수 없다** — 배지는 `NSApp.dockTile`에 그려서
  (`tao/…/macos/badge.rs`) Dock에 아이콘이 없으면 호출은 성공하고 아무것도 안 보인다.
  그래서 `ActivationPolicy::Regular`로 상주한다. Accessory ↔ Regular를 오가는 방식은
  전환할 때마다 포커스가 흔들려서 쓰지 않는다. Dock 아이콘 클릭은 `RunEvent::Reopen`
  으로 받아 알림함을 연다 — 팝오버는 트레이 좌표에 붙어 Dock에서 열면 위치가 어긋난다
- **`trw`는 별도 crate여야 한다** — 앱과 같은 패키지에 두면 `cargo build --bin trw`가
  tauri-build 빌드 스크립트를 함께 돌리고, 그 스크립트가 아직 만들어지지 않은
  externalBin 바이너리(`binaries/trw-<트리플>`)를 요구해 순환에 걸린다.
  `scripts/build-trw.sh`가 그 파일을 만들고 `beforeDevCommand`·`beforeBuildCommand`
  양쪽에 걸려 있다 — dev 실행에서도 externalBin 파일이 있어야 한다
- **enum의 `serde(rename_all)`은 필드에 안 먹는다** — variant 이름만 바꾼다.
  `Target`처럼 tag가 붙은 enum을 camelCase로 주고받으려면 `rename_all_fields`를
  따로 붙여야 한다. 없으면 `worktreeId`가 조용히 파싱되지 않아 알림함이 빈다
- **스킬은 `~/.agents/skills/<이름>`에 실물을 두고 에이전트마다 상대 심링크를 건다**
  (`installer.rs`) — 커뮤니티 skills CLI 규약이다. 심링크의 상대 경로는 링크가 놓인
  디렉토리 기준이라 `~/.claude/skills/`에서 `../../.agents/skills/<이름>`이 맞다.
  홈에 설정 디렉토리가 있는 에이전트만 대상으로 삼는다 — 없는 에이전트까지 만들면
  쓰지도 않는 설정 디렉토리를 흩뿌린다. 자리에 심링크가 아닌 파일이 있으면 건드리지 않는다
- **`trw`는 절대 실패로 종료하지 않는다** — 에이전트 훅이나 스크립트 안에서 돈다.
  앱이 꺼져 있으면 `pending/`에 남기고 0으로 빠지며, 전달 실패를 알아야 할 때만
  `--strict`를 쓴다. 알림 payload에 셸 명령을 담지 않는 것도 같은 이유다 — 훅
  스크립트가 오염되면 그대로 실행기가 된다. `kind` 화이트리스트만 받고 명령은
  `landing.rs`가 조립한다
- **긴 도구 실행은 5분 컷만으로 못 버틴다** — `PreToolUse` 뒤 10분짜리 빌드가 돌면 그동안
  훅 이벤트가 없어 `STALE_MS`에 걸려 쉬는 중이 된다. 컷을 없애면 반대로 `kill`·터미널 닫기로
  죽은 세션(SessionEnd도 Stop도 안 온다)이 영원히 working으로 남는다. 그래서 `trw`가 상태에
  **세션 프로세스의 pid와 시작 시각**(`proc_pidinfo`)을 같이 남기고, 앱은 낡은 상태를 그
  프로세스가 살아 있을 때만 믿는다. 시작 시각까지 맞춰야 재사용된 pid에 속지 않는다.
  pid는 판정 창을 **늘리기만** 한다 — pid를 못 잡았거나 틀렸으면 예전 5분 규칙 그대로다.
  살아 있어도 낡은 working이 거짓일 수 있는 두 경우를 transcript로 거른다:
  Esc 중단(Stop이 안 온다 — transcript 끝 `[Request interrupted by user…]`로 idle),
  훅 없이 transcript만 자랐다가 그것도 5분 넘게 멈춘 경우(idle)
- **Claude 훅 stdout은 비워야 한다** — `UserPromptSubmit`·`SessionStart`의 stdout은
  Claude 컨텍스트에 들어가고 `Stop`의 stdout JSON은 `decision`으로 해석된다. `trw hook
  claude`는 아무것도 찍지 않고, 도구 입력·프롬프트는 상태 파일에 남기지 않는다(명령에
  시크릿이 섞일 수 있다). 네트워크·소켓도 안 쓴다 — 매 도구 호출마다 돌아서다
- **SessionEnd는 상태 파일을 지운다** — "끝남" 표시를 남겨도 목록에서 빼는 것 말고 쓸 데가
  없다. 지우지 못하고 남은 파일은 pid 확인에서 걸러지고, `trw`가 쓸 때마다 24시간 넘은
  파일을 치운다
- **훅 설치 기록은 태그로 나눈다** — Claude 훅과 statusLine은 같은 `settings.json`을 고쳐서
  `hook-installs.json` 키가 겹친다. Claude 훅은 `<경로>#claude-hooks`로 따로 남기고, 먼저
  설치한 쪽의 `created_file`·빈 객체 기록을 뒤에 설치한 쪽이 물려받는다
  (`record_or_inherited`). 그래야 어떤 순서로 넣고 빼도 마지막에 빠지는 쪽이 원래 파일을
  바이트 그대로 되돌린다(`coexists_with_statusline_in_every_order` 테스트). 우리 그룹은
  배열 끝에만 붙이고 matcher를 비운다 — 오르카나 사용자 훅의 순서·내용을 건드리지 않는다
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
- [x] Claude 계정 여러 개 동시 표시 — 계정마다 잔여율과 리셋 시각, 주간을 한 카드
  안에서 펼쳐 본다(`claude_accounts.rs`, `RunwayStatus.accounts`). 카드 헤더와 트레이는
  지금 쓰는 계정, 펼치면 계정별 줄. 소진 경보와 리셋 경보 둘 다 계정마다 따로 울고
  제목에 계정 이름이 붙는다
- [x] 작업 완료 알림 — `trw notify`로 다른 앱·세션에서 완료를 받아 OS 알림 + 알림함에
  쌓고, 누르면 그 오르카 워크트리 탭이나 Paseo 에이전트로 되돌아간다. Dock 배지에
  읽지 않은 건수, 트레이 메뉴·팝오버 벨에도 개수. `trw`와 스킬은 앱 번들에 담아
  설정에서 설치한다(`installer.rs`)
- [x] 노치 표시 NSPanel 게이트 — 2026-11-09 수동 체크리스트 네 항목 통과: (a) 섬을 눌러도
  앞 앱 포커스가 안 빠진다 (b) 메뉴바 위 노치 양옆에 붙는다 (c) 전체화면 앱 위에서 숨는다
  (d) 섬 밖과 아래 투명 영역은 클릭이 통과한다. Dock 배지도 그대로 보였다. 그래서 축소하지
  않고 메뉴바 위 섬으로 간다. 방식은 tauri-nspanel이 아니라 objc2 직접 호출이다 — tao 창
  클래스를 NSPanel 하위 클래스(`TRWNotchPanel`)로 바꿔치기한다. 이미 tao가 쓰는 crate라
  의존성이 늘지 않고, 바꿔치기 전에 인스턴스 크기를 비교해 tao가 ivar를 바꾸면 멈춘다
- [x] 노치 중심 UI — 노치 표시 기본 켬, 노치 섬이 주 화면에 뜨면 트레이 아이콘을 숨김
  (`tray_title_mode`로 자동/항상/아이콘만), 펼친 노치에 도구별 잔여율과 결론, 리셋까지
  남은 시간, 주간 잔여율, 계정별 줄과 대시보드·히스토리·알림함·설정 바로가기를 넣었다.
  세션 목록 칸(`.notch-sessions`)엔 `SessionList`가 들어가고 세션이 없으면 접힌다. 팝오버는 그대로 두고
  노치에서 열면 섬 오른쪽에 띄운다. 다른 창들도 같은 자리에 뜬다
- [ ] 플랜 배지 대비 — `.plan-badge`와 `.acct-plan`이 같은 파란 배경에 흰 글자라
  대비가 3.6:1이다. WCAG AA-large는 넘지만 normal(4.5:1)에는 못 미친다. 한쪽만
  어둡게 하면 두 배지가 달라 보여 함께 손봐야 한다
- [ ] `AccountRows` memo화 — 접힌 카드는 조기 반환으로 렌더 비용이 없어졌지만 펼친
  상태는 30초 폴링마다 계정 줄을 다시 그린다. 계정이 두세 개인 지금은 memo 비교
  비용이 리렌더 비용과 비슷해 실익이 없다. 계정이 늘면 그때 넣는다
- [ ] 사용률 이력에 계정 축 — 롤업이 도구명만 키로 써서 계정을 전환하면 같은 키에
  두 계정 이력이 이어 붙는다. 요금제 추천의 "최악의 주" 판정이 그만큼 어긋난다.
  히스토리 창 조회도 도구명 기준이라 키를 바꾸면 과거 이력이 끊긴다 — 마이그레이션 필요
- [x] pet 활동 감지 정확도 — Claude Code 훅(`trw hook claude`)이 세션별 상태 파일을
  남기고 설정 토글로 `settings.json`에 넣고 뺀다. 권한 확인 대기(`waiting`)까지 구분하고
  노치 펼친 화면에 세션 목록이 붙는다
- [ ] Antigravity 완료 시점 — 완료 이벤트가 로그에 없어 훅 말고는 방법이 없다
- [ ] 권한 승인 직후 상태 — 승인해도 다음 `PostToolUse`까지 waiting으로 남는다.
  `PermissionRequest` 훅을 쓰면 당겨질 수 있지만 그 훅은 결정을 돌려받는 자리라 붙이지 않았다
- [ ] 주간 ETA에 요일 가중 — `seven_day_eta_minutes`는 경과 시간 대비 선형 페이스라
  평일에 완만하고 주말에 몰아 쓰는 패턴에서 낙관적으로 어긋난다. 2026-08 실측(롤업):
  화 21% → 목 55% → 일 84% → 월 100%가 2주 연속(8/2, 8/10)이라, 목요일까지는 "리셋까지
  여유"로 보였는데 결국 소진했다. 롤업에 요일별 사용량이 이미 있으니 같은 요일의 과거
  소비 비중으로 남은 날을 추정하면 "지난 2주 패턴이면 일요일에 소진"까지 말할 수 있다.
- [ ] Cursor / Copilot provider — 데이터 소스 스펙 확보(위 참고), 검증 환경에서 구현 필요
- [ ] 비-macOS Keychain 지원 (`keyring` crate)
