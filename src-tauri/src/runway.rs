//! RunwayEngine — 시계열 샘플을 런웨이 지표로 변환.
//!
//! AgentBar의 percent 스냅샷과 달리, 시계열에서 소진 속도와 ETA를 뽑는 것이
//! Token Runway의 핵심 차별점이다.

use crate::providers::{RunwayStatus, UsageProvider};

/// 소진 속도 측정 구간 (분). 최근 이 시간의 기울기로 ETA를 추정.
const BURN_WINDOW_MIN: f64 = 15.0;

/// provider 하나에 대한 런웨이 상태 계산.
///
/// 누적 윈도우와 단위는 provider가 정한다 (토큰 도구 5h, Gemini 요청·24h 등).
pub fn compute(provider: &dyn UsageProvider, now_ms: i64, limit: Option<u64>) -> RunwayStatus {
    let window_secs = provider.window_secs();
    let since_ms = now_ms - window_secs * 1000;
    let samples = provider.collect_samples(since_ms);

    let window_usage: u64 = samples.iter().map(|s| s.amount).sum();

    // 소진 속도: 최근 BURN_WINDOW_MIN 분간 사용량 / 분
    let recent_since = now_ms - (BURN_WINDOW_MIN as i64) * 60 * 1000;
    let recent_usage: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= recent_since)
        .map(|s| s.amount)
        .sum();
    let burn_rate_per_min = recent_usage as f64 / BURN_WINDOW_MIN;

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
            let note = if provider.unit() == "requests" {
                Some("추정치 — 무료티어 1000 req/day 가정".to_string())
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
            let note = if limit.is_none() {
                Some("한도 정보 없음 — 사용량/속도만 표시".to_string())
            } else {
                None
            };
            (pct, eta, None, None, note)
        }
    };

    RunwayStatus {
        tool: provider.tool_name().to_string(),
        available: provider.available(),
        unit: provider.unit().to_string(),
        window_hours: window_secs as f64 / 3600.0,
        window_usage,
        limit,
        percent_remaining,
        burn_rate_per_min,
        eta_minutes,
        resets_at,
        seven_day_remaining,
        note,
    }
}
