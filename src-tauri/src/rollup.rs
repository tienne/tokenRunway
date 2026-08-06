//! 일별 사용량 롤업 영속화 — 30일 이상 히스토리 제공용.
//!
//! 원본 JSONL은 영원히 남지 않으므로(세션 정리·디스크 관리), 매일 일별 요약을
//! 작은 파일에 누적 저장한다. 원본이 사라져도 과거 일별 통계가 유지되고
//! 긴 기간 조회도 빠르다.
//!
//! 사용률(utilization)은 실시간 폴링으로만 관측되는 값이라(과거 JSONL에서
//! 재계산할 수 없다) 여기 함께 기록한다. 요금제 하향 추천의 안전 가드로 쓴다.
//!
//! 저장 위치: `<config_dir>/token-runway/rollup.json`
//! 구조: 도구명 → (날짜 "YYYY-MM-DD" → DayRollup)

use crate::atomicfile::{preserve_corrupt, write_atomic};
use crate::providers::UsageProvider;
use crate::{local_date_full, local_hour, short_model};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// 최초 백필 시 거슬러 올라갈 최대 일수 (원본 mtime 필터로 실제 있는 만큼만 수집).
const MAX_BACKFILL_DAYS: i64 = 180;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayRollup {
    pub usage: u64,
    pub count: u64,
    pub cost: f64,
    /// 모델별 (usage, cost, count).
    #[serde(default)]
    pub models: BTreeMap<String, (u64, f64, u64)>,
    /// 시간대(0~23시)별 사용량.
    #[serde(default)]
    pub hourly: Vec<u64>,
    /// 그날 관측한 최대 5시간 윈도우 사용률 (%). 실시간 폴링으로만 채워진다.
    #[serde(default)]
    pub peak_five_hour_util: f64,
    /// 그날 관측한 최대 주간 윈도우 사용률 (%).
    #[serde(default)]
    pub peak_seven_day_util: f64,
}

/// 도구명 → (날짜 "YYYY-MM-DD" → DayRollup)
pub type RollupStore = BTreeMap<String, BTreeMap<String, DayRollup>>;

static ROLLUP: Mutex<Option<RollupStore>> = Mutex::new(None);

fn rollup_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("token-runway").join("rollup.json"))
}

fn load_from_disk() -> RollupStore {
    let Some(path) = rollup_path() else {
        return RollupStore::default();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return RollupStore::default();
    };
    match serde_json::from_str(&raw) {
        Ok(store) => store,
        Err(_) => {
            // 여기서 조용히 빈 값으로 넘어가면 백필 범위 밖 히스토리가 영구 유실된다.
            preserve_corrupt(&path);
            RollupStore::default()
        }
    }
}

fn save_to_disk(store: &RollupStore) {
    let Some(path) = rollup_path() else { return };
    if let Ok(json) = serde_json::to_string(store) {
        let _ = write_atomic(&path, &json);
    }
}

/// 전역 스토어를 잠그고 (필요하면 디스크에서 로드한 뒤) 반환.
fn locked() -> MutexGuard<'static, Option<RollupStore>> {
    let mut guard = ROLLUP.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(load_from_disk());
    }
    guard
}

/// 현재 롤업 사본 (최초 호출 시 디스크 로드).
pub fn get() -> RollupStore {
    locked().clone().unwrap_or_default()
}

/// "YYYY-MM-DD" → 그 날 로컬 0시의 epoch millis.
pub(crate) fn date_start_ms(date: &str) -> Option<i64> {
    use chrono::{NaiveDate, TimeZone};
    let nd = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let ndt = nd.and_hms_opt(0, 0, 0)?;
    chrono::Local
        .from_local_datetime(&ndt)
        .single()
        .map(|dt| dt.timestamp_millis())
}

/// 도구의 날짜 구간별 일별 사용량 (없는 날은 0).
///
/// 오늘은 확정 전이라 롤업에 저장되지 않으므로(사용률만 기록된 껍데기일 수 있다)
/// 호출자가 실시간으로 계산한 오늘 값을 넘겨 덮어쓴다.
pub fn daily_usage(tool: &str, dates: &[String], today: &str, today_usage: u64) -> Vec<u64> {
    let guard = locked();
    let tool_map = guard.as_ref().and_then(|s| s.get(tool));
    dates
        .iter()
        .map(|d| {
            if d == today {
                today_usage
            } else {
                tool_map.and_then(|m| m.get(d)).map_or(0, |r| r.usage)
            }
        })
        .collect()
}

/// 오늘 관측한 사용률의 최댓값을 기록한다 (값이 올라갔을 때만 저장).
///
/// 사용률은 과거 JSONL로 재계산할 수 없어 관측 시점에 남겨야 한다.
pub fn record_utilization(tool: &str, five_hour: f64, seven_day: f64) {
    let today = local_date_full(chrono::Utc::now().timestamp_millis());
    let snapshot = {
        let mut guard = locked();
        let Some(store) = guard.as_mut() else { return };
        let day = store
            .entry(tool.to_string())
            .or_default()
            .entry(today)
            .or_default();
        let mut changed = false;
        if five_hour > day.peak_five_hour_util {
            day.peak_five_hour_util = five_hour;
            changed = true;
        }
        if seven_day > day.peak_seven_day_util {
            day.peak_seven_day_util = seven_day;
            changed = true;
        }
        if !changed {
            return;
        }
        store.clone()
    };
    save_to_disk(&snapshot);
}

/// 어제까지의 일별 사용량을 집계해 롤업에 누적 저장한다.
///
/// 오늘(미확정)은 저장하지 않는다 — 조회 시 실시간으로 합친다.
/// 마지막 저장일 이후만 재수집하므로 첫 회(백필) 외에는 가볍다.
pub fn update(providers: &[Box<dyn UsageProvider>]) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let day = 24 * 3600 * 1000i64;
    let today = local_date_full(now_ms);

    // provider 파싱은 무거우므로 락 밖에서 먼저 끝낸다.
    let mut collected: Vec<(String, BTreeMap<String, DayRollup>)> = Vec::new();
    {
        let store = get();
        for p in providers {
            if !p.available() {
                continue;
            }
            let tool = p.tool_name().to_string();

            // 마지막 저장일부터 재수집(그 날은 미완성 저장됐을 수 있어 덮어씀).
            // 오늘 날짜는 사용률만 기록된 껍데기일 수 있어 기준에서 뺀다 —
            // 포함하면 어제 사용량이 영영 저장되지 않는다.
            let since = store
                .get(&tool)
                .and_then(|m| m.keys().filter(|d| d.as_str() < today.as_str()).max())
                .and_then(|d| date_start_ms(d))
                .unwrap_or(now_ms - MAX_BACKFILL_DAYS * day);

            let mut fresh: BTreeMap<String, DayRollup> = BTreeMap::new();
            for s in p.collect_samples(since) {
                let date = local_date_full(s.timestamp_ms);
                if date == today {
                    continue; // 오늘은 확정 전 — 저장 안 함
                }
                let e = fresh.entry(date).or_default();
                e.usage += s.amount;
                e.count += 1;
                e.cost += s.cost_usd;
                if e.hourly.is_empty() {
                    e.hourly = vec![0; 24];
                }
                e.hourly[local_hour(s.timestamp_ms)] += s.amount;
                if let Some(m) = &s.model {
                    let me = e.models.entry(short_model(m)).or_default();
                    me.0 += s.amount;
                    me.1 += s.cost_usd;
                    me.2 += 1;
                }
            }
            collected.push((tool, fresh));
        }
    }

    let snapshot = {
        let mut guard = locked();
        let Some(store) = guard.as_mut() else { return };
        for (tool, fresh) in collected {
            let tool_map = store.entry(tool).or_default();
            for (date, mut day_roll) in fresh {
                // 사용률은 샘플로 재계산할 수 없으므로 재집계가 덮어쓰면 안 된다.
                if let Some(prev) = tool_map.get(&date) {
                    day_roll.peak_five_hour_util = prev.peak_five_hour_util;
                    day_roll.peak_seven_day_util = prev.peak_seven_day_util;
                }
                tool_map.insert(date, day_roll);
            }
        }
        store.clone()
    };
    save_to_disk(&snapshot);
}
