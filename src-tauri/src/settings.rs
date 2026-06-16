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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// 잔여율이 이 % 이하로 떨어지면 경보.
    #[serde(default = "default_threshold")]
    pub alert_threshold: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            alert_threshold: default_threshold(),
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
    SETTINGS.lock().map(|s| s.alert_threshold).unwrap_or_else(|_| default_threshold())
}
