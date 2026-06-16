//! 도구별 사용량 수집기 추상화.
//!
//! 새 AI 도구(Codex, Cursor 등)를 붙일 때는 이 모듈에 파일을 추가하고
//! `UsageProvider`를 구현한 뒤 `lib.rs`의 provider 목록에 등록하면 된다.

pub mod claude_code;
pub mod codex;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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
