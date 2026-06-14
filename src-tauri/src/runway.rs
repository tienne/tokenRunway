//! RunwayEngine — 시계열 샘플을 런웨이 지표로 변환.
//!
//! AgentBar의 percent 스냅샷과 달리, 시계열에서 소진 속도와 ETA를 뽑는 것이
//! Token Runway의 핵심 차별점이다.

use crate::providers::{RunwayStatus, UsageProvider};

/// 사용량 누적 윈도우 (Claude Code 5h 한도 기준).
pub const WINDOW_SECS: i64 = 5 * 3600;

/// 소진 속도 측정 구간 (분). 최근 이 시간의 기울기로 ETA를 추정.
const BURN_WINDOW_MIN: f64 = 15.0;

/// provider 하나에 대한 런웨이 상태 계산.
pub fn compute(provider: &dyn UsageProvider, now_ms: i64, limit: Option<u64>) -> RunwayStatus {
    let since_ms = now_ms - WINDOW_SECS * 1000;
    let samples = provider.collect_samples(since_ms);

    let window_tokens: u64 = samples.iter().map(|s| s.tokens).sum();

    // 소진 속도: 최근 BURN_WINDOW_MIN 분간 토큰 / 분
    let recent_since = now_ms - (BURN_WINDOW_MIN as i64) * 60 * 1000;
    let recent_tokens: u64 = samples
        .iter()
        .filter(|s| s.timestamp_ms >= recent_since)
        .map(|s| s.tokens)
        .sum();
    let burn_rate_per_min = recent_tokens as f64 / BURN_WINDOW_MIN;

    let percent_remaining = limit.map(|l| {
        let remaining = (l as f64 - window_tokens as f64).max(0.0);
        (remaining / l as f64) * 100.0
    });

    let eta_minutes = match limit {
        Some(l) if burn_rate_per_min > 0.0 => {
            let remaining = (l as f64 - window_tokens as f64).max(0.0);
            Some(remaining / burn_rate_per_min)
        }
        _ => None,
    };

    let note = if limit.is_none() {
        Some("한도 미설정 — OAuth 연동 전까지 사용량/속도만 표시".to_string())
    } else {
        None
    };

    RunwayStatus {
        tool: provider.tool_name().to_string(),
        available: provider.available(),
        window_tokens,
        limit,
        percent_remaining,
        burn_rate_per_min,
        eta_minutes,
        note,
    }
}
