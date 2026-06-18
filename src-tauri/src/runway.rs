//! RunwayEngine — 시계열 샘플을 런웨이 지표로 변환.
//!
//! AgentBar의 percent 스냅샷과 달리, 시계열에서 소진 속도와 ETA를 뽑는 것이
//! Token Runway의 핵심 차별점이다.

use chrono::Local;
use crate::providers::{RunwayStatus, UsageProvider};

/// 소진 속도 측정 구간 (분). 최근 이 시간의 기울기로 ETA를 추정.
const BURN_WINDOW_MIN: f64 = 15.0;

/// 추세 스파크라인 구간 수.
const SPARK_BUCKETS: usize = 12;

/// 오늘(로컬) 자정의 epoch millis.
fn local_midnight_ms() -> i64 {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(Local).single())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// provider 하나에 대한 런웨이 상태 계산.
///
/// 누적 윈도우와 단위는 provider가 정한다 (토큰 도구 5h, Gemini 요청·24h 등).
pub fn compute(provider: &dyn UsageProvider, now_ms: i64, limit: Option<u64>) -> RunwayStatus {
    let window_secs = provider.window_secs();
    let window_start = now_ms - window_secs * 1000;
    let day_start = local_midnight_ms();
    // 윈도우 시작과 오늘 자정 중 더 이른 시점부터 한 번에 수집 후 구간별 집계.
    let samples = provider.collect_samples(window_start.min(day_start));

    let window_usage: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= window_start)
        .map(|s| s.amount)
        .sum();

    // 오늘(자정 이후) 누적 사용량 + 메시지/요청 수 + 비용.
    let daily_samples = samples.iter().filter(|s| s.timestamp_ms >= day_start);
    let daily_usage: u64 = daily_samples.clone().map(|s| s.amount).sum();
    let daily_cost: f64 = daily_samples.clone().map(|s| s.cost_usd).sum();
    let daily_count = daily_samples.count() as u64;

    // 윈도우 캐시 적중률 = cache_read / 입력 토큰 총량 (output 제외 — 캐시는 입력에만 적용).
    let window_cache_read: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= window_start)
        .map(|s| s.cache_read)
        .sum();
    let window_input: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= window_start)
        .map(|s| s.input_total)
        .sum();
    let cache_hit_rate = if window_cache_read > 0 && window_input > 0 {
        Some((window_cache_read as f64 / window_input as f64) * 100.0)
    } else {
        None
    };

    // 소진 속도: 최근 BURN_WINDOW_MIN 분간 사용량 / 분
    let recent_since = now_ms - (BURN_WINDOW_MIN as i64) * 60 * 1000;
    let recent_usage: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= recent_since)
        .map(|s| s.amount)
        .sum();
    let burn_rate_per_min = recent_usage as f64 / BURN_WINDOW_MIN;

    // 윈도우를 SPARK_BUCKETS개 구간으로 나눠 구간별 사용량 집계 (추세 그래프).
    let mut sparkline = vec![0u64; SPARK_BUCKETS];
    let bucket_ms = ((window_secs * 1000) / SPARK_BUCKETS as i64).max(1);
    for s in samples.iter().filter(|s| s.timestamp_ms >= window_start) {
        let idx = (((s.timestamp_ms - window_start) / bucket_ms) as usize).min(SPARK_BUCKETS - 1);
        sparkline[idx] += s.amount;
    }

    // 공식 사용률(OAuth)이 있으면 우선 사용 — 로컬 토큰 합산보다 정확하다.
    let official = provider.official_usage();

    let (percent_remaining, eta_minutes, resets_at, seven_day_remaining, note) = match &official {
        Some(u) => {
            let pct_remaining = (100.0 - u.five_hour_utilization).max(0.0);

            // 공식 사용률 %와 로컬 누적 토큰으로 한도(분모)를 역산해 ETA를 계산한다.
            // (Anthropic은 절대 토큰 한도를 공개하지 않으므로 역산이 유일한 방법)
            let eta = if u.five_hour_utilization > 0.0 && burn_rate_per_min > 0.0 {
                let implied_limit = window_usage as f64 / (u.five_hour_utilization / 100.0);
                let remaining = (implied_limit - window_usage as f64).max(0.0);
                Some(remaining / burn_rate_per_min)
            } else {
                None
            };

            // 요청 수 기반(Gemini)은 공식 한도가 아닌 추정치이므로 명시한다.
            // note는 i18n 키 — 프론트에서 번역한다.
            let note = if provider.unit() == "requests" {
                Some("note.estimate".to_string())
            } else {
                None
            };
            // 주간 데이터가 없으면(0%·빈 리셋) 표시하지 않는다.
            let seven_day = if u.seven_day_resets_at.is_empty() {
                None
            } else {
                Some((100.0 - u.seven_day_utilization).max(0.0))
            };
            (
                Some(pct_remaining),
                eta,
                Some(u.five_hour_resets_at.clone()),
                seven_day,
                note,
            )
        }
        None => {
            let pct = limit.map(|l| {
                let remaining = (l as f64 - window_usage as f64).max(0.0);
                (remaining / l as f64) * 100.0
            });
            let eta = match limit {
                Some(l) if burn_rate_per_min > 0.0 => {
                    let remaining = (l as f64 - window_usage as f64).max(0.0);
                    Some(remaining / burn_rate_per_min)
                }
                _ => None,
            };
            // 공식 사용률 실패 사유(에러)가 있으면 그걸 우선 표시.
            let note = provider
                .status_note()
                .or_else(|| limit.is_none().then(|| "note.no_limit".to_string()));
            (pct, eta, None, None, note)
        }
    };

    let plan = official.as_ref().and_then(|u| u.plan.clone());

    RunwayStatus {
        tool: provider.tool_name().to_string(),
        available: provider.available(),
        unit: provider.unit().to_string(),
        window_hours: window_secs as f64 / 3600.0,
        window_usage,
        daily_usage,
        daily_count,
        daily_cost,
        cache_hit_rate,
        sparkline,
        limit,
        percent_remaining,
        burn_rate_per_min,
        eta_minutes,
        resets_at,
        seven_day_remaining,
        plan,
        note,
    }
}
