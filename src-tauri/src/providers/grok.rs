//! xAI Grok Build 사용량 수집기 (best-effort · 스펙 기반).
//!
//! 데이터 소스: `~/.grok/sessions/<workspace>/<session-id>/updates.jsonl`
//! (JSON-RPC `session/update` 스트림, NDJSON). `GROK_HOME` 환경변수로 루트 override.
//!
//! - **시계열**: 각 업데이트의 `_meta.totalTokens`는 세션 안에서 단조 증가하는
//!   카운터다. 사용자 메시지(`sessionUpdate == "user_message_chunk"`)를 턴 경계로 잡고,
//!   턴 안에서 관측한 최댓값 − 턴 시작 직전 값 = 그 턴의 증분을 샘플 1건으로 만든다.
//!   값이 줄거나(압축·되감기) 반복되면 무시해 단조 증가로 취급한다.
//! - **모델**: 업데이트의 `modelId`, 없으면 형제 `summary.json`의 `current_model_id`.
//!
//! ⚠️ `totalTokens`는 소비 누적이 아니라 **컨텍스트 점유량**으로 보인다 — 실측에서
//! 관측 최댓값이 형제 `signals.json`의 `contextTokensUsed`와 정확히 일치했다.
//! 그렇다면 턴 증분은 새로 쌓인 컨텍스트량이지 실제 API 청구 토큰이 아니다(매 턴
//! 전체 컨텍스트를 다시 보내므로 실제 소비는 더 크다). Claude의 `message.usage`와
//! 의미가 달라 도구 간 절대 비교는 하지 말 것. 압축이 일어난 구간의 소비도 누락된다.
//!
//! **잔여율**: 로컬 파일엔 쿼터가 없지만 CLI가 서버에서 받아온다 —
//! `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits`.
//! `creditUsagePercent`(크레딧 소진율)와 `currentPeriod`(주기 시작·끝)를 쓴다.
//! 크레딧은 통합 지갑이라 Imagine·Chat이 쓴 몫까지 포함한다 — 코딩만 따로 보려면
//! `productUsage[].GrokBuild`가 있지만, 바닥나면 코딩도 못 쓰므로 전체를 잔여율로 쓴다.
//! 주기는 `type` 문자열이 아니라 `end - start`로 재므로 종류가 늘어도 맞는다.

use super::{find_recent_jsonl, OfficialUsage, UsageProvider, UsageSample};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const UNKNOWN_MODEL: &str = "grok-unknown";

/// 크레딧 조회 엔드포인트 (CLI가 `/usage`에 쓰는 것과 같다).
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

/// 폴링 최소 간격. Claude와 같은 보수적 값.
const BILLING_CACHE_TTL: Duration = Duration::from_secs(180);

/// HTTP 타임아웃 — 응답이 멈춰도 폴링 스레드가 매달리지 않게.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// 주기를 못 읽었을 때의 폴백 (관측된 실제 주기는 7일).
const DEFAULT_PERIOD_SECS: i64 = 7 * 24 * 3600;

struct CachedBilling {
    fetched_at: Instant,
    usage: Option<OfficialUsage>,
    /// 주기 길이(초) — 윈도우가 API 응답에 달려 있어 함께 캐시한다.
    window_secs: i64,
    error: Option<&'static str>,
}

static BILLING_CACHE: Mutex<Option<CachedBilling>> = Mutex::new(None);

/// 갱신 중복 방지. 데이터 락을 잡은 채 네트워크를 타지 않는다.
static FETCH_GUARD: Mutex<()> = Mutex::new(());

pub struct GrokProvider {
    root: PathBuf,
    sessions_dir: PathBuf,
}

impl GrokProvider {
    pub fn new() -> Self {
        // GROK_HOME > ~/.grok. sessions 하위에 세션별 디렉토리가 쌓인다.
        let root = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".grok")))
            .unwrap_or_default();
        Self {
            sessions_dir: root.join("sessions"),
            root,
        }
    }
}

impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageProvider for GrokProvider {
    fn tool_name(&self) -> &'static str {
        "Grok"
    }

    /// 크레딧 주기 길이. API가 주는 `currentPeriod`의 end−start를 쓰고,
    /// 아직 못 받았으면 관측된 기본값(7일)을 쓴다.
    fn window_secs(&self) -> i64 {
        self.billing().2
    }

    fn available(&self) -> bool {
        self.sessions_dir.is_dir()
    }

    fn official_usage(&self) -> Option<OfficialUsage> {
        self.billing().0
    }

    fn status_note(&self) -> Option<String> {
        self.billing().1.map(|s| s.to_string())
    }

    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample> {
        let mut samples = Vec::new();

        for path in find_recent_jsonl(&self.sessions_dir, since_ms) {
            // 세션 디렉토리엔 events.jsonl 등 다른 jsonl도 있다 — updates만.
            if path.file_name().and_then(|n| n.to_str()) != Some("updates.jsonl") {
                continue;
            }
            parse_session_file(&path, since_ms, &mut samples);
        }

        samples.sort_by_key(|s| s.timestamp_ms);
        samples
    }
}

impl GrokProvider {
    /// (사용률, 실패사유, 윈도우 초) — 180초 캐시.
    ///
    /// 네트워크 호출 동안 데이터 락을 잡지 않는다. 다른 스레드가 갱신 중이면
    /// 기다리지 않고 직전 값을 쓴다(Claude provider와 같은 규칙).
    fn billing(&self) -> (Option<OfficialUsage>, Option<&'static str>, i64) {
        if let Some(fresh) = cached(true) {
            return fresh;
        }
        let Ok(_guard) = FETCH_GUARD.try_lock() else {
            return cached(false).unwrap_or((None, None, DEFAULT_PERIOD_SECS));
        };
        if let Some(fresh) = cached(true) {
            return fresh;
        }

        let (usage, error, window_secs) = fetch_billing(&self.root);
        if let Ok(mut cache) = BILLING_CACHE.lock() {
            *cache = Some(CachedBilling {
                fetched_at: Instant::now(),
                usage: usage.clone(),
                window_secs,
                error,
            });
        }
        (usage, error, window_secs)
    }
}

fn cached(require_fresh: bool) -> Option<(Option<OfficialUsage>, Option<&'static str>, i64)> {
    let guard = BILLING_CACHE.lock().ok()?;
    let c = guard.as_ref()?;
    if require_fresh && c.fetched_at.elapsed() >= BILLING_CACHE_TTL {
        return None;
    }
    Some((c.usage.clone(), c.error, c.window_secs))
}

/// `auth.json`에서 액세스 토큰을 읽는다. 만료됐으면 사유를 돌려준다.
fn read_token(root: &Path) -> Result<String, &'static str> {
    let raw = fs::read_to_string(root.join("auth.json")).map_err(|_| "error.no_token")?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| "error.no_token")?;
    // 최상위는 `<issuer>::<uuid>` → 엔트리 맵. 만료되지 않은 것을 고른다.
    let entries = v.as_object().ok_or("error.no_token")?;
    let mut expired = false;
    for entry in entries.values() {
        let Some(key) = entry.get("key").and_then(|k| k.as_str()) else {
            continue;
        };
        if let Some(exp) = entry.get("expires_at").and_then(|e| e.as_str()) {
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(exp) {
                if t.timestamp_millis() <= chrono::Utc::now().timestamp_millis() {
                    expired = true;
                    continue;
                }
            }
        }
        return Ok(key.to_string());
    }
    Err(if expired {
        "error.expired"
    } else {
        "error.no_token"
    })
}

/// billing API 호출 → (사용률, 실패사유, 윈도우 초).
fn fetch_billing(root: &Path) -> (Option<OfficialUsage>, Option<&'static str>, i64) {
    let token = match read_token(root) {
        Ok(t) => t,
        Err(e) => return (None, Some(e), DEFAULT_PERIOD_SECS),
    };
    let version = fs::read_to_string(root.join(".metadata_version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "0.0.0".to_string());

    let resp = ureq::get(BILLING_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("x-grok-client-identifier", "grok-shell")
        .set("x-grok-client-mode", "billing")
        .set("x-grok-client-version", &version)
        .set("Accept", "application/json")
        .call();

    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return (None, Some("error.expired"), DEFAULT_PERIOD_SECS)
        }
        Err(ureq::Error::Status(429, _)) => {
            return (None, Some("error.rate_limit"), DEFAULT_PERIOD_SECS)
        }
        Err(_) => return (None, Some("error.unavailable"), DEFAULT_PERIOD_SECS),
    };

    let Ok(body) = resp.into_json::<Value>() else {
        return (None, Some("error.unavailable"), DEFAULT_PERIOD_SECS);
    };
    parse_billing(&body)
}

/// billing 응답 → (사용률, 실패사유, 윈도우 초).
///
/// 주기는 타입 문자열(`USAGE_PERIOD_TYPE_WEEKLY` 등)에 기대지 않고 end−start로
/// 직접 잰다 — 주기 종류가 늘어도 그대로 맞는다.
fn parse_billing(body: &Value) -> (Option<OfficialUsage>, Option<&'static str>, i64) {
    let cfg = body.get("config").unwrap_or(body);
    let Some(util) = cfg.get("creditUsagePercent").and_then(|v| v.as_f64()) else {
        return (None, Some("error.unavailable"), DEFAULT_PERIOD_SECS);
    };

    let period = cfg.get("currentPeriod");
    let start = period
        .and_then(|p| p.get("start"))
        .and_then(|s| s.as_str())
        .and_then(parse_ms);
    let end = period
        .and_then(|p| p.get("end"))
        .and_then(|s| s.as_str())
        .and_then(parse_ms)
        .or_else(|| {
            cfg.get("billingPeriodEnd")
                .and_then(|s| s.as_str())
                .and_then(parse_ms)
        });

    let window_secs = match (start, end) {
        (Some(s), Some(e)) if e > s => (e - s) / 1000,
        _ => DEFAULT_PERIOD_SECS,
    };
    let resets_at = end
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();

    // Grok은 단기(5시간) 윈도우가 없다 — 크레딧 주기 하나뿐이라 양쪽에 같은 값을 넣는다.
    // percent_remaining이 주 표시가 되고, 주간 일별 분해도 같은 주기로 그려진다.
    (
        Some(OfficialUsage {
            five_hour_utilization: util,
            five_hour_resets_at: resets_at.clone(),
            seven_day_utilization: util,
            seven_day_resets_at: resets_at,
            plan: cfg
                .get("subscription_tier")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            rate_limit_multiplier: None,
            is_estimate: false,
        }),
        None,
        window_secs,
    )
}

fn parse_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// 한 세션의 `updates.jsonl`을 파싱해 턴별 증분 샘플을 `out`에 추가한다.
///
/// `totalTokens`는 누적이므로 파일 전체를 순회하며 상태를 유지한 뒤,
/// `since_ms` 이후 타임스탬프의 턴만 샘플로 내보낸다.
fn parse_session_file(path: &Path, since_ms: i64, out: &mut Vec<UsageSample>) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };

    // 업데이트에 모델·타임스탬프가 없을 때의 폴백.
    let file_mtime = file_mtime_ms(path);
    let mut current_model = summary_model(path).unwrap_or_else(|| UNKNOWN_MODEL.to_string());

    let mut last_total: Option<i64> = None;
    let mut turn: Option<Turn> = None;
    // 첫 사용자 메시지 이전 구간은 세션 셋업분이다. 턴으로 세면 첫 턴 소비량이
    // 두 번 잡힌다 — baseline으로만 쓴다. 단 사용자 메시지가 하나도 없는 파일에서는
    // 그게 유일한 사용량이므로 그대로 내보낸다.
    let mut preamble = true;
    let push_turn = |t: Option<Turn>, out: &mut Vec<UsageSample>| {
        if let Some(t) = t {
            if let Some(sample) = t.into_sample(since_ms) {
                out.push(sample);
            }
        }
    };

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if let Some(m) = extract_model_id(&v) {
            current_model = m;
            if let Some(t) = turn.as_mut() {
                if t.model == UNKNOWN_MODEL {
                    t.model = current_model.clone();
                }
            }
        }

        let ts = extract_timestamp_ms(&v).unwrap_or(file_mtime);

        // 사용자 메시지 = 새 턴 시작. 직전 턴을 마감한다.
        if is_user_message(&v) {
            if preamble {
                preamble = false;
                turn = None;
            }
            push_turn(turn.take(), out);
            turn = Some(Turn::new(last_total.unwrap_or(0), ts, current_model.clone()));
        }

        let Some(total) = extract_total_tokens(&v) else {
            continue;
        };
        if total < 0 {
            continue;
        }

        match last_total {
            // 누적값이 줄거나(압축·되감기) 그대로면 단조 증가로 취급해 무시.
            Some(prev) if total <= prev => continue,
            _ => {}
        }
        // 사용자 메시지 없이 토큰이 늘면(첫 턴 등) 턴을 만들어준다.
        if turn.is_none() {
            turn = Some(Turn::new(last_total.unwrap_or(0), ts, current_model.clone()));
        }
        if let Some(t) = turn.as_mut() {
            t.observe(total, ts);
        }
        last_total = Some(total);
    }

    push_turn(turn.take(), out);
}

/// 진행 중인 한 턴 — 시작 직전 누적값과 관측한 최댓값의 차가 그 턴의 소비량.
struct Turn {
    baseline: i64,
    max_total: i64,
    timestamp: i64,
    model: String,
}

impl Turn {
    fn new(baseline: i64, timestamp: i64, model: String) -> Self {
        Self {
            baseline,
            max_total: baseline,
            timestamp,
            model,
        }
    }

    fn observe(&mut self, total: i64, timestamp: i64) {
        if total > self.max_total {
            self.max_total = total;
            self.timestamp = timestamp;
        }
    }

    fn into_sample(self, since_ms: i64) -> Option<UsageSample> {
        let delta = self.max_total.saturating_sub(self.baseline);
        if delta <= 0 || self.timestamp < since_ms {
            return None;
        }
        let model = if self.model.trim().is_empty() {
            UNKNOWN_MODEL.to_string()
        } else {
            self.model
        };
        Some(UsageSample {
            timestamp_ms: self.timestamp,
            amount: delta as u64,
            cost_usd: 0.0, // xAI 단가/크레딧 환산 미상 — 추후
            cache_read: 0,
            cache_write: 0,
            // Grok은 입력/출력·캐시 분해를 로컬에 남기지 않음 → 캐시 지표 미표시.
            input_total: 0,
            model: Some(model),
        })
    }
}

/// 형제 `summary.json`의 모델명 (업데이트에 modelId가 없을 때의 폴백).
fn summary_model(updates_path: &Path) -> Option<String> {
    let path = updates_path.parent()?.join("summary.json");
    let data = fs::read(&path).ok()?;
    let v: Value = serde_json::from_slice(&data).ok()?;
    string_at(&v, &["current_model_id"]).or_else(|| string_at(&v, &["model_id"]))
}

/// 파일 수정 시각(epoch ms). 타임스탬프 폴백용. 못 읽으면 0.
fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `params.update.sessionUpdate == "user_message_chunk"` 인지.
fn is_user_message(v: &Value) -> bool {
    string_at(v, &["params", "update", "sessionUpdate"]).as_deref() == Some("user_message_chunk")
}

fn extract_model_id(v: &Value) -> Option<String> {
    const PATHS: [&[&str]; 6] = [
        &["params", "update", "_meta", "modelId"],
        &["params", "_meta", "modelId"],
        &["params", "modelId"],
        &["model_id"],
        &["modelId"],
        &["model"],
    ];
    PATHS
        .iter()
        .find_map(|p| string_at(v, p))
        .filter(|s| !s.trim().is_empty())
}

fn extract_total_tokens(v: &Value) -> Option<i64> {
    const PATHS: [&[&str]; 6] = [
        &["params", "_meta", "totalTokens"],
        &["params", "update", "_meta", "totalTokens"],
        &["params", "update", "totalTokens"],
        &["params", "totalTokens"],
        &["usage", "totalTokens"],
        &["totalTokens"],
    ];
    PATHS.iter().find_map(|p| i64_at(v, p))
}

fn extract_timestamp_ms(v: &Value) -> Option<i64> {
    const PATHS: [&[&str]; 5] = [
        &["params", "_meta", "agentTimestampMs"],
        &["params", "update", "_meta", "agentTimestampMs"],
        &["params", "timestamp"],
        &["timestamp"],
        &["ts"],
    ];
    PATHS.iter().find_map(|p| i64_at(v, p))
}

/// 중첩 경로를 따라 값을 얻는다.
fn value_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, key| cur.get(*key))
}

fn string_at(v: &Value, path: &[&str]) -> Option<String> {
    value_at(v, path).and_then(|x| x.as_str()).map(String::from)
}

/// 정수/실수/숫자문자열 모두 i64로 수용.
fn i64_at(v: &Value, path: &[&str]) -> Option<i64> {
    let x = value_at(v, path)?;
    x.as_i64()
        .or_else(|| x.as_f64().map(|f| f as i64))
        .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, updates: &str, summary: Option<&str>) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("updates.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(updates.as_bytes()).unwrap();
        if let Some(s) = summary {
            fs::write(dir.join("summary.json"), s).unwrap();
        }
        path
    }

    #[test]
    fn parses_per_turn_token_deltas() {
        let tmp = std::env::temp_dir().join(format!("grok-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let path = write_session(
            &tmp,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"available_commands_update"},"_meta":{"totalTokens":100,"agentTimestampMs":1700000000000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk","_meta":{"modelId":"grok-composer-2.5-fast"}},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":300,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":450,"agentTimestampMs":1700000005000}}}"#,
            None,
        );
        let mut out = Vec::new();
        parse_session_file(&path, 0, &mut out);
        out.sort_by_key(|s| s.timestamp_ms);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].amount, 200); // 300 - 100
        assert_eq!(out[0].model.as_deref(), Some("grok-composer-2.5-fast"));
        assert_eq!(out[0].timestamp_ms, 1700000003000);
        assert_eq!(out[1].amount, 150); // 450 - 300
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ignores_decreasing_and_repeated_totals() {
        let tmp = std::env::temp_dir().join(format!("grok-test2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let path = write_session(
            &tmp,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":150,"agentTimestampMs":1700000002000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":150,"agentTimestampMs":1700000003000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":120,"agentTimestampMs":1700000004000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":200,"agentTimestampMs":1700000005000}}}"#,
            None,
        );
        let mut out = Vec::new();
        parse_session_file(&path, 0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].amount, 200); // 최댓값 200 - baseline 0
        assert_eq!(out[0].timestamp_ms, 1700000005000);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_back_to_summary_model() {
        let tmp = std::env::temp_dir().join(format!("grok-test3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let path = write_session(
            &tmp,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":220,"agentTimestampMs":1700000002000}}}"#,
            Some(r#"{"current_model_id":"grok-build"}"#),
        );
        let mut out = Vec::new();
        parse_session_file(&path, 0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].model.as_deref(), Some("grok-build"));
        assert_eq!(out[0].amount, 220);
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 실제 API 응답 형태 (2026-08-06 관측).
    fn billing_body() -> Value {
        serde_json::json!({
          "config": {
            "currentPeriod": {
              "type": "USAGE_PERIOD_TYPE_WEEKLY",
              "start": "2026-08-01T21:28:47.680036+00:00",
              "end": "2026-08-08T21:28:47.680036+00:00"
            },
            "creditUsagePercent": 90.0,
            "onDemandCap": { "val": 0 },
            "onDemandUsed": { "val": 0 },
            "productUsage": [
              { "product": "GrokImagine", "usagePercent": 89.0 },
              { "product": "GrokBuild", "usagePercent": 1.0 }
            ],
            "isUnifiedBillingUser": true,
            "prepaidBalance": { "val": 0 },
            "billingPeriodStart": "2026-08-01T21:28:47.680036+00:00",
            "billingPeriodEnd": "2026-08-08T21:28:47.680036+00:00"
          }
        })
    }

    #[test]
    fn parses_credit_usage_and_period() {
        let (usage, err, window) = parse_billing(&billing_body());
        assert!(err.is_none());
        let u = usage.expect("사용률이 나와야 한다");
        assert_eq!(u.five_hour_utilization, 90.0);
        assert_eq!(u.seven_day_utilization, 90.0);
        assert!(!u.is_estimate);
        assert_eq!(window, 7 * 24 * 3600, "주기를 end-start로 재야 한다");
        assert!(u.five_hour_resets_at.starts_with("2026-08-08T"));
    }

    #[test]
    fn measures_period_from_timestamps_not_type_string() {
        // 타입 문자열이 뭐든 실제 간격을 따른다 — 주기 종류가 늘어도 맞는다.
        let mut body = billing_body();
        body["config"]["currentPeriod"]["type"] = serde_json::json!("USAGE_PERIOD_TYPE_MONTHLY");
        body["config"]["currentPeriod"]["end"] = serde_json::json!("2026-08-31T21:28:47.680036+00:00");
        let (_, _, window) = parse_billing(&body);
        assert_eq!(window, 30 * 24 * 3600);
    }

    #[test]
    fn falls_back_when_period_is_missing() {
        let mut body = billing_body();
        body["config"]["currentPeriod"] = Value::Null;
        body["config"]["billingPeriodEnd"] = Value::Null;
        let (usage, _, window) = parse_billing(&body);
        assert_eq!(window, DEFAULT_PERIOD_SECS);
        assert!(usage.expect("사용률은 살아야 한다").five_hour_resets_at.is_empty());
    }

    #[test]
    fn reports_unavailable_without_usage_percent() {
        let mut body = billing_body();
        body["config"]["creditUsagePercent"] = Value::Null;
        let (usage, err, _) = parse_billing(&body);
        assert!(usage.is_none());
        assert_eq!(err, Some("error.unavailable"));
    }

    #[test]
    fn respects_since_ms() {
        let tmp = std::env::temp_dir().join(format!("grok-test4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let path = write_session(
            &tmp,
            r#"{"method":"session/update","params":{"update":{"sessionUpdate":"user_message_chunk"},"_meta":{"agentTimestampMs":1700000001000}}}
{"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"},"_meta":{"totalTokens":220,"agentTimestampMs":1700000002000}}}"#,
            None,
        );
        let mut out = Vec::new();
        parse_session_file(&path, 1800000000000, &mut out); // since > 턴 시각
        assert!(out.is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }
}
