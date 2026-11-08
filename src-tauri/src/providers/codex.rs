//! OpenAI Codex 사용량 수집기 (P1).
//!
//! 데이터 소스: `<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-*.jsonl` — 경로는 `homes`
//! `event_msg` 타입의 `token_count` 이벤트에서 턴별 토큰을 추출한다.
//!
//! 계정이 여럿이면 홈(`CODEX_HOME`)도 여럿이다. Claude와 달리 rollout 첫 줄
//! (`session_meta`)에 `creator_account_id`가 남아 있어서 시계열까지 계정별로 가를 수 있다.

mod app_server;

use super::{
    find_recent_jsonl, AccountUsage, OfficialUsage, SampleCache, UsageProvider, UsageSample,
};
use base64::Engine;
use chrono::DateTime;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// 계정별 최신 rate_limits를 찾으러 거슬러 올라가는 범위.
///
/// 가장 긴 윈도우가 주간이라 그보다 오래된 기록은 이미 리셋이 지난 값이다. 그래도
/// 안 쓰던 계정의 플랜과 "리셋됐다"는 사실은 알려줄 수 있어 넉넉히 잡는다.
const RATE_LIMIT_LOOKBACK_MS: i64 = 30 * 24 * 3600 * 1000;

/// rate_limits를 아직 못 읽었을 때 쓰는 윈도우.
const DEFAULT_WINDOW_SECS: i64 = 5 * 3600;

/// primary가 이 이상이면 주간 한도로 보고 7일 슬롯에도 같은 값을 넣는다.
const WEEKLY_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// 파싱 캐시 TTL. 대시보드 폴링과 통계 조회가 같은 결과를 재사용한다.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// 전체 계정 시계열 (히스토리 롤업용).
static SAMPLES_CACHE: SampleCache = SampleCache::new(CACHE_TTL);
/// 활성 계정 시계열 (카드 계산용).
static ACTIVE_SAMPLES_CACHE: SampleCache = SampleCache::new(CACHE_TTL);

/// 사용량 API. Codex CLI의 `/status`가 같은 경로를 부른다(바이너리 문자열로 확인).
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// API 캐시 TTL. 다른 provider의 OAuth 폴링 규칙(180초 미만 금지)을 그대로 따른다.
const API_TTL: Duration = Duration::from_secs(180);

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// 이 접두어가 없으면 서버가 Codex 클라이언트로 안 본다. 버전은 검증에 쓰이지 않는다.
const USER_AGENT: &str = "codex_cli_rs/0.166.0";

/// 계정 id → (조회 시각, rate_limits 또는 실패 사유 i18n 키).
type ApiResult = Result<RateLimits, &'static str>;
static API_CACHE: LazyLock<Mutex<HashMap<String, (Instant, ApiResult)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 계정 id → 마지막으로 app-server 폴백을 띄운 시각. 폴백도 실패하면 캐시엔 여전히
/// 토큰 만료가 남아서, 이게 없으면 폴링마다 자식 프로세스를 다시 띄운다.
static FALLBACK_AT: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 동시 갱신 방지. UI 폴링과 백그라운드 루프가 캐시 만료 시점에 겹쳐도 한 번만 쏜다.
static FETCH_GUARD: Mutex<()> = Mutex::new(());

/// 계정·rate_limits 스냅샷 캐시. 30일치 파일을 폴링마다 다시 훑지 않게 한다.
static SNAPSHOT_CACHE: Mutex<Option<(Instant, Snapshot)>> = Mutex::new(None);

pub struct CodexProvider;

impl CodexProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// rollout을 쌓는 Codex 홈 하나.
struct Home {
    sessions: PathBuf,
    auth: Option<AuthInfo>,
}

/// `auth.json`에서 읽은 계정 정보. 토큰 값은 들고 있지 않는다 — 캐시에 남지 않게
/// 요청 직전에 `home`에서 다시 읽는다.
#[derive(Clone)]
struct AuthInfo {
    account_id: String,
    email: Option<String>,
    plan: Option<String>,
    /// 토큰을 꺼낼 홈. API를 부르지 않는 계정(rollout에만 남은 계정)은 None.
    home: Option<PathBuf>,
    /// 같은 계정이 여러 홈에 있으면 가장 최근에 갱신된 쪽 토큰을 쓴다.
    last_refresh: Option<String>,
}

/// Codex 홈 후보 전부.
///
/// 오르카는 Codex 계정마다 `CODEX_HOME`을 `codex-accounts/<uuid>/home`으로 잡아 띄운다.
/// 그 세션은 `~/.codex`에 하나도 안 남아서, 홈만 읽으면 오르카에서 쓴 사용량과
/// rate_limits가 통째로 빠진다. 계정이 나중에 추가될 수 있어 부를 때마다 다시 훑는다.
pub(crate) fn home_dirs() -> Vec<PathBuf> {
    let mut dirs_: Vec<PathBuf> = Vec::new();
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        dirs_.push(PathBuf::from(h));
    }
    if let Some(home) = dirs::home_dir() {
        dirs_.push(home.join(".codex"));
        let orca = home.join("Library/Application Support/orca");
        if let Ok(entries) = fs::read_dir(orca.join("codex-accounts")) {
            for e in entries.flatten() {
                dirs_.push(e.path().join("home"));
            }
        }
        dirs_.push(orca.join("codex-runtime-home").join("home"));
    }
    let mut seen = HashSet::new();
    dirs_.retain(|d| d.is_dir() && seen.insert(d.clone()));
    dirs_
}

fn homes() -> Vec<Home> {
    home_dirs()
        .into_iter()
        .map(|d| Home {
            sessions: d.join("sessions"),
            auth: read_auth(&d),
        })
        .collect()
}

/// rollout 파일이 쌓이는 곳 전부. 활동 감지도 같은 목록을 쓴다.
pub fn session_roots() -> Vec<PathBuf> {
    home_dirs()
        .into_iter()
        .map(|d| d.join("sessions"))
        .filter(|s| s.is_dir())
        .collect()
}

/// 최근 rollout 파일과 그 홈의 계정. 같은 파일이 두 홈에 걸릴 일은 없다.
fn recent_rollouts(homes: &[Home], since_ms: i64) -> Vec<(PathBuf, Option<String>)> {
    homes
        .iter()
        .filter(|h| h.sessions.is_dir())
        .flat_map(|h| {
            let acct = h.auth.as_ref().map(|a| a.account_id.clone());
            find_recent_jsonl(&h.sessions, since_ms)
                .into_iter()
                .map(move |p| (p, acct.clone()))
        })
        .collect()
}

#[derive(Deserialize)]
struct AuthFile {
    tokens: Option<AuthTokens>,
    last_refresh: Option<String>,
}

#[derive(Deserialize)]
struct AuthTokens {
    account_id: Option<String>,
    id_token: Option<String>,
    access_token: Option<String>,
}

fn read_auth_file(home: &Path) -> Option<AuthFile> {
    serde_json::from_str(&fs::read_to_string(home.join("auth.json")).ok()?).ok()
}

/// `auth.json`에서 계정 id와 표시용 이메일, 플랜만 뽑는다.
///
/// 이메일과 플랜은 id_token(JWT) 본문에만 있다. 서명은 확인하지 않는다 — 표시용
/// 라벨이고, 인증에 쓰는 값이 아니다. API 키로 로그인한 홈은 계정이 없어 None.
fn read_auth(home: &Path) -> Option<AuthInfo> {
    let file = read_auth_file(home)?;
    let last_refresh = file.last_refresh;
    let tokens = file.tokens?;
    let claims = tokens.id_token.as_deref().and_then(jwt_claims);
    let auth_claims = claims
        .as_ref()
        .and_then(|c| c.get("https://api.openai.com/auth"));
    let account_id = tokens
        .account_id
        .filter(|s| !s.is_empty())
        .or_else(|| {
            auth_claims
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })?;
    Some(AuthInfo {
        account_id,
        email: claims
            .as_ref()
            .and_then(|c| c.get("email"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        plan: auth_claims
            .and_then(|a| a.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .map(plan_label),
        home: Some(home.to_path_buf()),
        last_refresh,
    })
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let body = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// rollout 첫 줄(`session_meta`)에 적힌 계정.
///
/// 홈의 `auth.json`은 계정을 바꾸면 덮어써져서, 예전 rollout을 지금 계정으로
/// 잘못 붙이게 된다. 파일에 적힌 값이 있으면 그걸 믿는다. 0.16x 이전 CLI는 이
/// 필드를 안 남겨서 그때만 홈 계정으로 간다.
fn creator_account(first_line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(first_line).ok()?;
    let p = v.get("payload")?;
    p.get("creator_account_id")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn file_account(path: &Path, home_account: Option<&String>) -> Option<String> {
    let first = BufReader::new(fs::File::open(path).ok()?).lines().next()?.ok()?;
    creator_account(&first).or_else(|| home_account.cloned())
}

/// 계정별 rate_limits 스냅샷.
#[derive(Clone, Default)]
struct Snapshot {
    accounts: Vec<AccountUsage>,
    /// 가장 최근에 Codex를 돌린 계정 — 카드 대표.
    active: Option<String>,
    /// 계정을 하나도 못 가린 환경(API 키 로그인 등)에서 쓰는 가장 신선한 값.
    latest: Option<(OfficialUsage, i64)>,
}

impl Snapshot {
    fn active_window(&self) -> i64 {
        let from_active = self
            .accounts
            .iter()
            .find(|a| a.is_active)
            .and_then(|a| a.window_secs);
        from_active
            .or(self.latest.as_ref().map(|(_, w)| *w))
            .unwrap_or(DEFAULT_WINDOW_SECS)
    }
}

#[derive(Deserialize)]
struct TranscriptLine {
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<Info>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct Info {
    /// 직전 턴의 토큰 사용량 (증분). 시계열 합산에 사용.
    last_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    /// Codex의 input_tokens는 cached_input_tokens를 포함한 입력 총량.
    #[serde(default)]
    input_tokens: u64,
}

/// Codex가 token_count 이벤트에 함께 기록하는 공식 rate limit 상태.
/// 사용량 API 응답도 이 모양으로 옮겨 같은 변환을 탄다.
#[derive(Deserialize, Clone)]
struct RateLimits {
    /// 짧은 쪽 윈도우. 길이는 계정마다 달라 `window_minutes`로 확인해야 한다 —
    /// 5시간(300)인 계정도, 주간(10080) 하나뿐인 계정도 있다.
    primary: Option<RateWindow>,
    /// 긴 쪽 윈도우. primary가 이미 주간이면 없다.
    secondary: Option<RateWindow>,
    /// 구독 등급 (예: "plus", "pro").
    plan_type: Option<String>,
    /// 남은 무료 리셋 횟수. app-server 응답에만 있어 rollout과 wham/usage 경로에선 None.
    #[serde(skip)]
    reset_credits: Option<u32>,
}

#[derive(Deserialize, Clone)]
struct RateWindow {
    #[serde(default)]
    used_percent: f64,
    /// epoch seconds.
    #[serde(default)]
    resets_at: i64,
    /// 윈도우 길이(분). 300=5시간, 10080=주간.
    window_minutes: Option<i64>,
}

impl UsageProvider for CodexProvider {
    fn tool_name(&self) -> &'static str {
        "Codex"
    }

    fn available(&self) -> bool {
        !session_roots().is_empty()
    }

    /// 활성 계정의 primary 윈도우 길이를 따른다 — 계정에 따라 5시간이거나 주간이다.
    /// 5시간으로 고정하면 주간 계정에서 윈도우가 리셋 직전 5시간(=미래)으로 잡혀
    /// 사용량이 0이 되고, 비활성 숨김에 걸려 카드까지 사라진다.
    fn window_secs(&self) -> i64 {
        snapshot().active_window()
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        if let Some(hit) = SAMPLES_CACHE.get(since_ms) {
            return hit;
        }
        let samples = collect(since_ms, None);
        SAMPLES_CACHE.put(since_ms, &samples);
        samples
    }

    fn collect_active_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let snap = snapshot();
        // 계정을 못 가린 환경이면 나눌 기준이 없다. 전부가 곧 활성이다.
        let Some(active) = snap.active.filter(|_| !snap.accounts.is_empty()) else {
            return self.collect_samples(since_ms);
        };
        if let Some(hit) = ACTIVE_SAMPLES_CACHE.get(since_ms) {
            return hit;
        }
        let samples = collect(since_ms, Some(&active));
        ACTIVE_SAMPLES_CACHE.put(since_ms, &samples);
        samples
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        snapshot().latest.map(|(u, _)| u)
    }

    fn accounts(&self) -> Vec<AccountUsage> {
        snapshot().accounts
    }

    fn status_note(&self) -> Option<String> {
        let snap = snapshot();
        if snap.accounts.is_empty() {
            return None;
        }
        match snap.accounts.iter().find(|a| a.is_active) {
            Some(a) if a.usage.is_some() => None,
            Some(a) => a.note.clone(),
            None => Some("error.active_unknown".to_string()),
        }
    }
}

/// rollout에서 턴별 토큰을 모은다. `account`가 있으면 그 계정 파일만.
fn collect(since_ms: i64, account: Option<&str>) -> Vec<UsageSample> {
    let mut samples = Vec::new();
    // 세션을 resume하면 이전 턴의 token_count가 새 rollout 파일에 다시 실린다.
    // Claude transcript와 달리 메시지 id가 없어 (시각, 토큰 조합)으로 구분한다 —
    // 밀리초 타임스탬프까지 같으면서 토큰 수도 같은 별개의 턴은 사실상 없다.
    let mut seen: HashSet<(i64, u64, u64, u64)> = HashSet::new();

    for (path, home_acct) in recent_rollouts(&homes(), since_ms) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(want) = account {
            let first = content.lines().next().unwrap_or("");
            let owner = creator_account(first).or(home_acct);
            if owner.as_deref() != Some(want) {
                continue;
            }
        }
        for line in content.lines() {
            let Some(sample) = parse_line(line, since_ms) else {
                continue;
            };
            let key = (
                sample.timestamp_ms,
                sample.amount,
                sample.input_total,
                sample.cache_read,
            );
            if seen.insert(key) {
                samples.push(sample);
            }
        }
    }

    samples.sort_by_key(|s| s.timestamp_ms);
    samples
}

/// 60초 캐시를 거친 스냅샷.
fn snapshot() -> Snapshot {
    if let Ok(cache) = SNAPSHOT_CACHE.lock() {
        if let Some((at, snap)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return snap.clone();
            }
        }
    }
    let fresh = scan_snapshot(chrono::Utc::now().timestamp_millis());
    if let Ok(mut cache) = SNAPSHOT_CACHE.lock() {
        *cache = Some((Instant::now(), fresh.clone()));
    }
    fresh
}

/// 계정마다 가장 최근 rollout의 rate_limits를 읽는다.
///
/// 최근 수정된 파일부터 보며 계정마다 처음 만난 값만 쓴다. 홈에 로그인된 계정이
/// 전부 채워지면 멈춘다 — 30일치를 매번 끝까지 읽을 필요가 없다.
fn scan_snapshot(now_ms: i64) -> Snapshot {
    let homes = homes();
    let mut known: Vec<AuthInfo> = Vec::new();
    for a in homes.iter().filter_map(|h| h.auth.clone()) {
        match known.iter_mut().find(|k| k.account_id == a.account_id) {
            Some(k) if a.last_refresh > k.last_refresh => *k = a,
            Some(_) => {}
            None => known.push(a),
        }
    }

    let mut files = recent_rollouts(&homes, now_ms - RATE_LIMIT_LOOKBACK_MS);
    let mtime = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    files.sort_by_key(|(p, _)| std::cmp::Reverse(mtime(p)));

    // 계정 → (기록 시각, rate_limits). 계정을 못 가린 파일은 None 키로 모은다.
    let mut found: HashMap<Option<String>, (i64, RateLimits)> = HashMap::new();
    for (path, home_acct) in files {
        let all_known_found = !known.is_empty()
            && known
                .iter()
                .all(|k| found.contains_key(&Some(k.account_id.clone())));
        if all_known_found {
            break;
        }
        let owner = file_account(&path, home_acct.as_ref());
        if found.contains_key(&owner) {
            continue;
        }
        if let Some(latest) = latest_rate_limits(&path) {
            found.insert(owner, latest);
        }
    }

    let active = found
        .iter()
        .max_by_key(|(_, (ts, _))| *ts)
        .and_then(|(acct, _)| acct.clone());

    let latest = found
        .values()
        .max_by_key(|(ts, _)| *ts)
        .and_then(|(_, rl)| to_usage(rl, now_ms));

    // 홈에 로그인은 없지만 rollout에 적힌 계정도 목록에 올린다 (auth.json을
    // 다른 계정으로 덮어쓴 경우).
    for acct in found.keys().flatten() {
        if !known.iter().any(|k| &k.account_id == acct) {
            known.push(AuthInfo {
                account_id: acct.clone(),
                email: None,
                plan: None,
                home: None,
                last_refresh: None,
            });
        }
    }

    if let Ok(_guard) = FETCH_GUARD.try_lock() {
        refresh_api(&known);
        refresh_app_server(
            &known,
            crate::settings::codex_app_server_fallback(),
            &|home| home_busy(home, now_ms),
            &|home| match app_server::find_codex_binary() {
                Some(bin) => app_server::read_rate_limits(&bin, home, app_server::APP_SERVER_TIMEOUT),
                None => Err("error.stale_token"),
            },
        );
    }

    let single = known.len() == 1;
    let accounts = known
        .into_iter()
        .map(|k| {
            // 서버 값이 먼저다. 다른 기기나 웹에서 쓴 분량까지 들어 있고, 오래 안 쓴
            // 계정도 지금 값을 준다. 실패하면 로컬 rollout의 마지막 기록으로 버틴다.
            let api = api_cached(&k.account_id);
            let local = found.get(&Some(k.account_id.clone())).map(|(_, rl)| rl);
            let source = api.as_ref().and_then(|r| r.as_ref().ok()).or(local);
            let usage = source.and_then(|rl| to_usage(rl, now_ms));
            let plan = source
                .and_then(|rl| rl.plan_type.as_deref().map(plan_label))
                .or(k.plan);
            let fail_note = match &api {
                Some(Err(key)) => key.to_string(),
                _ => "error.no_local_usage".to_string(),
            };
            let short: String = k.account_id.chars().take(8).collect();
            AccountUsage {
                is_active: single || active.as_deref() == Some(k.account_id.as_str()),
                label: k.email.unwrap_or(short),
                plan: plan.clone(),
                note: usage.is_none().then_some(fail_note),
                window_secs: usage.as_ref().map(|(_, w)| *w),
                reset_credits_available: source.and_then(|rl| rl.reset_credits),
                usage: usage.map(|(mut u, _)| {
                    u.plan = plan;
                    u
                }),
                id: k.account_id,
            }
        })
        .collect();

    Snapshot {
        accounts,
        active,
        latest,
    }
}

/// 캐시된 API 결과. 조회한 적이 없으면 None.
fn api_cached(account_id: &str) -> Option<ApiResult> {
    API_CACHE.lock().ok()?.get(account_id).map(|(_, r)| r.clone())
}

/// TTL이 지난 계정만 동시에 다시 조회한다.
///
/// 캐시 락은 HTTP 동안 잡지 않는다 — 응답이 늦으면 UI 폴링까지 매달린다.
fn refresh_api(accounts: &[AuthInfo]) {
    let stale: Vec<&AuthInfo> = {
        let Ok(cache) = API_CACHE.lock() else {
            return;
        };
        accounts
            .iter()
            .filter(|a| a.home.is_some())
            .filter(|a| {
                cache
                    .get(&a.account_id)
                    .is_none_or(|(at, _)| at.elapsed() >= API_TTL)
            })
            .collect()
    };
    if stale.is_empty() {
        return;
    }
    let results: Vec<(String, ApiResult)> = std::thread::scope(|scope| {
        let handles: Vec<_> = stale
            .iter()
            .map(|a| scope.spawn(move || (a.account_id.clone(), fetch_usage(a))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if let Ok(mut cache) = API_CACHE.lock() {
        let now = Instant::now();
        for (id, r) in results {
            cache.insert(id, (now, r));
        }
    }
}

/// 사용량 API가 토큰 만료를 돌려준 계정만 app-server로 다시 묻는다.
///
/// 성공했거나 rate limit, 일시 장애로 실패한 계정은 토큰 문제가 아니라 대상이 아니다.
/// 대상은 캐시 락을 잡은 동안 모으고, 자식 프로세스는 락 밖에서 계정마다 스레드로
/// 띄운다 — 순차로 띄우면 FETCH_GUARD를 계정 수 × 15초까지 쥐게 된다.
///
/// 세션이 도는 홈은 건너뛴다. 같은 홈에서 app-server가 토큰을 갱신하면 도는 세션이
/// 들고 있는 refresh token이 죽을 수 있는데, 실험으로 그렇지 않다는 걸 확인하지
/// 못했다(CLAUDE.md Codex 항목). 건너뛴 홈은 시도 기록을 남기지 않아 세션이 쉬면
/// 다음 폴링에서 바로 묻는다.
fn refresh_app_server(
    accounts: &[AuthInfo],
    enabled: bool,
    busy: &dyn Fn(&Path) -> bool,
    read: &(dyn Fn(&Path) -> ApiResult + Sync),
) {
    if !enabled {
        return;
    }
    let expired: Vec<&AuthInfo> = {
        let Ok(cache) = API_CACHE.lock() else {
            return;
        };
        accounts
            .iter()
            .filter(|a| a.home.is_some())
            .filter(|a| {
                matches!(
                    cache.get(&a.account_id),
                    Some((_, Err("error.stale_token")))
                )
            })
            .collect()
    };
    let targets: Vec<&AuthInfo> = {
        let Ok(mut tried) = FALLBACK_AT.lock() else {
            return;
        };
        let now = Instant::now();
        expired
            .into_iter()
            .filter(|a| !a.home.as_deref().is_some_and(busy))
            .filter(|a| {
                let due = tried
                    .get(&a.account_id)
                    .is_none_or(|at| at.elapsed() >= API_TTL);
                if due {
                    tried.insert(a.account_id.clone(), now);
                }
                due
            })
            .collect()
    };
    if targets.is_empty() {
        return;
    }
    let results: Vec<(String, ApiResult)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .filter_map(|a| Some((a.account_id.clone(), a.home.clone()?)))
            .map(|(id, home)| scope.spawn(move || (id, read(&home))))
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if let Ok(mut cache) = API_CACHE.lock() {
        let now = Instant::now();
        for (id, r) in results {
            cache.insert(id, (now, r));
        }
    }
}

/// activity가 "쉬는 중"으로 보는 기준과 같다 — 둘이 어긋나면 pet은 쉬는데
/// 폴백은 계속 건너뛰거나, 그 반대가 된다.
fn home_busy(home: &Path, now_ms: i64) -> bool {
    !find_recent_jsonl(&home.join("sessions"), now_ms - crate::activity::STALE_MS).is_empty()
}

#[derive(Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<ApiRateLimit>,
}

#[derive(Deserialize)]
struct ApiRateLimit {
    primary_window: Option<ApiWindow>,
    secondary_window: Option<ApiWindow>,
}

#[derive(Deserialize)]
struct ApiWindow {
    #[serde(default)]
    used_percent: f64,
    #[serde(default)]
    limit_window_seconds: i64,
    /// epoch seconds.
    #[serde(default)]
    reset_at: i64,
}

impl From<ApiWindow> for RateWindow {
    fn from(w: ApiWindow) -> Self {
        RateWindow {
            used_percent: w.used_percent,
            resets_at: w.reset_at,
            window_minutes: (w.limit_window_seconds > 0).then_some(w.limit_window_seconds / 60),
        }
    }
}

/// 계정 하나의 사용량을 서버에서 받는다.
///
/// 토큰이 만료돼도 여기서 갱신하지 않는다. Codex의 refresh token은 한 번 쓰면
/// 바뀌어서, 우리가 갱신하면 CLI가 들고 있는 토큰이 죽고 그 계정이 로그아웃된다.
fn fetch_usage(acc: &AuthInfo) -> ApiResult {
    let home = acc.home.as_deref().ok_or("error.no_token")?;
    let token = read_auth_file(home)
        .and_then(|f| f.tokens)
        .and_then(|t| t.access_token)
        .filter(|t| !t.is_empty())
        .ok_or("error.no_token")?;
    let resp = ureq::get(USAGE_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("ChatGPT-Account-Id", &acc.account_id)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/json")
        .call();
    let body: UsageResponse = match resp {
        Ok(r) => r.into_json().map_err(|_| "error.unavailable")?,
        Err(ureq::Error::Status(401 | 403, _)) => return Err("error.stale_token"),
        Err(ureq::Error::Status(429, _)) => return Err("error.rate_limit"),
        Err(_) => return Err("error.unavailable"),
    };
    let rl = body.rate_limit.ok_or("error.unavailable")?;
    Ok(RateLimits {
        primary: Some(rl.primary_window.ok_or("error.unavailable")?.into()),
        secondary: rl.secondary_window.map(Into::into),
        plan_type: body.plan_type,
        reset_credits: None,
    })
}

/// 파일 안에서 가장 늦게 기록된 rate_limits.
fn latest_rate_limits(path: &Path) -> Option<(i64, RateLimits)> {
    let content = fs::read_to_string(path).ok()?;
    let mut latest: Option<(i64, RateLimits)> = None;
    for line in content.lines() {
        if let Some((ts, rl)) = parse_rate_limits(line) {
            if latest.as_ref().is_none_or(|(t, _)| ts > *t) {
                latest = Some((ts, rl));
            }
        }
    }
    latest
}

/// rate_limits → (공식 사용률, 윈도우 초).
///
/// 리셋 시각이 이미 지난 윈도우는 사용률 0, 리셋 시각 미상으로 둔다. 오래 안 쓴
/// 계정의 마지막 기록이 "64% 썼음"으로 남아 있어도 지금은 한도가 다시 찼다.
/// 다른 기기에서 그 뒤에 썼다면 실제와 다를 수 있다 — 로컬 기록으로는 알 수 없다.
fn to_usage(rl: &RateLimits, now_ms: i64) -> Option<(OfficialUsage, i64)> {
    let primary = rl.primary.as_ref()?;
    let window_secs = primary
        .window_minutes
        .filter(|m| *m > 0)
        .map_or(DEFAULT_WINDOW_SECS, |m| m * 60);
    let live = |w: &RateWindow| -> (f64, String) {
        if w.resets_at * 1000 > now_ms {
            (w.used_percent, epoch_to_rfc3339(w.resets_at))
        } else {
            (0.0, String::new())
        }
    };
    let (p_util, p_reset) = live(primary);

    // secondary가 없는데 primary가 이미 주간이면 그게 유일한 한도다 —
    // 주간 슬롯을 0으로 두면 "주간 100% 남음"이라는 거짓 여유가 표시된다.
    // 양쪽에 같은 값을 넣고, UI는 윈도우가 7일 이상이면 주간 줄을 숨긴다.
    let weekly_only = rl.secondary.is_none() && window_secs >= WEEKLY_WINDOW_SECS;
    let (s_util, s_reset) = match (&rl.secondary, weekly_only) {
        (Some(s), _) => live(s),
        (None, true) => (p_util, p_reset.clone()),
        (None, false) => (0.0, String::new()),
    };
    let usage = OfficialUsage {
        five_hour_utilization: p_util,
        five_hour_resets_at: p_reset,
        seven_day_utilization: s_util,
        seven_day_resets_at: s_reset,
        plan: rl.plan_type.as_deref().map(plan_label),
        rate_limit_multiplier: None, // Codex는 소비자 사다리 배수 미상.
        is_estimate: false,          // Codex가 직접 기록한 공식 사용률.
    };
    Some((usage, window_secs))
}

/// 플랜 코드를 배지 문구로 ("plus" → "Plus", "self_serve_business_prolite" →
/// "Business Pro Lite"). `self_serve_`는 결제 경로라 배지에선 뺀다.
fn plan_label(code: &str) -> String {
    let code = code.strip_prefix("self_serve_").unwrap_or(code);
    code.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "prolite" => "Pro Lite".to_string(),
            _ => {
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// epoch seconds → RFC3339 문자열.
fn epoch_to_rfc3339(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// token_count 이벤트 줄에서 (timestamp_ms, rate_limits)를 추출.
fn parse_rate_limits(line: &str) -> Option<(i64, RateLimits)> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let payload = parsed.payload?;
    if payload.kind.as_deref() != Some("token_count") {
        return None;
    }
    let rl = payload.rate_limits?;
    let ts = parsed.timestamp?;
    let ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    Some((ms, rl))
}

/// `token_count` 이벤트 줄만 골라 턴 증분 토큰을 샘플로 변환.
fn parse_line(line: &str, since_ms: i64) -> Option<UsageSample> {
    let parsed: TranscriptLine = serde_json::from_str(line).ok()?;
    let payload = parsed.payload?;
    if payload.kind.as_deref() != Some("token_count") {
        return None;
    }
    let usage = payload.info?.last_token_usage?;
    let ts = parsed.timestamp?;
    let timestamp_ms = DateTime::parse_from_rfc3339(&ts).ok()?.timestamp_millis();
    if timestamp_ms < since_ms {
        return None;
    }
    Some(UsageSample {
        timestamp_ms,
        amount: usage.total_tokens,
        cost_usd: 0.0, // OpenAI 단가 매핑 불확실 — 추후
        cache_read: usage.cached_input_tokens,
        cache_write: 0,
        input_total: usage.input_tokens, // cached 포함된 입력 총량
        model: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실제 홈을 읽어 경로와 사용률이 잡히는지 본다. `cargo test -- --ignored`로 돌린다.
    #[test]
    fn plan_labels() {
        assert_eq!(plan_label("plus"), "Plus");
        assert_eq!(plan_label("self_serve_business_prolite"), "Business Pro Lite");
    }

    #[test]
    fn expired_window_reads_as_full() {
        let rl = RateLimits {
            primary: Some(RateWindow {
                used_percent: 64.0,
                resets_at: 1_000,
                window_minutes: Some(300),
            }),
            secondary: None,
            plan_type: Some("plus".into()),
            reset_credits: None,
        };
        let (u, w) = to_usage(&rl, 2_000_000).unwrap();
        assert_eq!(w, 5 * 3600);
        assert_eq!(u.five_hour_utilization, 0.0);
        assert!(u.five_hour_resets_at.is_empty());
    }

    fn acct(id: &str) -> AuthInfo {
        AuthInfo {
            account_id: id.to_string(),
            email: None,
            plan: None,
            home: Some(PathBuf::from(format!("/homes/{id}"))),
            last_refresh: None,
        }
    }

    fn sample_limits() -> RateLimits {
        RateLimits {
            primary: Some(RateWindow {
                used_percent: 10.0,
                resets_at: 4_000_000_000,
                window_minutes: Some(300),
            }),
            secondary: None,
            plan_type: Some("plus".into()),
            reset_credits: Some(1),
        }
    }

    /// 테스트끼리 전역 캐시를 같이 쓰므로 계정 id를 테스트마다 다르게 잡는다.
    fn seed(id: &str, r: ApiResult) {
        API_CACHE
            .lock()
            .unwrap()
            .insert(id.to_string(), (Instant::now(), r));
    }

    fn counting(
        count: &std::sync::atomic::AtomicUsize,
        result: ApiResult,
    ) -> impl Fn(&Path) -> ApiResult + Sync + '_ {
        move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            result.clone()
        }
    }

    #[test]
    fn healthy_or_transient_accounts_never_spawn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        seed("fb-ok", Ok(sample_limits()));
        seed("fb-429", Err("error.rate_limit"));
        seed("fb-5xx", Err("error.unavailable"));
        let accounts = [acct("fb-ok"), acct("fb-429"), acct("fb-5xx"), acct("fb-unseen")];
        let count = AtomicUsize::new(0);
        refresh_app_server(&accounts, true, &|_| false, &counting(&count, Ok(sample_limits())));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(matches!(api_cached("fb-ok"), Some(Ok(_))));
        assert_eq!(api_cached("fb-429").unwrap().err(), Some("error.rate_limit"));
    }

    #[test]
    fn expired_account_is_filled_and_cached() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        seed("fb-401", Err("error.stale_token"));
        let accounts = [acct("fb-401")];
        let count = AtomicUsize::new(0);
        let read = counting(&count, Ok(sample_limits()));
        refresh_app_server(&accounts, true, &|_| false, &read);
        refresh_app_server(&accounts, true, &|_| false, &read);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let rl = api_cached("fb-401").unwrap().unwrap();
        assert_eq!(rl.reset_credits, Some(1));
    }

    #[test]
    fn failed_fallback_is_not_retried_within_ttl() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        seed("fb-401-again", Err("error.stale_token"));
        let accounts = [acct("fb-401-again")];
        let count = AtomicUsize::new(0);
        let read = counting(&count, Err("error.stale_token"));
        refresh_app_server(&accounts, true, &|_| false, &read);
        refresh_app_server(&accounts, true, &|_| false, &read);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(api_cached("fb-401-again").unwrap().err(), Some("error.stale_token"));
    }

    #[test]
    fn missing_binary_leaves_stale_token() {
        seed("fb-nobin", Err("error.stale_token"));
        refresh_app_server(&[acct("fb-nobin")], true, &|_| false, &|_| Err("error.stale_token"));
        assert_eq!(api_cached("fb-nobin").unwrap().err(), Some("error.stale_token"));
    }

    #[test]
    fn toggle_off_skips_everything() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        seed("fb-off", Err("error.stale_token"));
        let count = AtomicUsize::new(0);
        refresh_app_server(&[acct("fb-off")], false, &|_| false, &counting(&count, Ok(sample_limits())));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert_eq!(api_cached("fb-off").unwrap().err(), Some("error.stale_token"));
    }

    #[test]
    fn busy_home_is_skipped_and_retried_once_idle() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        seed("fb-busy", Err("error.stale_token"));
        seed("fb-idle", Err("error.stale_token"));
        let accounts = [acct("fb-busy"), acct("fb-idle")];
        let count = AtomicUsize::new(0);
        let read = counting(&count, Ok(sample_limits()));
        let busy = |home: &Path| home.ends_with("fb-busy");
        refresh_app_server(&accounts, true, &busy, &read);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(api_cached("fb-busy").unwrap().err(), Some("error.stale_token"));
        assert!(matches!(api_cached("fb-idle"), Some(Ok(_))));
        refresh_app_server(&accounts, true, &|_| false, &read);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(matches!(api_cached("fb-busy"), Some(Ok(_))));
    }

    #[test]
    fn home_busy_follows_recent_rollouts() {
        let home = std::env::temp_dir().join(format!("trw-codex-busy-{}", std::process::id()));
        let day = home.join("sessions/2026/11/08");
        fs::create_dir_all(&day).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        assert!(!home_busy(&home.join("nowhere"), now));
        assert!(!home_busy(&home, now));
        fs::write(day.join("rollout-x.jsonl"), "{}\n").unwrap();
        assert!(home_busy(&home, now));
        assert!(!home_busy(&home, now + crate::activity::STALE_MS + 60_000));
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn slow_fallback_does_not_hold_cache_lock_and_runs_in_parallel() {
        seed("fb-slow-a", Err("error.stale_token"));
        seed("fb-slow-b", Err("error.stale_token"));
        seed("fb-probe", Ok(sample_limits()));
        let accounts = [acct("fb-slow-a"), acct("fb-slow-b")];
        let start = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                refresh_app_server(&accounts, true, &|_| false, &|_| {
                    std::thread::sleep(Duration::from_millis(600));
                    Ok(sample_limits())
                })
            });
            std::thread::sleep(Duration::from_millis(150));
            let probe = Instant::now();
            assert!(api_cached("fb-probe").is_some());
            assert!(probe.elapsed() < Duration::from_millis(100), "{:?}", probe.elapsed());
        });
        // 둘을 순차로 띄웠다면 1.2초를 넘는다.
        assert!(start.elapsed() < Duration::from_millis(1100), "{:?}", start.elapsed());
        assert!(matches!(api_cached("fb-slow-b"), Some(Ok(_))));
    }

    /// 실제 값과 겹칠 일 없는 가짜 토큰.
    const FAKE_ACCESS: &str = "fake-access-7f3a9c";
    const FAKE_REFRESH: &str = "fake-refresh-1b2d4e";

    fn fake_home(name: &str, with_access: bool) -> PathBuf {
        let home = std::env::temp_dir().join(format!("trw-codex-secret-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let claims = serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {"chatgpt_plan_type": "plus", "chatgpt_account_id": "acct-fake"}
        });
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        let mut tokens = serde_json::json!({
            "account_id": "acct-fake",
            "id_token": format!("e30.{body}.sig"),
            "refresh_token": FAKE_REFRESH,
        });
        if with_access {
            tokens["access_token"] = serde_json::json!(FAKE_ACCESS);
        }
        let auth = serde_json::json!({"tokens": tokens, "last_refresh": "2026-11-01T00:00:00Z"});
        fs::write(home.join("auth.json"), auth.to_string()).unwrap();
        home
    }

    #[test]
    fn auth_info_and_notes_hold_no_token() {
        use crate::providers::secret_check::assert_no_secret;
        let secrets = [FAKE_ACCESS, FAKE_REFRESH];
        let home = fake_home("info", true);
        let info = read_auth(&home).unwrap();
        let shown = format!(
            "{} {:?} {:?} {:?} {:?}",
            info.account_id, info.email, info.plan, info.home, info.last_refresh
        );
        assert_no_secret("AuthInfo", &shown, &secrets);
        assert_eq!(info.email.as_deref(), Some("dev@example.com"));

        let no_token = fake_home("notoken", false);
        let mut acc = read_auth(&no_token).unwrap();
        acc.home = Some(no_token.clone());
        let r = fetch_usage(&acc);
        assert_eq!(r.as_ref().err(), Some(&"error.no_token"));
        assert_no_secret("fetch 실패 노트", &format!("{:?}", r.err()), &secrets);
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&no_token);
    }

    #[test]
    fn app_server_error_note_drops_server_message() {
        use crate::providers::secret_check::assert_no_secret;
        use std::os::unix::fs::PermissionsExt;
        let home = fake_home("appserver", true);
        let bin = home.join("codex");
        // 서버가 에러 메시지에 토큰을 실어 보내도 노트에는 i18n 키만 남아야 한다.
        let script = format!(
            "#!/bin/sh\necho 'Authorization: Bearer {FAKE_ACCESS}' >&2\nread l; echo '{{\"id\":1,\"result\":{{}}}}'\nread l; read l\n\
             echo '{{\"id\":2,\"error\":{{\"message\":\"401 Bearer {FAKE_ACCESS} refresh {FAKE_REFRESH}\"}}}}'\n"
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let r = app_server::read_rate_limits(&bin, &home, Duration::from_secs(5));
        let note = r.as_ref().err().copied().unwrap();
        assert_eq!(note, "error.stale_token");
        assert_no_secret("app-server 노트", note, &[FAKE_ACCESS, FAKE_REFRESH]);
        assert_no_secret("app-server 로그", &format!("{:?}", r.err()), &[FAKE_ACCESS, FAKE_REFRESH]);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    #[ignore]
    fn real_home() {
        let p = CodexProvider::new();
        println!("roots={:?}", session_roots());
        let since0 = chrono::Utc::now().timestamp_millis() - 24 * 3600 * 1000;
        println!("samples_24h={}", p.collect_samples(since0).len());
        println!("window_secs={} usage={:?}", p.window_secs(), p.official_usage());
        for a in p.accounts() {
            println!(
                "account {} active={} plan={:?} usage={:?} note={:?}",
                a.label, a.is_active, a.plan, a.usage.map(|u| u.five_hour_utilization), a.note
            );
        }
        let since = chrono::Utc::now().timestamp_millis() - 24 * 3600 * 1000;
        println!("active_samples_24h={}", p.collect_active_samples(since).len());
    }
}
