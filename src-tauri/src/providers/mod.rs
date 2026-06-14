//! 도구별 사용량 수집기 추상화.
//!
//! 새 AI 도구(Codex, Cursor 등)를 붙일 때는 이 모듈에 파일을 추가하고
//! `UsageProvider`를 구현한 뒤 `lib.rs`의 provider 목록에 등록하면 된다.

pub mod claude_code;

use serde::Serialize;

/// 단일 시점의 토큰 사용 샘플 (시계열의 한 점).
#[derive(Debug, Clone, Serialize)]
pub struct UsageSample {
    /// epoch milliseconds (UTC)
    pub timestamp_ms: i64,
    /// 해당 시점에 소비된 총 토큰 (input + output + cache).
    pub tokens: u64,
}

/// 도구 하나의 런웨이 상태 — UI로 그대로 전달되는 뷰 모델.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunwayStatus {
    pub tool: String,
    /// 데이터 수집 가능 여부 (예: 로그 디렉토리 존재).
    pub available: bool,
    /// 최근 5h 롤링 윈도우 누적 토큰.
    pub window_tokens: u64,
    /// 한도(분모). OAuth 연동 전에는 None → percent/eta 계산 불가.
    pub limit: Option<u64>,
    /// 남은 비율 (%). limit이 있을 때만.
    pub percent_remaining: Option<f64>,
    /// 최근 구간 소진 속도 (토큰/분).
    pub burn_rate_per_min: f64,
    /// 소진까지 예상 시간(분). limit과 burn_rate가 유효할 때만.
    pub eta_minutes: Option<f64>,
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
}
