//! 앱 업데이트 — 시작할 때와 그 뒤 6시간마다 확인하고, 찾은 버전을 노치와 설정, 트레이
//! 메뉴에 "vX 설치"로 띄운다.
//!
//! 예전엔 시작할 때 한 번만 확인했다. 메뉴바 앱은 몇 주씩 안 껐다 켜서 그 사이에 나온
//! 버전을 영영 못 봤고, 찾아도 누를 데 없는 OS 알림 한 번이 전부라 지나가면 끝이었다.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// 잠자기 동안엔 스레드 sleep 시계가 멈춰서 6시간을 한 번에 자면 실제로는 며칠 뒤에
/// 깬다. 짧게 자고 벽시계로 지난 시간을 잰다.
const TICK: Duration = Duration::from_secs(10 * 60);

/// 지금 받을 수 있는 버전. 노치와 설정의 설치 버튼이 이 값으로 뜬다.
static AVAILABLE: Mutex<Option<String>> = Mutex::new(None);
/// 알림을 이미 띄운 버전 — 6시간마다 같은 버전으로 다시 울리지 않게.
static NOTIFIED: Mutex<Option<String>> = Mutex::new(None);
static INSTALLING: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub available: Option<String>,
    pub installing: bool,
}

pub fn status() -> UpdateStatus {
    UpdateStatus {
        available: AVAILABLE.lock().ok().and_then(|g| g.clone()),
        installing: INSTALLING.load(Ordering::Relaxed),
    }
}

/// 트레이 메뉴 라벨용 — 받을 버전이 있으면 그 버전.
pub fn available_version() -> Option<String> {
    AVAILABLE.lock().ok().and_then(|g| g.clone())
}

fn publish(app: &AppHandle) {
    let _ = app.emit("update-status", status());
    crate::refresh_localized_ui(app);
}

fn set_available(app: &AppHandle, version: Option<String>) {
    let changed = match AVAILABLE.lock() {
        Ok(mut g) if *g != version => {
            *g = version;
            true
        }
        _ => false,
    };
    if changed {
        publish(app);
    }
}

/// 처음 보는 버전이면 기록하고 true.
fn first_sighting(notified: &mut Option<String>, version: &str) -> bool {
    if notified.as_deref() == Some(version) {
        return false;
    }
    *notified = Some(version.to_string());
    true
}

/// 시작 직후 한 번, 그 뒤로 6시간마다 확인한다.
pub fn spawn_checker(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last: Option<SystemTime> = None;
        loop {
            let due = last
                .and_then(|t| t.elapsed().ok())
                .map_or(true, |e| e >= CHECK_INTERVAL);
            if due {
                tauri::async_runtime::block_on(check_in_background(&app));
                last = Some(SystemTime::now());
            }
            std::thread::sleep(TICK);
        }
    });
}

/// 조용한 확인 — 찾으면 버튼을 띄우고 처음 보는 버전일 때만 알린다. 실패하면 넘어간다.
async fn check_in_background(app: &AppHandle) {
    if INSTALLING.load(Ordering::Relaxed) {
        return;
    }
    let Ok(updater) = app.updater() else { return };
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            set_available(app, Some(version.clone()));
            let fresh = NOTIFIED
                .lock()
                .map(|mut g| first_sighting(&mut g, &version))
                .unwrap_or(false);
            if fresh {
                notify_available(app, &version);
            }
        }
        Ok(None) => set_available(app, None),
        Err(_) => {}
    }
}

/// 누르면 바로 설치하는 알림.
fn notify_available(app: &AppHandle, version: &str) {
    let lang = crate::i18n::current();
    let title = format!("{} v{version}", lang.update_available_title());
    let handle = app.clone();
    crate::notify_os::show_clickable(title, lang.update_available_body().to_string(), move || {
        tauri::async_runtime::spawn(async move { install(handle).await });
    });
}

fn plain_notice(app: &AppHandle, body: &str) {
    let _ = app
        .notification()
        .builder()
        .title("Token Runway")
        .body(body)
        .show();
}

/// 사람이 고른 확인이자 설치 — 트레이 메뉴, 설정, 노치 버튼, 알림 클릭이 여기로 온다.
/// 최신이거나 실패해도 결과를 알린다. 누른 사람은 답을 기다리고 있어서다.
pub async fn install(app: AppHandle) {
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return;
    }
    publish(&app);
    let lang = crate::i18n::current();

    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(Some(update)) => {
            set_available(&app, Some(update.version.clone()));
            plain_notice(&app, lang.update_installing());
            crate::set_tray_mode(crate::TRAY_MODE_CHARGE);
            if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                app.restart();
            }
            // 재시작했다면 여기 안 온다.
            crate::restore_tray_mode();
            plain_notice(&app, lang.update_failed());
        }
        Ok(None) => {
            set_available(&app, None);
            plain_notice(&app, lang.update_uptodate());
        }
        Err(_) => plain_notice(&app, lang.update_failed()),
    }
    INSTALLING.store(false, Ordering::SeqCst);
    publish(&app);
}

#[tauri::command]
pub fn get_update_status() -> UpdateStatus {
    status()
}

#[tauri::command]
pub fn install_update(app: AppHandle) {
    tauri::async_runtime::spawn(async move { install(app).await });
}

#[cfg(test)]
mod tests {
    use super::first_sighting;

    #[test]
    fn notifies_each_version_once() {
        let mut seen = None;
        assert!(first_sighting(&mut seen, "0.6.1"));
        assert!(!first_sighting(&mut seen, "0.6.1"));
        assert!(first_sighting(&mut seen, "0.7.0"));
        assert!(!first_sighting(&mut seen, "0.7.0"));
    }
}
