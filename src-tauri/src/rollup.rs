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

/// 어제까지의 일별 사용량을 집계해 롤업에 누적 저장한다.
///
/// 오늘(미확정)은 저장하지 않는다 — 조회 시 실시간으로 합친다.
/// 마지막 저장일 이후만 재수집하므로 첫 회(백필) 외에는 가볍다.
pub fn update(providers: &[Box<dyn UsageProvider>]) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let day = 24 * 3600 * 1000i64;
    let today = local_date_full(now_ms);

    let mut store = get();

    for p in providers {
        if !p.available() {
            continue;
        }
        let tool = p.tool_name().to_string();
        let tool_map = store.entry(tool).or_default();

        // 마지막 저장일이 있으면 그 날부터 재수집(그 날은 미완성 저장됐을 수 있어 덮어씀).
        // 없으면 최대 백필 범위부터.
        let since = tool_map
            .keys()
            .max()
            .and_then(|d| date_start_ms(d))
            .unwrap_or(now_ms - MAX_BACKFILL_DAYS * day);

        // since 이후 ~ 어제까지 일별 재집계 (오늘 제외).
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

        // 재집계분으로 upsert (같은 날 재수집은 완전 집계이므로 덮어쓰기가 정확).
        for (date, day_roll) in fresh {
            tool_map.insert(date, day_roll);
        }
    }

    save_to_disk(&store);
    if let Ok(mut guard) = ROLLUP.lock() {
        *guard = Some(store);
    }
}
