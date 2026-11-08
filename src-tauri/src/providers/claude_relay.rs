//! Claude Code statusLine 릴레이 — `trw statusline`이 넘긴 rate_limits.
//!
//! OAuth 사용량 폴링(180초)의 보조 경로다. statusLine은 응답마다 불려서 OAuth보다 자주
//! 최신 값을 준다. 폴링을 대체하지 않고, 받은 값이 OAuth 값보다 늦게 왔을 때만 그
//! 계정의 공식 사용률로 쓴다.

use serde::Deserialize;

/// 소켓으로 온 릴레이 한 건. `trw`의 `relay_payload`와 모양이 같다.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayMessage {
    /// 훅 프로세스의 `CLAUDE_CONFIG_DIR`(없으면 `~/.claude`). 계정을 찾는 열쇠다.
    pub config_dir: String,
    pub rate_limits: RelayLimits,
    #[serde(default)]
    pub sent_ms: Option<i64>,
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
