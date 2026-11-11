//! 계정별로 마지막에 받은 공식 사용률.
//!
//! 안 쓰는 계정은 CLI가 토큰을 갱신하지 않아 몇 시간 뒤 조회가 401로 막히고, 앱을
//! 다시 켜면 메모리 캐시도 사라진다. 그때 계정 줄을 비우는 대신 마지막 값을 흐리게
//! 보여주려고 디스크(`~/.token-runway/last-usage.json`)에 남긴다.
//!
//! 이 값은 계정 줄에서만 쓴다. 관측 시점이 지난 값이라 카드 대표와 트레이, 경보,
//! 사용률 이력에는 올리지 않는다.

use super::OfficialUsage;
use chrono::{DateTime, Duration};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// 이보다 오래된 값은 버린다. 계정을 지웠거나 조직을 떠난 경우 영원히 남지 않게.
const MAX_AGE_MS: i64 = 30 * 24 * 3600 * 1000;

/// 주간 윈도우 길이. 주간 리셋은 요일과 시각이 고정이라 지난 리셋에서 이만큼씩 더하면
/// 다음 리셋이 나온다.
const WEEK: Duration = Duration::days(7);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    observed_at_ms: i64,
    usage: OfficialUsage,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    accounts: HashMap<String, Entry>,
}

static STORE: LazyLock<Mutex<StoreFile>> = LazyLock::new(|| Mutex::new(load()));

fn key(tool: &str, id: &str) -> String {
    format!("{tool}#{id}")
}

/// 방금 받은 값들을 남긴다. 추정치(`is_estimate`)는 남기지 않는다.
pub fn remember(tool: &str, observed: &[(String, OfficialUsage)], now_ms: i64) {
    if observed.is_empty() {
        return;
    }
    // 락 안에서는 직렬화까지만 하고 파일 쓰기는 락을 놓은 뒤에 한다.
    let json = {
        let Ok(mut store) = STORE.lock() else {
            return;
        };
        put(&mut store, tool, observed, now_ms);
        serde_json::to_string_pretty(&*store).ok()
    };
    if let (Some(json), Some(path)) = (json, store_path()) {
        let _ = crate::atomicfile::write_atomic(&path, &json);
    }
}

/// 마지막으로 받은 값과 그 시각(epoch ms). 리셋이 지난 윈도우는 [`roll_forward`]로 넘긴다.
pub fn recall(tool: &str, id: &str, now_ms: i64) -> Option<(i64, OfficialUsage)> {
    let store = STORE.lock().ok()?;
    get(&store, tool, id, now_ms)
}

fn put(store: &mut StoreFile, tool: &str, observed: &[(String, OfficialUsage)], now_ms: i64) {
    for (id, usage) in observed.iter().filter(|(_, u)| !u.is_estimate) {
        store.accounts.insert(
            key(tool, id),
            Entry {
                observed_at_ms: now_ms,
                usage: usage.clone(),
            },
        );
    }
    store
        .accounts
        .retain(|_, e| now_ms - e.observed_at_ms < MAX_AGE_MS);
}

fn get(store: &StoreFile, tool: &str, id: &str, now_ms: i64) -> Option<(i64, OfficialUsage)> {
    let entry = store.accounts.get(&key(tool, id))?;
    if now_ms - entry.observed_at_ms >= MAX_AGE_MS {
        return None;
    }
    Some((entry.observed_at_ms, roll_forward(entry.usage.clone(), now_ms)))
}

/// 리셋 시각이 지난 윈도우를 지금 기준으로 넘긴다.
///
/// 계정을 안 쓰는 사이 리셋이 지나갔으면 마지막 값은 이미 의미가 없다. 그대로 두면
/// 바닥났던 계정이 계속 바닥난 것처럼 보인다. 리셋 뒤 다른 기기에서 썼을 수도 있지만
/// 그건 알 길이 없어 사용률 0으로 본다 — 계정 줄이 "N시간 전 값"으로 흐리게 그려서
/// 관측값이 아님을 드러낸다.
///
/// 5시간 윈도우는 리셋 뒤 처음 쓸 때 새로 시작해서 다음 리셋 시각을 알 수 없다. 그래서
/// 비운다. 주간은 고정 주기라 7일씩 더해 다음 리셋을 맞춘다.
pub fn roll_forward(mut usage: OfficialUsage, now_ms: i64) -> OfficialUsage {
    if reset_passed(&usage.five_hour_resets_at, now_ms) {
        usage.five_hour_utilization = 0.0;
        usage.five_hour_resets_at = String::new();
    }
    if reset_passed(&usage.seven_day_resets_at, now_ms) {
        usage.seven_day_utilization = 0.0;
        usage.seven_day_resets_at = next_weekly_reset(&usage.seven_day_resets_at, now_ms);
    }
    usage
}

fn reset_passed(resets_at: &str, now_ms: i64) -> bool {
    DateTime::parse_from_rfc3339(resets_at).is_ok_and(|t| t.timestamp_millis() <= now_ms)
}

fn next_weekly_reset(last: &str, now_ms: i64) -> String {
    let Ok(mut t) = DateTime::parse_from_rfc3339(last) else {
        return String::new();
    };
    let weeks = (now_ms - t.timestamp_millis()) / WEEK.num_milliseconds() + 1;
    t += WEEK * weeks as i32;
    t.to_rfc3339()
}

#[cfg(not(test))]
fn store_path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("last-usage.json"))
}

/// 테스트가 실제 `~/.token-runway`를 건드리지 않게 막는다.
#[cfg(test)]
fn store_path() -> Option<PathBuf> {
    None
}

fn load() -> StoreFile {
    let Some(path) = store_path() else {
        return StoreFile::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return StoreFile::default();
    };
    match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(_) => {
            crate::atomicfile::preserve_corrupt(&path);
            StoreFile::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600 * 1000;

    fn usage(five: f64, five_reset: &str, seven: f64, seven_reset: &str) -> OfficialUsage {
        OfficialUsage {
            five_hour_utilization: five,
            five_hour_resets_at: five_reset.to_string(),
            seven_day_utilization: seven,
            seven_day_resets_at: seven_reset.to_string(),
            plan: Some("Max 5x".to_string()),
            rate_limit_multiplier: Some(5.0),
            is_estimate: false,
        }
    }

    fn ms(rfc: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc).unwrap().timestamp_millis()
    }

    #[test]
    fn keeps_windows_whose_reset_is_still_ahead() {
        let u = usage(80.0, "2026-11-11T12:00:00+00:00", 40.0, "2026-11-14T00:00:00+00:00");
        let rolled = roll_forward(u, ms("2026-11-11T10:00:00+00:00"));
        assert_eq!(rolled.five_hour_utilization, 80.0);
        assert_eq!(rolled.seven_day_utilization, 40.0);
        assert_eq!(rolled.seven_day_resets_at, "2026-11-14T00:00:00+00:00");
    }

    #[test]
    fn passed_five_hour_reset_empties_window_and_forgets_reset_time() {
        let u = usage(100.0, "2026-11-11T12:00:00+00:00", 40.0, "2026-11-14T00:00:00+00:00");
        let rolled = roll_forward(u, ms("2026-11-11T13:00:00+00:00"));
        assert_eq!(rolled.five_hour_utilization, 0.0);
        assert_eq!(rolled.five_hour_resets_at, "");
        assert_eq!(rolled.seven_day_utilization, 40.0);
    }

    #[test]
    fn passed_weekly_reset_moves_to_the_next_week_slot() {
        let u = usage(0.0, "", 90.0, "2026-11-01T00:00:00+00:00");
        // 리셋에서 9일 반 지났다 — 다음 리셋은 11-08을 지나 11-15다.
        let rolled = roll_forward(u, ms("2026-11-10T12:00:00+00:00"));
        assert_eq!(rolled.seven_day_utilization, 0.0);
        assert_eq!(
            ms(&rolled.seven_day_resets_at),
            ms("2026-11-15T00:00:00+00:00")
        );
    }

    #[test]
    fn reset_exactly_now_counts_as_passed() {
        let u = usage(50.0, "2026-11-11T12:00:00+00:00", 0.0, "");
        let rolled = roll_forward(u, ms("2026-11-11T12:00:00+00:00"));
        assert_eq!(rolled.five_hour_utilization, 0.0);
    }

    #[test]
    fn recall_returns_observation_time_and_skips_estimates() {
        let mut store = StoreFile::default();
        let now = ms("2026-11-11T10:00:00+00:00");
        let mut estimate = usage(10.0, "", 0.0, "");
        estimate.is_estimate = true;
        put(
            &mut store,
            "claude",
            &[
                ("a".to_string(), usage(30.0, "", 0.0, "")),
                ("b".to_string(), estimate),
            ],
            now,
        );
        let (at, u) = get(&store, "claude", "a", now + HOUR).unwrap();
        assert_eq!(at, now);
        assert_eq!(u.five_hour_utilization, 30.0);
        assert!(get(&store, "claude", "b", now).is_none());
        // 도구가 다르면 같은 id라도 다른 계정이다.
        assert!(get(&store, "codex", "a", now).is_none());
    }

    #[test]
    fn entries_older_than_max_age_are_dropped() {
        let mut store = StoreFile::default();
        let then = ms("2026-10-01T00:00:00+00:00");
        put(&mut store, "claude", &[("old".to_string(), usage(1.0, "", 0.0, ""))], then);
        let later = then + MAX_AGE_MS;
        assert!(get(&store, "claude", "old", later).is_none());
        put(&mut store, "claude", &[("new".to_string(), usage(2.0, "", 0.0, ""))], later);
        assert!(!store.accounts.contains_key("claude#old"));
    }

    #[test]
    fn store_file_round_trips_through_json() {
        let mut store = StoreFile::default();
        put(&mut store, "claude", &[("a".to_string(), usage(30.0, "", 5.0, ""))], 1);
        let json = serde_json::to_string(&store).unwrap();
        let back: StoreFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.accounts["claude#a"].usage.seven_day_utilization, 5.0);
    }
}
