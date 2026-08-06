//! RunwayEngine — 시계열 샘플을 런웨이 지표로 변환.
//!
//! AgentBar의 percent 스냅샷과 달리, 시계열에서 소진 속도와 ETA를 뽑는 것이
//! Token Runway의 핵심 차별점이다.

use std::collections::HashMap;

use crate::providers::{
    Insight, ModelBreakdown, OfficialUsage, RunwayStatus, UsageProvider, UsageSample, Verdict,
    WeeklyDay,
};
use chrono::{Datelike, Local, NaiveDate};

/// 소진 속도 집계 버킷 크기(분).
const BURN_BUCKET_MIN: f64 = 5.0;
/// 소진 속도를 볼 전체 구간(분).
const BURN_LOOKBACK_MIN: f64 = 60.0;
/// 지수가중 반감기(분) — 이만큼 과거의 버킷은 가중치가 절반이 된다.
const BURN_HALF_LIFE_MIN: f64 = 20.0;
/// 추세 비교 기준점(분 전).
const BURN_TREND_OFFSET_MIN: i64 = 30;

/// 추세 스파크라인 구간 수.
const SPARK_BUCKETS: usize = 12;

/// 주간 윈도우 길이(초).
const WEEK_SECS: i64 = 7 * 24 * 3600;

/// 오늘(로컬) 자정의 epoch millis.
fn local_midnight_ms() -> i64 {
    Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|d| d.and_local_timezone(Local).single())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

fn parse_rfc3339_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// 최근 구간을 버킷으로 나눠 지수가중 평균한 소진 속도(단위/분).
///
/// 단순 15분 평균은 잠깐만 손을 놔도 0으로 떨어져 ETA가 사라졌다 나타났다 했다.
/// 최근 버킷에 큰 가중을 주되 과거도 남겨서, 짧은 공백은 견디고 진짜로 멈추면
/// 서서히 0으로 수렴하게 한다.
fn burn_rate(samples: &[UsageSample], now_ms: i64) -> f64 {
    let bucket_ms = (BURN_BUCKET_MIN * 60_000.0) as i64;
    let buckets = (BURN_LOOKBACK_MIN / BURN_BUCKET_MIN) as usize;
    let decay = 0.5_f64.powf(BURN_BUCKET_MIN / BURN_HALF_LIFE_MIN);

    let (mut weighted, mut total_weight) = (0.0, 0.0);
    for i in 0..buckets {
        let end = now_ms - (i as i64) * bucket_ms;
        let start = end - bucket_ms;
        let usage: u64 = samples
            .iter()
            .filter(|s| s.timestamp_ms >= start && s.timestamp_ms < end)
            .map(|s| s.amount)
            .sum();
        let weight = decay.powi(i as i32);
        weighted += (usage as f64 / BURN_BUCKET_MIN) * weight;
        total_weight += weight;
    }
    if total_weight > 0.0 {
        weighted / total_weight
    } else {
        0.0
    }
}

/// 윈도우 경과 대비 소진 페이스로 본 소진 예상 시각(분 후).
///
/// 공식 사용률만으로 계산하므로 추가 파싱이 없다. 리셋 전에 다 쓰지 않을
/// 페이스면 None — 소진하지 않는데 소진 시각을 말하면 거짓말이 된다.
fn pace_eta_minutes(util: f64, resets_at: &str, window_secs: i64, now_ms: i64) -> Option<f64> {
    if !(0.0..100.0).contains(&util) || util <= 0.0 {
        return None;
    }
    let reset_ms = parse_rfc3339_ms(resets_at)?;
    let to_reset = (reset_ms - now_ms) as f64;
    if to_reset <= 0.0 {
        return None;
    }
    let elapsed = window_secs as f64 * 1000.0 - to_reset;
    if elapsed <= 0.0 {
        return None;
    }
    let consumed = util / 100.0;
    let per_ms = consumed / elapsed;
    if per_ms <= 0.0 {
        return None;
    }
    let ms_left = (1.0 - consumed) / per_ms;
    if ms_left >= to_reset {
        return None;
    }
    Some(ms_left / 60_000.0)
}

/// 현재 주간 윈도우를 날짜별로 쪼개 각 날이 주간 한도의 몇 %를 썼는지 낸다.
///
/// 주간 절대 토큰 한도는 비공개라 알 수 없다. 대신 일별 토큰을 윈도우 총합 대비
/// 공식 사용률로 안분한다 — `그날 % = 그날 토큰 / 윈도우 총 토큰 × 공식 사용률`.
/// 이러면 일별 %의 합이 항상 공식 사용률과 같아져, 같은 카드 안의 두 숫자가
/// 어긋나지 않는다. 다른 기기에서 쓴 분량은 로컬에 없지만 비율로 흡수되며,
/// 그만큼 개별 날짜 값은 근사가 된다.
///
/// 슬롯은 윈도우가 걸친 날짜 전부다 — 리셋 시각이 자정이 아니면 8칸이 될 수 있다.
pub(crate) fn weekly_days(
    tool: &str,
    official: &OfficialUsage,
    now_ms: i64,
    today_usage: u64,
) -> Vec<WeeklyDay> {
    let util = official.seven_day_utilization;
    if util <= 0.0 || official.seven_day_resets_at.is_empty() {
        return Vec::new();
    }
    let Some(reset_ms) = parse_rfc3339_ms(&official.seven_day_resets_at) else {
        return Vec::new();
    };
    if reset_ms <= now_ms {
        return Vec::new(); // 지나간 리셋 시각 — 신뢰할 수 없다
    }

    let dates = window_dates(reset_ms);
    if dates.is_empty() {
        return Vec::new();
    }

    let today = crate::local_date_full(now_ms);
    let usage = crate::rollup::daily_usage(tool, &dates, &today, today_usage);
    allocate(&dates, &usage, util, &today)
}

/// 리셋 시각이 정하는 주간 윈도우가 걸친 날짜들.
///
/// 윈도우는 `[reset - 7d, reset)` 반개구간이라 마지막 순간은 reset 직전이다.
/// 리셋이 자정이면 7칸, 하루 중간이면 양끝이 부분일이라 8칸이 된다.
fn window_dates(reset_ms: i64) -> Vec<String> {
    crate::date_range(
        &crate::local_date_full(reset_ms - WEEK_SECS * 1000),
        &crate::local_date_full(reset_ms - 1),
    )
}

/// 일별 사용량을 공식 사용률로 안분한다 (순수 계산).
///
/// 합계가 항상 `util`이 되도록 총합 대비 비율로 나눈다.
fn allocate(dates: &[String], usage: &[u64], util: f64, today: &str) -> Vec<WeeklyDay> {
    let total: u64 = usage.iter().sum();
    if total == 0 {
        return Vec::new();
    }
    let mut cumulative = 0.0;
    dates
        .iter()
        .zip(usage)
        .map(|(date, amount)| {
            let daily = *amount as f64 / total as f64 * util;
            cumulative += daily;
            WeeklyDay {
                date: date.get(5..).unwrap_or(date).replace('-', "/"),
                weekday: weekday_index(date),
                usage: *amount,
                daily_percent: daily,
                cumulative_percent: cumulative,
                is_today: date.as_str() == today,
                is_future: date.as_str() > today,
            }
        })
        .collect()
}

/// "YYYY-MM-DD" → 0(월) ~ 6(일). 파싱 실패 시 0.
fn weekday_index(date: &str) -> u8 {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.weekday().num_days_from_monday() as u8)
        .unwrap_or(0)
}

/// 카드 맨 위 한 줄 결론 — 소진이 먼저인지 리셋이 먼저인지.
fn build_verdict(eta_minutes: Option<f64>, reset_minutes: Option<f64>) -> Verdict {
    let make = |key: &str, level: &str, eta: Option<f64>| Verdict {
        key: key.to_string(),
        level: level.to_string(),
        eta_minutes: eta,
        reset_minutes,
    };
    match (eta_minutes, reset_minutes) {
        // 리셋보다 소진이 빠르다 — 지금 아껴야 하는 상황.
        (Some(eta), Some(reset)) if eta < reset => make("verdict.runsOut", "danger", Some(eta)),
        // 리셋 시각을 알고, 그 전엔 안 떨어진다.
        (_, Some(_)) => make("verdict.safe", "good", None),
        // 리셋 시각을 모르는 도구 — 소진 시점만 말한다.
        (Some(eta), None) => make("verdict.runsOutNoReset", "warn", Some(eta)),
        (None, None) => make("verdict.steady", "good", None),
    }
}

/// provider 하나에 대한 런웨이 상태 계산.
///
/// 누적 윈도우와 단위는 provider가 정한다 (토큰 도구 5h, Gemini 요청·24h 등).
pub fn compute(provider: &dyn UsageProvider, now_ms: i64, limit: Option<u64>) -> RunwayStatus {
    let window_secs = provider.window_secs();
    let rolling_start = now_ms - window_secs * 1000;
    let day_start = local_midnight_ms();

    // 공식 사용률(OAuth·로컬 rate_limits)이 있으면 우선 사용 — 로컬 합산보다 정확하다.
    let official = provider.official_usage();

    // 도구의 윈도우는 롤링이 아니라 리셋 시각 기준 고정 구간이다. 롤링 합계로
    // 한도를 역산하면 경계가 어긋난 만큼 분자가 부풀어 한도가 과대 추정되고
    // ETA가 실제보다 낙관적으로 나온다. 리셋 시각을 알면 거기에 맞춘다.
    let win_start = official
        .as_ref()
        .and_then(|u| parse_rfc3339_ms(&u.five_hour_resets_at))
        .filter(|reset| *reset > now_ms)
        .map(|reset| reset - window_secs * 1000)
        .unwrap_or(rolling_start);

    // 윈도우 시작과 오늘 자정 중 더 이른 시점부터 한 번에 수집 후 구간별 집계.
    let samples = provider.collect_samples(win_start.min(day_start).min(rolling_start));
    let in_window = || samples.iter().filter(|s| s.timestamp_ms >= win_start);

    let window_usage: u64 = in_window().map(|s| s.amount).sum();

    // 오늘(자정 이후) 누적 사용량 + 메시지/요청 수 + 비용.
    let daily_samples = samples.iter().filter(|s| s.timestamp_ms >= day_start);
    let daily_usage: u64 = daily_samples.clone().map(|s| s.amount).sum();
    let daily_cost: f64 = daily_samples.clone().map(|s| s.cost_usd).sum();
    let daily_count = daily_samples.count() as u64;

    // 윈도우 캐시 적중률 = cache_read / 입력 토큰 총량 (output 제외 — 캐시는 입력에만 적용).
    let window_cache_read: u64 = in_window().map(|s| s.cache_read).sum();
    let window_input: u64 = in_window().map(|s| s.input_total).sum();
    let cache_hit_rate = if window_cache_read > 0 && window_input > 0 {
        Some((window_cache_read as f64 / window_input as f64) * 100.0)
    } else {
        None
    };

    // 효율 인사이트(코칭) — 비효율 신호를 하나 골라 알린다.
    let window_count = in_window().count() as u64;
    let window_cache_write: u64 = in_window().map(|s| s.cache_write).sum();
    let cache_write_ratio = if window_input > 0 {
        window_cache_write as f64 / window_input as f64
    } else {
        0.0
    };
    // 요청당 "실제 새로 처리한" 토큰 = 전체에서 캐시 읽기 제외.
    // (캐시 read는 매 요청 컨텍스트 재전송분이라 포함하면 정상 세션도 무겁게 잡힘)
    let fresh_per_msg = if window_count > 0 {
        window_usage.saturating_sub(window_cache_read) / window_count
    } else {
        0
    };

    // 효율 인사이트(코칭/칭찬) — 해당하는 것을 모두 모은다(최대 3개).
    let mut insights: Vec<Insight> = Vec::new();
    // 캐시 재생성 과다 = 컨텍스트가 자주 바뀜
    if cache_write_ratio > 0.5 && window_cache_write > 50_000 {
        insights.push(Insight::new("insight.cache_write", "warn"));
    }
    // 요청당 새 토큰 과다 = 무거운 컨텍스트
    if fresh_per_msg > 40_000 {
        insights.push(Insight::new("insight.heavy", "warn"));
    }
    // 캐시 적중률: 충분히 입력이 쌓였을 때만 평가
    if window_input > 50_000 {
        match cache_hit_rate {
            Some(hit) if hit >= 80.0 => {
                insights.push(Insight::new("insight.good_cache", "good"));
            }
            Some(hit) if hit < 30.0 => {
                insights.push(Insight::new("insight.low_cache", "tip"));
            }
            _ => {}
        }
    }
    insights.truncate(3);

    // 윈도우 내 모델별 사용량 집계 (model을 채우는 도구 — 현재 Claude·Grok).
    let mut model_map: HashMap<String, (u64, f64)> = HashMap::new();
    for s in in_window() {
        if let Some(m) = &s.model {
            let e = model_map.entry(crate::short_model(m)).or_insert((0, 0.0));
            e.0 += s.amount;
            e.1 += s.cost_usd;
        }
    }
    let mut models: Vec<ModelBreakdown> = model_map
        .into_iter()
        .map(|(model, (usage, cost))| ModelBreakdown { model, usage, cost })
        .collect();
    models.sort_by(|a, b| b.usage.cmp(&a.usage));

    let burn_rate_per_min = burn_rate(&samples, now_ms);
    let prev_rate = burn_rate(&samples, now_ms - BURN_TREND_OFFSET_MIN * 60_000);
    let burn_trend = if burn_rate_per_min <= 0.0 && prev_rate <= 0.0 {
        "flat"
    } else if burn_rate_per_min > prev_rate * 1.2 {
        "up"
    } else if burn_rate_per_min < prev_rate * 0.8 {
        "down"
    } else {
        "flat"
    }
    .to_string();

    // 윈도우를 SPARK_BUCKETS개 구간으로 나눠 구간별 사용량 집계 (추세 그래프).
    let mut sparkline = vec![0u64; SPARK_BUCKETS];
    let bucket_ms = ((window_secs * 1000) / SPARK_BUCKETS as i64).max(1);
    for s in in_window() {
        let idx = (((s.timestamp_ms - win_start) / bucket_ms) as usize).min(SPARK_BUCKETS - 1);
        sparkline[idx] += s.amount;
    }

    let (percent_remaining, eta_minutes, resets_at, seven_day_remaining, note) = match &official {
        Some(u) => {
            let pct_remaining = (100.0 - u.five_hour_utilization).max(0.0);

            // 공식 사용률 %와 같은 윈도우의 로컬 누적 토큰으로 한도(분모)를 역산해
            // ETA를 계산한다. (Anthropic은 절대 토큰 한도를 공개하지 않는다)
            let eta = if u.five_hour_utilization > 0.0 && burn_rate_per_min > 0.0 {
                let implied_limit = window_usage as f64 / (u.five_hour_utilization / 100.0);
                let remaining = (implied_limit - window_usage as f64).max(0.0);
                Some(remaining / burn_rate_per_min)
            } else {
                // 로컬 샘플이 없어도(다른 기기에서 쓴 경우 등) 공식 사용률의
                // 소진 페이스만으로 추정할 수 있다.
                pace_eta_minutes(
                    u.five_hour_utilization,
                    &u.five_hour_resets_at,
                    window_secs,
                    now_ms,
                )
            };

            // 요청 수 기반(Gemini)은 공식 한도가 아닌 추정치이므로 명시한다.
            // note는 i18n 키 — 프론트에서 번역한다.
            let note = if u.is_estimate {
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

    // 주간 한도 소진 페이스 — Max 사용자의 실제 병목은 5시간이 아니라 주간인 경우가 많다.
    let seven_day_eta_minutes = official.as_ref().and_then(|u| {
        pace_eta_minutes(
            u.seven_day_utilization,
            &u.seven_day_resets_at,
            WEEK_SECS,
            now_ms,
        )
    });

    // 주간 한도를 날짜별로 쪼갠 소진 분해 — 어느 날 몰아 썼는지 보이게 한다.
    let weekly_breakdown = official
        .as_ref()
        .map(|u| weekly_days(provider.tool_name(), u, now_ms, daily_usage))
        .unwrap_or_default();

    let reset_minutes = resets_at
        .as_deref()
        .and_then(parse_rfc3339_ms)
        .map(|ms| (ms - now_ms) as f64 / 60_000.0)
        .filter(|m| *m > 0.0);
    let verdict = percent_remaining.map(|_| build_verdict(eta_minutes, reset_minutes));

    let plan = official.as_ref().and_then(|u| u.plan.clone());
    let is_estimate = official.as_ref().is_some_and(|u| u.is_estimate);

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
        insights,
        plan_hint: plan_hint(official.as_ref()),
        models,
        sparkline,
        limit,
        percent_remaining,
        burn_rate_per_min,
        burn_trend,
        eta_minutes,
        resets_at,
        seven_day_remaining,
        seven_day_eta_minutes,
        weekly_days: weekly_breakdown,
        verdict,
        is_estimate,
        plan,
        note,
    }
}

/// 요금제 상향 힌트 — 주간 사용률 기반.
///
/// 주간 윈도우는 한 주를 평균낸 값이라 5h 스냅샷보다 안정적이라, 별도 사용률
/// 이력 저장 없이도 신뢰할 수 있다. 상향만 다룬다: 주간 캡에 근접 = 지금 요금제가
/// 빠듯하다는 확실한 신호. 하향(절약) 판단은 한 주 스냅샷으로는 "잠깐 조용한 주"와
/// 구분되지 않아, 여러 주의 사용률 이력을 보는 히스토리 창의 요금제 추천이 담당한다.
fn plan_hint(official: Option<&OfficialUsage>) -> Option<Insight> {
    let u = official?;
    (!u.seven_day_resets_at.is_empty() && u.seven_day_utilization >= 85.0)
        .then(|| Insight::new("planHint.upgrade", "warn"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, amount: u64) -> UsageSample {
        UsageSample {
            timestamp_ms: ts,
            amount,
            cost_usd: 0.0,
            cache_read: 0,
            cache_write: 0,
            input_total: 0,
            model: None,
        }
    }

    #[test]
    fn burn_rate_survives_a_short_idle_gap() {
        let now = 1_700_000_000_000i64;
        let min = 60_000i64;
        // 20~40분 전에 활발히 썼고 최근 15분은 쉬었다.
        let samples: Vec<_> = (20..40).map(|m| sample(now - m * min, 1_000)).collect();
        let rate = burn_rate(&samples, now);
        assert!(rate > 0.0, "짧은 공백에 속도가 0으로 죽으면 안 된다: {rate}");
    }

    #[test]
    fn burn_rate_decays_to_zero_when_truly_idle() {
        let now = 1_700_000_000_000i64;
        let min = 60_000i64;
        // 전부 lookback(60분) 바깥.
        let samples: Vec<_> = (90..120).map(|m| sample(now - m * min, 1_000)).collect();
        assert_eq!(burn_rate(&samples, now), 0.0);
    }

    #[test]
    fn burn_rate_weights_recent_buckets_more() {
        let now = 1_700_000_000_000i64;
        let min = 60_000i64;
        let recent = burn_rate(&[sample(now - 2 * min, 10_000)], now);
        let older = burn_rate(&[sample(now - 50 * min, 10_000)], now);
        assert!(recent > older, "최근 사용이 더 큰 가중을 받아야 한다");
    }

    #[test]
    fn pace_eta_is_none_when_reset_comes_first() {
        let now = 1_700_000_000_000i64;
        // 5시간 윈도우의 절반이 지났는데 10%만 썼다 — 리셋 전에 소진될 리 없다.
        let reset = chrono::DateTime::from_timestamp_millis(now + 150 * 60_000)
            .unwrap()
            .to_rfc3339();
        assert_eq!(pace_eta_minutes(10.0, &reset, 5 * 3600, now), None);
    }

    #[test]
    fn pace_eta_fires_when_burning_too_fast() {
        let now = 1_700_000_000_000i64;
        // 윈도우 절반 지났는데 90% 소진 — 리셋 전에 바닥난다.
        let reset = chrono::DateTime::from_timestamp_millis(now + 150 * 60_000)
            .unwrap()
            .to_rfc3339();
        let eta = pace_eta_minutes(90.0, &reset, 5 * 3600, now).expect("소진 예상이 나와야 한다");
        assert!(eta > 0.0 && eta < 150.0, "리셋(150분)보다 빨라야 한다: {eta}");
    }

    #[test]
    fn verdict_flags_running_out_before_reset() {
        let v = build_verdict(Some(30.0), Some(120.0));
        assert_eq!(v.key, "verdict.runsOut");
        assert_eq!(v.level, "danger");
    }

    #[test]
    fn verdict_is_safe_when_reset_comes_first() {
        let v = build_verdict(Some(300.0), Some(120.0));
        assert_eq!(v.key, "verdict.safe");
        assert_eq!(v.level, "good");
        assert_eq!(v.eta_minutes, None);
    }

    #[test]
    fn verdict_is_safe_when_not_burning() {
        let v = build_verdict(None, Some(90.0));
        assert_eq!(v.key, "verdict.safe");
    }

    fn dates(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("2026-08-{:02}", i + 3)).collect()
    }

    #[test]
    fn allocated_percents_sum_to_official_utilization() {
        let d = dates(7);
        // 다른 기기 사용분이 있어 로컬 토큰만으로는 47%가 안 나오는 상황.
        let usage = [0, 1_390, 720, 300, 0, 0, 0];
        let days = allocate(&d, &usage, 47.0, "2026-08-06");
        let sum: f64 = days.iter().map(|x| x.daily_percent).sum();
        assert!((sum - 47.0).abs() < 1e-9, "합계가 공식 사용률과 달라졌다: {sum}");
        assert!((days.last().unwrap().cumulative_percent - 47.0).abs() < 1e-9);
    }

    #[test]
    fn allocation_is_proportional_to_daily_usage() {
        let d = dates(2);
        // 3:1로 썼으면 퍼센트도 3:1이어야 한다.
        let days = allocate(&d, &[300, 100], 40.0, "2026-08-04");
        assert!((days[0].daily_percent - 30.0).abs() < 1e-9);
        assert!((days[1].daily_percent - 10.0).abs() < 1e-9);
    }

    #[test]
    fn cumulative_percent_is_monotonic() {
        let d = dates(5);
        let days = allocate(&d, &[10, 0, 30, 5, 1], 80.0, "2026-08-07");
        for pair in days.windows(2) {
            assert!(pair[1].cumulative_percent >= pair[0].cumulative_percent);
        }
    }

    #[test]
    fn marks_today_and_future_slots() {
        let d = dates(7); // 08-03 ~ 08-09
        let days = allocate(&d, &[1, 1, 1, 1, 0, 0, 0], 20.0, "2026-08-06");
        let today = days.iter().find(|x| x.is_today).expect("오늘이 있어야 한다");
        assert_eq!(today.date, "08/06");
        assert_eq!(days.iter().filter(|x| x.is_future).count(), 3); // 07,08,09
        assert!(!days[0].is_future);
    }

    #[test]
    fn allocation_is_empty_without_local_usage() {
        // 로컬 토큰이 하나도 없으면 안분할 분모가 없다.
        assert!(allocate(&dates(7), &[0; 7], 30.0, "2026-08-06").is_empty());
    }

    #[test]
    fn midnight_reset_spans_seven_slots() {
        use chrono::{Duration, Local};
        let midnight = Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .unwrap();
        let reset = midnight + Duration::days(7);
        assert_eq!(window_dates(reset.timestamp_millis()).len(), 7);
    }

    #[test]
    fn mid_day_reset_spans_eight_slots() {
        use chrono::{Duration, Local};
        // 하루 중간에 리셋되면 양끝이 부분일이라 8칸에 걸친다.
        let noon = Local::now()
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .and_then(|d| d.and_local_timezone(Local).single())
            .unwrap();
        let reset = noon + Duration::days(7);
        assert_eq!(window_dates(reset.timestamp_millis()).len(), 8);
    }

    #[test]
    fn weekday_index_counts_from_monday() {
        assert_eq!(weekday_index("2026-08-03"), 0); // 월
        assert_eq!(weekday_index("2026-08-06"), 3); // 목
        assert_eq!(weekday_index("2026-08-09"), 6); // 일
    }

    #[test]
    fn weekly_days_is_empty_when_reset_already_passed() {
        let now = 1_700_000_000_000i64;
        let stale = chrono::DateTime::from_timestamp_millis(now - 60_000)
            .unwrap()
            .to_rfc3339();
        let official = OfficialUsage {
            five_hour_utilization: 10.0,
            five_hour_resets_at: String::new(),
            seven_day_utilization: 40.0,
            seven_day_resets_at: stale,
            plan: None,
            rate_limit_multiplier: None,
            is_estimate: false,
        };
        assert!(weekly_days("Claude Code", &official, now, 0).is_empty());
    }
}
