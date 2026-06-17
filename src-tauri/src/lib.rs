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
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_positioner::{Position, WindowExt};

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

/// 사용 가능하고 비활성 처리되지 않은 도구의 런웨이 상태를 계산.
fn compute_all() -> Vec<RunwayStatus> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let disabled = settings::disabled_tools();
    providers()
        .iter()
        .filter(|p| p.available() && !disabled.iter().any(|d| d == p.tool_name()))
        .map(|p| runway::compute(p.as_ref(), now_ms, default_limit(p.tool_name())))
        .collect()
}

/// 도구 가용성 정보 (설정 UI의 on/off 토글용).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolInfo {
    tool: String,
    available: bool,
}

/// 등록된 모든 도구와 가용 여부 (비활성 필터 전).
#[tauri::command]
fn get_available_tools() -> Vec<ToolInfo> {
    providers()
        .iter()
        .map(|p| ToolInfo {
            tool: p.tool_name().to_string(),
            available: p.available(),
        })
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

/// 설정 창을 열거나 포커스. 같은 프론트(label로 화면 분기)를 일반 창으로 띄운다.
fn open_settings(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Token Runway 설정")
        .inner_size(420.0, 560.0)
        .resizable(false)
        .build();
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    open_settings(&app);
}

/// 임계치 이하로 떨어진 도구에 네이티브 알림을 발사 (도구별 1회, 회복 시 재무장).
fn check_alerts(app: &AppHandle, statuses: &[RunwayStatus]) {
    let threshold = settings::alert_threshold();
    let eta_threshold = settings::eta_alert_minutes();
    let Ok(mut alerted) = ALERTED.lock() else {
        return;
    };
    for s in statuses {
        let Some(pct) = s.percent_remaining else {
            continue;
        };
        let was_alerted = alerted.get(&s.tool).copied().unwrap_or(false);

        let pct_hit = pct <= threshold;
        let eta_hit = eta_threshold > 0.0
            && s.eta_minutes.is_some_and(|e| e <= eta_threshold);
        let should_alert = pct_hit || eta_hit;

        if should_alert && !was_alerted {
            // ETA만 걸렸으면 시간 중심 메시지, 그 외엔 잔여율 메시지
            let body = if eta_hit && !pct_hit {
                format!(
                    "{} 약 {:.0}분 후 소진 ({:.0}% 남음)",
                    s.tool,
                    s.eta_minutes.unwrap_or(0.0),
                    pct
                )
            } else {
                format!("{} 런웨이 {pct:.0}% 남음", s.tool)
            };
            let _ = app
                .notification()
                .builder()
                .title("🛬 Token Runway 경보")
                .body(body)
                .show();
            alerted.insert(s.tool.clone(), true);
        } else if !should_alert && was_alerted {
            // 리셋 등으로 회복 → 다음 소진 시 다시 알림 가능하게 재무장
            alerted.insert(s.tool.clone(), false);
        }
    }
}

/// 트레이 타이틀에 잔여율 표시.
/// 설정에 지정 도구가 있으면 그 도구를, 없으면 잔여율 최저(가장 임박)를 표시.
fn update_tray_title(app: &AppHandle, statuses: &[RunwayStatus]) {
    let pct = match settings::tray_tool() {
        Some(tool) => statuses
            .iter()
            .find(|s| s.tool == tool)
            .and_then(|s| s.percent_remaining),
        None => statuses
            .iter()
            .filter_map(|s| s.percent_remaining)
            .min_by(|a, b| a.total_cmp(b)),
    };
    let title = pct.map(|p| format!("{p:.0}%"));

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
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![
            get_runway,
            get_settings,
            set_settings,
            get_available_tools,
            open_settings_window
        ])
        // 팝오버 UX는 main 창에만 적용 (설정 창은 일반 창으로 동작).
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                WindowEvent::Focused(false) => {
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .setup(|app| {
            // Dock 아이콘 없이 메뉴바 전용 앱으로 (macOS).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // 우클릭 메뉴 (열기/설정/종료).
            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let settings_item =
                MenuItem::with_id(app, "settings", "설정...", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &settings_item, &quit])?;

            // 메뉴바 전용 단색 아이콘 (template 모드 → macOS가 라이트/다크에 맞게 자동 반전)
            let tray_icon = tauri::image::Image::from_bytes(include_bytes!(
                "../icons/tray@2x.png"
            ))?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .tooltip("Token Runway")
                .menu(&menu)
                // 좌클릭은 메뉴 대신 팝오버 토글에 쓴다.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "show" => toggle_popover(app, true),
                    "settings" => open_settings(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    let app = tray.app_handle();
                    // positioner가 트레이 위치를 기억하도록 이벤트 전달.
                    tauri_plugin_positioner::on_tray_event(app, &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_popover(app, false);
                    }
                })
                .build(app)?;

            // 백그라운드 경보 루프 시작 (창이 닫혀도 메뉴바 상주 상태로 동작)
            spawn_alert_loop(app.handle().clone());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 트레이 팝오버 토글. `force_show`면 항상 표시(메뉴 "열기"용), 아니면 토글.
fn toggle_popover(app: &AppHandle, force_show: bool) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let visible = win.is_visible().unwrap_or(false);
    if visible && !force_show {
        let _ = win.hide();
    } else {
        // 트레이 아이콘 아래 중앙에 배치 후 표시.
        let _ = win.move_window(Position::TrayBottomCenter);
        let _ = win.show();
        let _ = win.set_focus();
    }
}
