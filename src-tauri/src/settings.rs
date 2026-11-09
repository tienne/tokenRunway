//! 사용자 설정 — JSON 파일 영속화 + 전역 state.
//!
//! Rust(AlertManager)와 프론트(설정 UI)가 공유한다.
//! 저장 위치: `~/.token-runway/settings.json` (rollup.json·pet/도 같은 루트를 쓴다)

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
fn default_completion_alerts() -> bool {
    true
}
fn default_haptics_enabled() -> bool {
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
    /// `trw`로 들어오는 작업 완료 알림 수신 여부.
    #[serde(default = "default_completion_alerts")]
    pub completion_alerts_enabled: bool,
    /// 완료 알림에도 방해금지 시간대를 적용할지.
    ///
    /// 잔여율 경보와 달리 기본이 꺼짐이다 — 밤에 돌려놓고 자는 경우엔 막아야 맞지만
    /// 밤에 직접 작업하는 경우엔 막으면 알림을 놓친다. 성격이 달라 따로 둔다.
    #[serde(default)]
    pub completion_alerts_quiet: bool,
    /// 알림이 뜰 때 MX Master 4도 진동시킬지.
    ///
    /// 기본이 켜짐인 건 Logi 플러그인을 깔아야만 실제로 울리기 때문이다 — 플러그인이
    /// 없으면 소켓 연결만 실패하고 끝난다.
    #[serde(default = "default_haptics_enabled")]
    pub haptics_enabled: bool,
    /// trw 완료 알림 말고 에이전트 턴이 끝날 때마다도 진동할지. pet이 손을 흔드는 순간과 같다.
    #[serde(default = "default_haptics_enabled")]
    pub haptics_on_turn_end: bool,
    /// 앱이 꺼져 있을 때 `trw`가 알림을 보내면 앱을 띄울지. 이 값은 앱이 아니라 `trw`가
    /// settings.json을 직접 읽어서 쓴다 — 앱이 꺼져 있을 때 필요한 값이라서.
    #[serde(default = "default_completion_alerts")]
    pub wake_on_notify: bool,
    /// Codex 훅(`trw hook codex`)을 깔았는지. 켜져 있을 때만 활동 감지가 훅 상태 파일을
    /// rollout보다 먼저 본다 — 훅을 지운 뒤 남은 상태 파일이 판정을 흔들지 않게.
    #[serde(default)]
    pub codex_hooks_installed: bool,
    /// Claude statusLine 릴레이(`trw statusline`)를 받을지. 기본 꺼짐 — 사용자 settings.json의
    /// statusLine을 바꿔야 해서 직접 켠 경우에만 쓴다. 꺼져 있으면 들어온 값도 버린다.
    #[serde(default)]
    pub statusline_relay_enabled: bool,
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
    /// 최근 사용량이 0인(현재 윈도우에서 안 쓴) 도구 카드 숨김.
    #[serde(default)]
    pub hide_inactive: bool,
    /// 트레이 타이틀에 남은 토큰 비율(%) 표시.
    #[serde(default = "default_true")]
    pub tray_show_percent: bool,
    /// 트레이 타이틀에 리셋까지 남은 시간 표시.
    #[serde(default = "default_true")]
    pub tray_show_reset: bool,
    /// 데스크톱 pet 오버레이 표시 여부.
    #[serde(default = "default_true")]
    pub pet_enabled: bool,
    /// 저장해둔 커스텀 pet 스킨 목록 — 여러 개를 가져와 전환할 수 있다.
    #[serde(default)]
    pub pet_bundles: Vec<crate::pet::PetBundle>,
    /// 현재 활성 pet 스킨의 id. None이면 기본 내장 pet.
    #[serde(default)]
    pub active_pet_bundle_id: Option<String>,
    /// 종료 시점 pet 창 위치(논리 px) — 다음 실행에서 이어서 시작한다. None이면 기본 위치.
    #[serde(default)]
    pub pet_last_x: Option<f64>,
    #[serde(default)]
    pub pet_last_y: Option<f64>,
    /// pet 표시 배율 (1.0 = 기본 크기). PET_SCALE_MIN~MAX로 제한한다.
    #[serde(default = "default_pet_scale")]
    pub pet_scale: f64,
    /// 사용량 API가 토큰 만료(401)를 돌려준 Codex 계정만 `codex app-server`로 다시 묻는다.
    #[serde(default = "default_true")]
    pub codex_app_server_fallback: bool,
    /// 노치 양옆에 잔여율과 활동 상태를 띄울지. 메뉴바 위를 덮어서 기본은 꺼짐이다.
    #[serde(default)]
    pub notch_enabled: bool,
    /// 트레이 타이틀에 숫자를 띄울지. 기본은 노치가 보이면 숨기는 자동.
    #[serde(default)]
    pub tray_title_mode: TrayTitleMode,
}

/// 트레이 타이틀 숫자 표시 규칙.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayTitleMode {
    /// 메뉴바가 있는 주 화면에 노치 섬이 떠 있으면 숨기고, 아니면 띄운다.
    #[default]
    Auto,
    Always,
    IconOnly,
}

/// pet 배율 허용 범위 — 너무 작으면 못 잡고, 너무 크면 화면을 가린다.
pub const PET_SCALE_MIN: f64 = 0.5;
pub const PET_SCALE_MAX: f64 = 2.5;

fn default_pet_scale() -> f64 {
    1.0
}

fn default_true() -> bool {
    true
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
            hide_inactive: false,
            tray_show_percent: true,
            tray_show_reset: true,
            pet_enabled: true,
            pet_bundles: Vec::new(),
            active_pet_bundle_id: None,
            pet_last_x: None,
            pet_last_y: None,
            pet_scale: 1.0,
            codex_app_server_fallback: true,
            notch_enabled: false,
            tray_title_mode: TrayTitleMode::Auto,
            completion_alerts_enabled: default_completion_alerts(),
            completion_alerts_quiet: false,
            haptics_enabled: default_haptics_enabled(),
            haptics_on_turn_end: default_haptics_enabled(),
            wake_on_notify: default_completion_alerts(),
            codex_hooks_installed: false,
            statusline_relay_enabled: false,
        }
    }
}

static SETTINGS: LazyLock<Mutex<Settings>> = LazyLock::new(|| Mutex::new(load_from_disk()));

/// 앱 데이터 루트 — settings.json·rollup.json·pet/이 모두 이 아래에 있다.
pub fn app_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".token-runway"))
}

fn settings_path() -> Option<PathBuf> {
    app_dir().map(|d| d.join("settings.json"))
}

/// 예전 위치(`<OS config dir>/token-runway`)에 데이터가 있고 새 위치(`~/.token-runway`)가
/// 아직 없으면 통째로 옮긴다. `SETTINGS`/`ROLLUP` 등 어떤 static도 건드리기 전, `run()`
/// 맨 앞에서 한 번만 호출해야 한다 — 늦게 부르면 이미 빈 새 위치에서 읽어버린 뒤라 늦는다.
pub fn migrate_data_dir() {
    let Some(old) = dirs::config_dir().map(|d| d.join("token-runway")) else {
        return;
    };
    let Some(new) = app_dir() else { return };
    if new.exists() || !old.exists() {
        return;
    }
    let _ = fs::rename(&old, &new);
}

fn load_from_disk() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Settings::default();
    };
    match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(_) => {
            // 깨진 설정을 조용히 기본값으로 덮으면 사용자가 잃은 줄도 모른다.
            crate::atomicfile::preserve_corrupt(&path);
            Settings::default()
        }
    }
}

/// 현재 설정 사본.
pub fn get() -> Settings {
    SETTINGS.lock().map(|s| s.clone()).unwrap_or_default()
}

/// 설정 저장 (디스크 + 전역 state).
pub fn set(new: Settings) {
    if let Some(path) = settings_path() {
        if let Ok(json) = serde_json::to_string_pretty(&new) {
            let _ = crate::atomicfile::write_atomic(&path, &json);
        }
    }
    if let Ok(mut s) = SETTINGS.lock() {
        *s = new;
    }
}

/// `trw` 완료 알림 수신 여부.
pub fn completion_alerts_enabled() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.completion_alerts_enabled)
        .unwrap_or(true)
}

/// 완료 알림에 방해금지를 적용할지.
pub fn completion_alerts_quiet() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.completion_alerts_quiet)
        .unwrap_or(false)
}

/// 알림과 함께 마우스 진동을 보낼지.
pub fn haptics_enabled() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.haptics_enabled)
        .unwrap_or(true)
}

/// Codex 훅 상태 파일을 활동 감지에 쓸지.
pub fn codex_hooks_installed() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.codex_hooks_installed)
        .unwrap_or(false)
}

/// statusLine 릴레이 값을 받아 쓸지.
pub fn statusline_relay_enabled() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.statusline_relay_enabled)
        .unwrap_or(false)
}

/// 턴 종료 때도 마우스 진동을 보낼지.
pub fn haptics_on_turn_end() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.haptics_on_turn_end)
        .unwrap_or(true)
}

pub fn codex_app_server_fallback() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.codex_app_server_fallback)
        .unwrap_or(true)
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
    SETTINGS
        .lock()
        .map(|s| s.disabled_tools.clone())
        .unwrap_or_default()
}

pub fn notch_enabled() -> bool {
    SETTINGS.lock().map(|s| s.notch_enabled).unwrap_or(false)
}

pub fn tray_title_mode() -> TrayTitleMode {
    SETTINGS
        .lock()
        .map(|s| s.tray_title_mode)
        .unwrap_or_default()
}

/// 비활성(최근 사용량 0) 도구 숨김 여부.
pub fn hide_inactive() -> bool {
    SETTINGS.lock().map(|s| s.hide_inactive).unwrap_or(false)
}

/// 트레이에 남은 비율(%) 표시 여부.
pub fn tray_show_percent() -> bool {
    SETTINGS.lock().map(|s| s.tray_show_percent).unwrap_or(true)
}

/// 트레이에 리셋까지 시간 표시 여부.
pub fn tray_show_reset() -> bool {
    SETTINGS.lock().map(|s| s.tray_show_reset).unwrap_or(true)
}

/// 트레이 표시 도구 (None이면 자동).
pub fn tray_tool() -> Option<String> {
    SETTINGS.lock().ok().and_then(|s| s.tray_tool.clone())
}

/// 리셋 임박 알림 임계치(분). 0이면 비활성.
pub fn reset_alert_minutes() -> f64 {
    SETTINGS
        .lock()
        .map(|s| s.reset_alert_minutes)
        .unwrap_or(0.0)
}

/// OS 알림 마스터 스위치.
pub fn notifications_enabled() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.notifications_enabled)
        .unwrap_or(true)
}

/// 설정 언어 (None이면 auto).
pub fn language() -> Option<String> {
    SETTINGS.lock().ok().and_then(|s| s.language.clone())
}

/// 익명 분석 활성화 여부.
pub fn analytics_enabled() -> bool {
    SETTINGS
        .lock()
        .map(|s| s.analytics_enabled)
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tray_title_mode_means_auto() {
        let s: Settings = serde_json::from_str(r#"{"alertThreshold": 15}"#).unwrap();
        assert_eq!(s.tray_title_mode, TrayTitleMode::Auto);
    }

    #[test]
    fn tray_title_mode_round_trips_as_snake_case() {
        let s: Settings = serde_json::from_str(r#"{"trayTitleMode": "icon_only"}"#).unwrap();
        assert_eq!(s.tray_title_mode, TrayTitleMode::IconOnly);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["trayTitleMode"], "icon_only");
    }
}
