//! 사용자 설정 — JSON 파일 영속화 + 전역 state.
//!
//! Rust(AlertManager)와 프론트(설정 UI)가 공유한다.
//! 저장 위치: `<config_dir>/token-runway/settings.json`

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

fn default_threshold() -> f64 {
    20.0
}
fn default_eta_alert_minutes() -> f64 {
    30.0
}
fn default_poll_seconds() -> u64 {
    30
}
fn default_reset_alert_minutes() -> f64 {
    0.0
}
fn default_notifications_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// 잔여율이 이 % 이하로 떨어지면 경보.
    #[serde(default = "default_threshold")]
    pub alert_threshold: f64,
    /// 예상 소진까지 이 분(分) 이하면 경보. 0이면 ETA 경보 비활성.
    #[serde(default = "default_eta_alert_minutes")]
    pub eta_alert_minutes: f64,
    /// UI 새로고침 주기(초).
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// 모니터링에서 제외할 도구명 목록.
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// 트레이에 표시할 도구. None이면 잔여율 최저(가장 임박) 자동 선택.
    #[serde(default)]
    pub tray_tool: Option<String>,
    /// 리셋까지 이 분(分) 이하이고 잔여가 충분하면 "남은 토큰 활용" 알림. 0이면 비활성.
    #[serde(default = "default_reset_alert_minutes")]
    pub reset_alert_minutes: f64,
    /// 모든 OS 알림 마스터 스위치.
    #[serde(default = "default_notifications_enabled")]
    pub notifications_enabled: bool,
    /// UI/알림 언어. None이면 시스템 로케일 자동 감지 ("ko" | "en").
    #[serde(default)]
    pub language: Option<String>,
    /// 방해금지 시간대 사용 여부.
    #[serde(default)]
    pub quiet_enabled: bool,
    /// 방해금지 시작 시각(0~23시).
    #[serde(default = "default_quiet_start")]
    pub quiet_start_hour: u32,
    /// 방해금지 종료 시각(0~23시).
    #[serde(default = "default_quiet_end")]
    pub quiet_end_hour: u32,
    /// 익명 사용 통계 공유 (opt-in, 기본 꺼짐).
    #[serde(default)]
    pub analytics_enabled: bool,
    /// 익명 분석용 랜덤 ID (개인 식별 불가). 최초 1회 생성.
    #[serde(default)]
    pub anon_id: Option<String>,
}

fn default_quiet_start() -> u32 {
    22
}
fn default_quiet_end() -> u32 {
    8
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alert_threshold: default_threshold(),
            eta_alert_minutes: default_eta_alert_minutes(),
            poll_seconds: default_poll_seconds(),
            disabled_tools: Vec::new(),
            tray_tool: None,
            reset_alert_minutes: default_reset_alert_minutes(),
            notifications_enabled: default_notifications_enabled(),
            language: None,
            quiet_enabled: false,
            quiet_start_hour: default_quiet_start(),
            quiet_end_hour: default_quiet_end(),
            analytics_enabled: false,
            anon_id: None,
        }
    }
}

static SETTINGS: LazyLock<Mutex<Settings>> = LazyLock::new(|| Mutex::new(load_from_disk()));

fn settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("token-runway").join("settings.json"))
}

fn load_from_disk() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 현재 설정 사본.
pub fn get() -> Settings {
    SETTINGS.lock().map(|s| s.clone()).unwrap_or_default()
}

/// 설정 저장 (디스크 + 전역 state).
pub fn set(new: Settings) {
    if let Some(path) = settings_path() {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&new) {
            let _ = fs::write(path, json);
        }
    }
    if let Ok(mut s) = SETTINGS.lock() {
        *s = new;
    }
}

/// 경보 임계치 (%).
pub fn alert_threshold() -> f64 {
    SETTINGS
        .lock()
        .map(|s| s.alert_threshold)
        .unwrap_or_else(|_| default_threshold())
}

/// ETA 경보 임계치(분). 0이면 비활성.
pub fn eta_alert_minutes() -> f64 {
    SETTINGS
        .lock()
        .map(|s| s.eta_alert_minutes)
        .unwrap_or_else(|_| default_eta_alert_minutes())
}

/// 모니터링 제외 도구 목록.
pub fn disabled_tools() -> Vec<String> {
    SETTINGS.lock().map(|s| s.disabled_tools.clone()).unwrap_or_default()
}

/// 트레이 표시 도구 (None이면 자동).
pub fn tray_tool() -> Option<String> {
    SETTINGS.lock().ok().and_then(|s| s.tray_tool.clone())
}

/// 리셋 임박 알림 임계치(분). 0이면 비활성.
pub fn reset_alert_minutes() -> f64 {
    SETTINGS.lock().map(|s| s.reset_alert_minutes).unwrap_or(0.0)
}

/// OS 알림 마스터 스위치.
pub fn notifications_enabled() -> bool {
    SETTINGS.lock().map(|s| s.notifications_enabled).unwrap_or(true)
}

/// 설정 언어 (None이면 auto).
pub fn language() -> Option<String> {
    SETTINGS.lock().ok().and_then(|s| s.language.clone())
}

/// 익명 분석 활성화 여부.
pub fn analytics_enabled() -> bool {
    SETTINGS.lock().map(|s| s.analytics_enabled).unwrap_or(false)
}

/// 익명 ID — 없으면 생성·저장 후 반환.
pub fn anon_id() -> String {
    if let Some(id) = SETTINGS.lock().ok().and_then(|s| s.anon_id.clone()) {
        return id;
    }
    // 의사난수 기반 익명 ID 생성 (개인 식별 불가).
    let id = generate_anon_id();
    let mut current = get();
    current.anon_id = Some(id.clone());
    set(current);
    id
}

fn generate_anon_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("anon-{nanos:x}")
}
