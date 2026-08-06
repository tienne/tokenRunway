//! 도구별 사용량 수집기 추상화.
//!
//! 새 AI 도구(Codex, Cursor 등)를 붙일 때는 이 모듈에 파일을 추가하고
//! `UsageProvider`를 구현한 뒤 `lib.rs`의 provider 목록에 등록하면 된다.

pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod grok;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

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
    /// 레이트리밋 등급 배수 (Pro 대비). "max_5x"→5. 요금제 사다리 앵커용.
    /// 엔터프라이즈도 좌석 등급(예: max_5x)이 있어 개인 플랜 환산의 기준이 된다.
    pub rate_limit_multiplier: Option<f64>,
    /// 도구가 준 값이 아니라 우리가 가정한 한도로 계산한 추정치인지.
    ///
    /// Gemini처럼 한도를 가정해 만든 수치는 공식 사용률과 같은 무게로 다루면
    /// 안 된다 — 트레이 자동 선택에서 공식 데이터에 밀리게 한다.
    pub is_estimate: bool,
}

/// 효율 인사이트 한 건 — i18n 키 + 레벨(색 구분용).
#[derive(Debug, Clone, Serialize)]
pub struct Insight {
    /// i18n 키 (예: "insight.cache_write").
    pub key: String,
    /// "good"(긍정) | "warn"(주의) | "tip"(팁).
    pub level: String,
}

impl Insight {
    pub fn new(key: &str, level: &str) -> Self {
        Self {
            key: key.to_string(),
            level: level.to_string(),
        }
    }
}

/// 카드 맨 위에 세우는 한 줄 결론 — "얼마나 남았어?"에 대한 직접적인 답.
///
/// 잔여율·ETA·리셋 시각을 사용자가 머리로 조합하지 않아도 되게 미리 판정한다.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    /// i18n 키 (예: "verdict.runsOut").
    pub key: String,
    /// "danger"(리셋 전 소진) | "warn" | "good"(리셋까지 여유).
    pub level: String,
    /// 소진까지 남은 분. 소진 페이스가 아니면 None.
    pub eta_minutes: Option<f64>,
    /// 리셋까지 남은 분. 리셋 시각을 모르면 None.
    pub reset_minutes: Option<f64>,
}

/// 주간 윈도우 안의 하루 — 그날이 주간 한도의 몇 %를 썼는지.
///
/// 주간 잔여율 한 줄만으로는 어느 날 몰아 썼는지 알 수 없어, 리셋 시각 기준
/// 윈도우를 날짜로 쪼개 보여준다.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyDay {
    /// 표시용 "MM/DD".
    pub date: String,
    /// 0=월 ~ 6=일. 프론트가 요일 라벨을 현지화한다.
    pub weekday: u8,
    /// 그날 사용량 (토큰).
    pub usage: u64,
    /// 그날이 쓴 주간 한도 비율 (%).
    pub daily_percent: f64,
    /// 그날까지 누적된 주간 한도 비율 (%).
    pub cumulative_percent: f64,
    /// 오늘인지 (강조용).
    pub is_today: bool,
    /// 아직 오지 않은 날인지 (빈 슬롯).
    pub is_future: bool,
}

/// 윈도우 내 모델별 사용량 (실시간 카드용). model을 채우는 도구(현재 Claude)만 비어있지 않음.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelBreakdown {
    /// 단축 표시명 ("opus-4-8").
    pub model: String,
    /// 윈도우 누적 사용량 (토큰).
    pub usage: u64,
    /// API 기준 환산 비용 (USD). 단가 미상 도구는 0.
    pub cost: f64,
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
    /// 효율 인사이트 목록 (코칭/칭찬). 비어있을 수 있음.
    pub insights: Vec<Insight>,
    /// 요금제 추천(방향) — 주간 사용률 기반. 공식 사용률이 있는 도구만 채워질 수 있음.
    pub plan_hint: Option<Insight>,
    /// 윈도우 내 모델별 사용량 (사용량 내림차순). model을 채우는 도구만 비어있지 않음.
    pub models: Vec<ModelBreakdown>,
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
    /// 주간 한도 소진까지 예상 시간(분). 윈도우 경과 대비 소진 페이스로 계산.
    /// 리셋 전에 소진되지 않을 페이스면 None.
    pub seven_day_eta_minutes: Option<f64>,
    /// 현재 주간 윈도우의 일별 소진 분해. 주간 한도가 없는 도구는 빈 배열.
    pub weekly_days: Vec<WeeklyDay>,
    /// 카드 맨 위 한 줄 결론.
    pub verdict: Option<Verdict>,
    /// 잔여율이 공식 값이 아니라 가정한 한도 기반 추정치인지.
    pub is_estimate: bool,
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

struct CachedSamples {
    fetched_at: Instant,
    since_ms: i64,
    samples: Vec<UsageSample>,
}

/// 파싱한 샘플의 짧은 수명 캐시.
///
/// 세션 디렉토리는 수백 MB까지 자라서 매 폴링마다 재파싱하면 UI가 버벅인다.
/// 가장 넓은 윈도우를 담아두고 더 좁은 요청은 메모리 필터로 처리한다.
pub struct SampleCache {
    inner: Mutex<Option<CachedSamples>>,
    ttl: Duration,
}

impl SampleCache {
    pub const fn new(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(None),
            ttl,
        }
    }

    /// 캐시가 신선하고 요청보다 넓은 범위를 담고 있으면 필터해서 반환.
    pub fn get(&self, since_ms: i64) -> Option<Vec<UsageSample>> {
        let guard = self.inner.lock().ok()?;
        let c = guard.as_ref()?;
        if c.fetched_at.elapsed() >= self.ttl || c.since_ms > since_ms {
            return None;
        }
        Some(
            c.samples
                .iter()
                .filter(|s| s.timestamp_ms >= since_ms)
                .cloned()
                .collect(),
        )
    }

    /// 저장. 단 이미 더 넓고 신선한 캐시가 있으면 덮지 않는다
    /// (대시보드의 짧은 윈도우 폴링이 통계의 긴 윈도우 캐시를 좁히지 않도록).
    pub fn put(&self, since_ms: i64, samples: &[UsageSample]) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        let wider_exists = guard
            .as_ref()
            .is_some_and(|c| c.fetched_at.elapsed() < self.ttl && c.since_ms <= since_ms);
        if wider_exists {
            return;
        }
        *guard = Some(CachedSamples {
            fetched_at: Instant::now(),
            since_ms,
            samples: samples.to_vec(),
        });
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
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && is_recent(&entry, since_ms)
        {
            out.push(path);
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
