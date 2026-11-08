//! Claude Code statusLine 릴레이 — `trw statusline`이 넘긴 rate_limits.
//!
//! OAuth 사용량 폴링(180초)의 보조 경로다. statusLine은 응답마다 불려서 OAuth보다 자주
//! 최신 값을 준다. 폴링을 대체하지 않고, 받은 값이 OAuth 값보다 늦게 왔을 때만 그
//! 계정의 공식 사용률로 쓴다. 메모리에만 두고 영속하지 않는다 — 앱이 꺼져 있던 사이의
//! 값은 이미 낡아서 OAuth가 다시 받아오는 쪽이 맞다.

use super::OfficialUsage;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// 소켓으로 온 릴레이 한 건. `trw`의 `relay_payload`와 모양이 같다.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayMessage {
    /// 훅 프로세스의 `CLAUDE_CONFIG_DIR`(없으면 `~/.claude`). 계정을 찾는 열쇠다.
    pub config_dir: String,
    pub rate_limits: RelayLimits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayLimits {
    #[serde(default)]
    pub five_hour: Option<RelayWindow>,
    #[serde(default)]
    pub seven_day: Option<RelayWindow>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayWindow {
    pub used_percentage: f64,
    /// epoch 초(숫자)나 RFC3339(문자열). Claude Code 버전에 따라 달라 둘 다 받는다.
    #[serde(default)]
    pub resets_at: Option<serde_json::Value>,
}

/// 정규화한 윈도우 하나.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    utilization: f64,
    /// OAuth 응답과 같은 RFC3339 — runway가 그대로 파싱한다.
    resets_at: Option<String>,
    resets_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct Entry {
    five: Option<Window>,
    seven: Option<Window>,
    received_at: Instant,
}

/// 계정 id → 마지막으로 합친 릴레이 값.
static RELAY_CACHE: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 소켓 콜백에서 부른다. 꺼져 있거나 계정에 안 이어지는 값은 버린다.
pub fn receive(msg: RelayMessage) {
    if !crate::settings::statusline_relay_enabled() {
        return;
    }
    let known = super::claude_code::known_account_services();
    let Some(id) = map_account(&msg.config_dir, &known) else {
        return;
    };
    store(&id, &msg.rate_limits, Instant::now());
}

/// config dir → Keychain 서비스 이름 → 그 이름을 가진 계정 id.
///
/// 계정 목록은 OAuth 쪽이 이미 찾아둔 캐시만 본다. 여기서 Keychain을 다시 훑으면
/// statusLine이 불릴 때마다 `security`를 띄우게 된다.
fn map_account(config_dir: &str, known: &[(String, Vec<String>)]) -> Option<String> {
    let services = super::claude_accounts::services_for_config_dir(config_dir);
    known
        .iter()
        .find(|(_, owned)| owned.iter().any(|s| services.contains(s)))
        .map(|(id, _)| id.clone())
}

fn store(id: &str, limits: &RelayLimits, now: Instant) {
    let five = limits.five_hour.as_ref().and_then(normalize);
    let seven = limits.seven_day.as_ref().and_then(normalize);
    if five.is_none() && seven.is_none() {
        return;
    }
    let Ok(mut cache) = RELAY_CACHE.lock() else {
        return;
    };
    let entry = cache.entry(id.to_string()).or_insert(Entry {
        five: None,
        seven: None,
        received_at: now,
    });
    entry.five = merge(entry.five.take(), five);
    entry.seven = merge(entry.seven.take(), seven);
    entry.received_at = now;
}

fn normalize(w: &RelayWindow) -> Option<Window> {
    if !w.used_percentage.is_finite() {
        return None;
    }
    let (resets_at, resets_ms) = match w.resets_at.as_ref().and_then(parse_reset) {
        Some(dt) => (Some(dt.to_rfc3339()), Some(dt.timestamp_millis())),
        None => (None, None),
    };
    Some(Window {
        utilization: w.used_percentage.clamp(0.0, 100.0),
        resets_at,
        resets_ms,
    })
}

fn parse_reset(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    match v {
        serde_json::Value::Number(n) => {
            let raw = n.as_f64()?;
            // 밀리초로 오는 버전도 있다. 초 단위 epoch는 2286년까지 1e12를 넘지 않는다.
            let ms = if raw > 1e12 { raw } else { raw * 1000.0 };
            Utc.timestamp_millis_opt(ms as i64).single()
        }
        serde_json::Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc)),
        _ => None,
    }
}

/// 같은 계정의 세션 여럿이 각자 마지막 응답의 값을 보내서 도착 순서가 곧 최신은 아니다.
/// 리셋 시각이 늦은 쪽이 새 윈도우라 이긴다. 같은 윈도우면 사용률은 줄지 않으니 큰 쪽이 맞다.
fn merge(old: Option<Window>, new: Option<Window>) -> Option<Window> {
    match (old, new) {
        (None, n) => n,
        (o, None) => o,
        (Some(o), Some(n)) => {
            if n.resets_ms > o.resets_ms {
                Some(n)
            } else if n.resets_ms < o.resets_ms {
                Some(o)
            } else if n.utilization >= o.utilization {
                Some(n)
            } else {
                Some(o)
            }
        }
    }
}

/// OAuth 값 위에 릴레이 값을 얹는다. 릴레이가 그 계정의 OAuth 조회보다 늦게 왔을 때만.
///
/// `fetched_at`이 None이면 OAuth를 아직 한 번도 못 받은 것이라 릴레이가 이긴다.
/// 리셋 시각이 지난 윈도우는 이미 끝난 구간의 사용률이라 얹지 않는다.
pub(crate) fn overlay(
    id: &str,
    usage: Option<OfficialUsage>,
    fetched_at: Option<Instant>,
    plan: Option<String>,
    rate_mult: Option<f64>,
    now_ms: i64,
) -> Option<OfficialUsage> {
    if !crate::settings::statusline_relay_enabled() {
        return usage;
    }
    overlay_with(id, usage, fetched_at, plan, rate_mult, now_ms)
}

fn overlay_with(
    id: &str,
    usage: Option<OfficialUsage>,
    fetched_at: Option<Instant>,
    plan: Option<String>,
    rate_mult: Option<f64>,
    now_ms: i64,
) -> Option<OfficialUsage> {
    let entry = match RELAY_CACHE.lock() {
        Ok(cache) => cache.get(id).cloned(),
        Err(_) => None,
    };
    let Some(entry) = entry else {
        return usage;
    };
    if fetched_at.is_some_and(|f| entry.received_at <= f) {
        return usage;
    }
    let live = |w: Option<Window>| w.filter(|w| w.resets_ms.is_none_or(|r| r > now_ms));
    let five = live(entry.five);
    let seven = live(entry.seven);
    // OAuth 값이 없을 때 5시간 값까지 없으면 카드 헤더를 0%로 세우게 된다.
    if usage.is_none() && five.is_none() {
        return usage;
    }
    let mut out = usage.unwrap_or(OfficialUsage {
        five_hour_utilization: 0.0,
        five_hour_resets_at: String::new(),
        seven_day_utilization: 0.0,
        seven_day_resets_at: String::new(),
        plan,
        rate_limit_multiplier: rate_mult,
        is_estimate: false,
    });
    if let Some(w) = five {
        out.five_hour_utilization = w.utilization;
        if let Some(r) = w.resets_at {
            out.five_hour_resets_at = r;
        }
    }
    if let Some(w) = seven {
        out.seven_day_utilization = w.utilization;
        if let Some(r) = w.resets_at {
            out.seven_day_resets_at = r;
        }
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    pub(crate) fn limits(five: Option<(f64, i64)>, seven: Option<(f64, i64)>) -> RelayLimits {
        let w = |(p, r): (f64, i64)| RelayWindow {
            used_percentage: p,
            resets_at: Some(serde_json::json!(r)),
        };
        RelayLimits {
            five_hour: five.map(w),
            seven_day: seven.map(w),
        }
    }

    pub(crate) fn store_for_test(id: &str, l: &RelayLimits, at: Instant) {
        store(id, l, at);
    }

    pub(crate) fn overlay_for_test(
        id: &str,
        usage: Option<OfficialUsage>,
        fetched_at: Option<Instant>,
        now_ms: i64,
    ) -> Option<OfficialUsage> {
        overlay_with(id, usage, fetched_at, None, None, now_ms)
    }

    fn oauth(five: f64, seven: f64) -> OfficialUsage {
        OfficialUsage {
            five_hour_utilization: five,
            five_hour_resets_at: "2026-11-08T10:00:00+00:00".into(),
            seven_day_utilization: seven,
            seven_day_resets_at: "2026-11-12T00:00:00+00:00".into(),
            plan: Some("Max 5x".into()),
            rate_limit_multiplier: Some(5.0),
            is_estimate: false,
        }
    }

    const T1: i64 = 1_786_000_000;
    const T2: i64 = T1 + 5 * 3600;
    const NOW_MS: i64 = (T1 - 3600) * 1000;

    #[test]
    fn later_reset_wins_even_if_it_arrives_first() {
        let id = "merge-later-reset";
        let t = Instant::now();
        store(id, &limits(Some((5.0, T2)), None), t);
        // 늦게 도착했지만 이전 윈도우 값이다.
        store(id, &limits(Some((90.0, T1)), None), t);
        let u = overlay_with(id, None, None, None, None, NOW_MS).unwrap();
        assert_eq!(u.five_hour_utilization, 5.0);
    }

    #[test]
    fn same_reset_keeps_the_higher_percentage() {
        let id = "merge-same-reset";
        let t = Instant::now();
        store(id, &limits(Some((40.0, T1)), Some((60.0, T2))), t);
        store(id, &limits(Some((35.0, T1)), Some((61.0, T2))), t);
        let u = overlay_with(id, None, None, None, None, NOW_MS).unwrap();
        assert_eq!(u.five_hour_utilization, 40.0);
        assert_eq!(u.seven_day_utilization, 61.0);
    }

    #[test]
    fn relay_replaces_oauth_only_when_newer() {
        let id = "overlay-newer";
        let fetched = Instant::now();
        store(id, &limits(Some((70.0, T1)), None), fetched - Duration::from_secs(1));
        // 릴레이가 OAuth 조회보다 먼저 왔다 — OAuth 값을 그대로 쓴다.
        let u = overlay_with(id, Some(oauth(20.0, 30.0)), Some(fetched), None, None, NOW_MS);
        assert_eq!(u.unwrap().five_hour_utilization, 20.0);

        store(id, &limits(Some((72.0, T1)), None), fetched + Duration::from_secs(1));
        let u = overlay_with(id, Some(oauth(20.0, 30.0)), Some(fetched), None, None, NOW_MS)
            .unwrap();
        assert_eq!(u.five_hour_utilization, 72.0);
        // 릴레이에 없는 주간 값과 플랜은 OAuth 것을 남긴다.
        assert_eq!(u.seven_day_utilization, 30.0);
        assert_eq!(u.plan.as_deref(), Some("Max 5x"));
        assert_eq!(
            parse_reset(&serde_json::json!(u.five_hour_resets_at)).map(|d| d.timestamp()),
            Some(T1)
        );
    }

    #[test]
    fn percentage_is_clamped_and_ms_epoch_is_accepted() {
        let id = "normalize";
        let l = RelayLimits {
            five_hour: Some(RelayWindow {
                used_percentage: 150.0,
                resets_at: Some(serde_json::json!(T1 * 1000)),
            }),
            seven_day: Some(RelayWindow {
                used_percentage: 12.0,
                resets_at: Some(serde_json::json!("2026-11-12T00:00:00Z")),
            }),
        };
        store(id, &l, Instant::now());
        let u = overlay_with(id, None, None, None, None, NOW_MS).unwrap();
        assert_eq!(u.five_hour_utilization, 100.0);
        assert_eq!(
            parse_reset(&serde_json::json!(u.five_hour_resets_at)).map(|d| d.timestamp()),
            Some(T1)
        );
        assert!(u.seven_day_resets_at.starts_with("2026-11-12T00:00:00"));
    }

    #[test]
    fn expired_window_is_not_overlaid() {
        let id = "expired";
        store(id, &limits(Some((95.0, T1)), None), Instant::now());
        let after_reset = (T1 + 60) * 1000;
        let u = overlay_with(id, Some(oauth(3.0, 30.0)), None, None, None, after_reset).unwrap();
        assert_eq!(u.five_hour_utilization, 3.0);
    }

    #[test]
    fn unmapped_config_dir_is_dropped() {
        let known = vec![(
            "org-a".to_string(),
            super::super::claude_accounts::services_for_config_dir("/Users/example/.claude-a"),
        )];
        assert_eq!(
            map_account("/Users/example/.claude-a/", &known).as_deref(),
            Some("org-a")
        );
        assert!(map_account("/Users/example/.claude-b", &known).is_none());
        assert!(map_account("/Users/example/.claude-b", &[]).is_none());
    }
}
