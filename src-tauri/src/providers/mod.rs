//! 도구별 사용량 수집기 추상화.
//!
//! 새 AI 도구(Codex, Cursor 등)를 붙일 때는 이 모듈에 파일을 추가하고
//! `UsageProvider`를 구현한 뒤 `lib.rs`의 provider 목록에 등록하면 된다.

pub mod claude_code;
pub mod codex;
pub mod gemini;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 단일 시점의 사용 샘플 (시계열의 한 점).
///
/// `amount`의 단위는 provider의 [`UsageProvider::unit`]에 따른다
/// (토큰 도구는 토큰 수, 요청 기반 도구는 요청 1건).
#[derive(Debug, Clone, Serialize)]
pub struct UsageSample {
    /// epoch milliseconds (UTC)
    pub timestamp_ms: i64,
    /// 해당 시점에 소비된 사용량 (토큰 수 또는 요청 수).
    pub amount: u64,
    /// API 기준 환산 비용 (USD). 단가를 모르는 도구는 0.
    pub cost_usd: f64,
    /// 캐시 읽기 토큰 수 (효율 지표용). 해당 없으면 0.
    pub cache_read: u64,
    /// 캐시 쓰기(생성) 토큰 수. 재생성 과다 판정용. 해당 없으면 0.
    pub cache_write: u64,
    /// 입력 토큰 총량 (input + cache_creation + cache_read). 캐시 적중률 분모.
    pub input_total: u64,
    /// 사용 모델명 (Claude만 채움). 모델별 분해용. 모르면 None.
    pub model: Option<String>,
}

/// 도구가 제공하는 공식(권위) 사용률. Claude Code는 OAuth `/api/oauth/usage`에서 받는다.
///
/// 로컬 토큰 합산보다 정확하므로 percent 계산에 우선 사용한다.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficialUsage {
    /// 5시간 윈도우 사용률 (%).
    pub five_hour_utilization: f64,
    /// 5시간 윈도우 리셋 시각 (RFC3339).
    pub five_hour_resets_at: String,
    /// 주간 윈도우 사용률 (%).
    pub seven_day_utilization: f64,
    /// 주간 윈도우 리셋 시각 (RFC3339).
    pub seven_day_resets_at: String,
    /// 구독 플랜/등급 (예: "Plus", "Max 5x"). 표시용.
    pub plan: Option<String>,
}

/// 도구 하나의 런웨이 상태 — UI로 그대로 전달되는 뷰 모델.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunwayStatus {
    pub tool: String,
    /// 데이터 수집 가능 여부 (예: 로그 디렉토리 존재).
    pub available: bool,
    /// 사용량 단위 ("tokens" | "requests").
    pub unit: String,
    /// 누적 윈도우 길이(시간). UI 라벨용 (토큰 도구 5h, Gemini 24h 등).
    pub window_hours: f64,
    /// 윈도우 누적 사용량 (단위는 `unit`).
    pub window_usage: u64,
    /// 오늘(로컬 자정 이후) 누적 사용량.
    pub daily_usage: u64,
    /// 오늘 메시지/요청 수 (시계열 샘플 개수).
    pub daily_count: u64,
    /// 오늘 API 기준 환산 비용 (USD). 단가 미상 도구는 0.
    pub daily_cost: f64,
    /// 윈도우 캐시 적중률 (%) — cache_read / 전체. 캐시 개념 없으면 None.
    pub cache_hit_rate: Option<f64>,
    /// 효율 인사이트 (i18n 키). 없으면 None. 예: "insight.cache_write".
    pub insight: Option<String>,
    /// 윈도우를 균등 분할한 구간별 사용량 (미니 추세 그래프용).
    pub sparkline: Vec<u64>,
    /// 한도(분모). OAuth 연동 전에는 None → percent/eta 계산 불가.
    pub limit: Option<u64>,
    /// 남은 비율 (%). 공식 사용률 또는 limit이 있을 때만.
    pub percent_remaining: Option<f64>,
    /// 최근 구간 소진 속도 (단위/분).
    pub burn_rate_per_min: f64,
    /// 소진 속도 추세: "up"(가속) | "down"(감속) | "flat" (직전 구간 대비).
    pub burn_trend: String,
    /// 소진까지 예상 시간(분). 한도와 burn_rate가 유효할 때만.
    pub eta_minutes: Option<f64>,
    /// 5시간 윈도우 리셋 시각 (공식 사용률이 있을 때만).
    pub resets_at: Option<String>,
    /// 주간 윈도우 남은 비율 (%) (공식 사용률이 있을 때만).
    pub seven_day_remaining: Option<f64>,
    /// 구독 플랜/등급 배지 (표시용).
    pub plan: Option<String>,
    /// 상태 보조 설명.
    pub note: Option<String>,
}

/// 도구별 사용량 수집기.
pub trait UsageProvider: Send + Sync {
    /// 표시용 도구 이름.
    fn tool_name(&self) -> &'static str;

    /// 데이터 소스가 존재해 수집 가능한지.
    fn available(&self) -> bool;

    /// `since_ms`(epoch millis) 이후의 사용 샘플 수집.
    fn collect_samples(&self, since_ms: i64) -> Vec<UsageSample>;

    /// 사용량 단위. 기본 "tokens".
    fn unit(&self) -> &'static str {
        "tokens"
    }

    /// 사용량 누적 윈도우(초). 기본 5h (Claude Code/Codex 한도 주기).
    fn window_secs(&self) -> i64 {
        5 * 3600
    }

    /// 공식(권위) 사용률. 제공 가능한 도구만 Some을 반환한다 (기본 None).
    fn official_usage(&self) -> Option<OfficialUsage> {
        None
    }

    /// 공식 사용률을 못 받은 이유 (i18n 키). 정상이거나 해당 없으면 None.
    /// 예: "error.expired", "error.rate_limit", "error.no_token".
    fn status_note(&self) -> Option<String> {
        None
    }
}

/// `root` 아래의 모든 `.jsonl` 파일을 재귀적으로 수집한다.
///
/// 파일 mtime이 `since_ms` 이전이면 윈도우 밖이므로 건너뛴다(성능 최적화).
/// mtime을 읽을 수 없으면 보수적으로 포함한다.
pub fn find_recent_jsonl(root: &Path, since_ms: i64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_jsonl(root, since_ms, &mut out);
    out
}

fn collect_jsonl(dir: &Path, since_ms: i64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, since_ms, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if is_recent(&entry, since_ms) {
                out.push(path);
            }
        }
    }
}

/// 파일 수정 시각이 `since_ms` 이후인지. 알 수 없으면 true(포함).
fn is_recent(entry: &std::fs::DirEntry, since_ms: i64) -> bool {
    let Ok(meta) = entry.metadata() else {
        return true;
    };
    let Ok(mtime) = meta.modified() else {
        return true;
    };
    match mtime.duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_millis() as i64 >= since_ms,
        Err(_) => true,
    }
}
