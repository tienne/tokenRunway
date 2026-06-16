mod providers;
mod runway;
mod settings;

use settings::Settings;

use providers::claude_code::ClaudeCodeProvider;
use providers::codex::CodexProvider;
use providers::gemini::GeminiProvider;
use providers::{RunwayStatus, UsageProvider};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};
use tauri_plugin_notification::NotificationExt;

/// 백그라운드 경보 체크 주기.
const ALERT_CHECK_SECS: u64 = 60;

/// 도구별 경보 발사 여부 (임계치 아래에서 1회만, 회복 시 리셋).
static ALERTED: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 등록된 모든 provider 목록. 새 도구는 여기에 추가한다.
fn providers() -> Vec<Box<dyn UsageProvider>> {
    vec![
        Box::new(ClaudeCodeProvider::new()),
        Box::new(CodexProvider::new()),
        Box::new(GeminiProvider::new()),
    ]
}

/// 도구별 한도(분모). OAuth 연동 전까지는 None.
/// TODO: Keychain OAuth로 Claude Code 공식 한도 수신 (P0 다음 단계).
fn default_limit(_tool: &str) -> Option<u64> {
    None
}

/// 사용 가능한 모든 도구의 런웨이 상태를 계산.
fn compute_all() -> Vec<RunwayStatus> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    providers()
        .iter()
        .filter(|p| p.available())
        .map(|p| runway::compute(p.as_ref(), now_ms, default_limit(p.tool_name())))
        .collect()
}

/// 사용 가능한 모든 도구의 런웨이 상태를 반환.
#[tauri::command]
fn get_runway() -> Vec<RunwayStatus> {
    compute_all()
}

/// 현재 설정을 반환.
#[tauri::command]
fn get_settings() -> Settings {
    settings::get()
}

/// 설정을 저장 (디스크 + 전역).
#[tauri::command]
fn set_settings(settings: Settings) {
    settings::set(settings);
}

/// 임계치 이하로 떨어진 도구에 네이티브 알림을 발사 (도구별 1회, 회복 시 재무장).
fn check_alerts(app: &AppHandle, statuses: &[RunwayStatus]) {
    let threshold = settings::alert_threshold();
    let Ok(mut alerted) = ALERTED.lock() else {
        return;
    };
    for s in statuses {
        let Some(pct) = s.percent_remaining else {
            continue;
        };
        let was_alerted = alerted.get(&s.tool).copied().unwrap_or(false);

        if pct <= threshold && !was_alerted {
            let _ = app
                .notification()
                .builder()
                .title("🛬 Token Runway 경보")
                .body(format!("{} 런웨이 {:.0}% 남음", s.tool, pct))
                .show();
            alerted.insert(s.tool.clone(), true);
        } else if pct > threshold && was_alerted {
            // 리셋 등으로 회복 → 다음 소진 시 다시 알림 가능하게 재무장
            alerted.insert(s.tool.clone(), false);
        }
    }
}

/// 가장 임박한(잔여율 최저) 도구의 %를 트레이 타이틀로 표시.
fn update_tray_title(app: &AppHandle, statuses: &[RunwayStatus]) {
    let min_pct = statuses
        .iter()
        .filter_map(|s| s.percent_remaining)
        .min_by(|a, b| a.total_cmp(b));
    let title = min_pct.map(|p| format!("{p:.0}%"));

    // 트레이 UI 갱신은 메인 스레드에서.
    let app_for_tray = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(tray) = app_for_tray.tray_by_id("main") {
            let _ = tray.set_title(title);
        }
    });
}

/// 백그라운드에서 주기적으로 경보 + 트레이 타이틀 갱신 (창이 닫혀 있어도 동작).
fn spawn_alert_loop(app: AppHandle) {
    std::thread::spawn(move || loop {
        let statuses = compute_all();
        update_tray_title(&app, &statuses);
        check_alerts(&app, &statuses);
        std::thread::sleep(Duration::from_secs(ALERT_CHECK_SECS));
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_runway,
            get_settings,
            set_settings
        ])
        .setup(|app| {
            // 메뉴바 tray — Token Runway의 기본 폼팩터.
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            // 메뉴바 전용 단색 아이콘 (template 모드 → macOS가 라이트/다크에 맞게 자동 반전)
            let tray_icon = tauri::image::Image::from_bytes(include_bytes!(
                "../icons/tray@2x.png"
            ))?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .tooltip("Token Runway")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "show" => {
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.set_focus();
                        }
                    }
                    _ => {}
                })
                .build(app)?;

            // 백그라운드 경보 루프 시작 (창이 닫혀도 메뉴바 상주 상태로 동작)
            spawn_alert_loop(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
