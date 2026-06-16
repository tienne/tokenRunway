mod providers;
mod runway;

use providers::claude_code::ClaudeCodeProvider;
use providers::codex::CodexProvider;
use providers::{RunwayStatus, UsageProvider};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

/// 등록된 모든 provider 목록. 새 도구는 여기에 추가한다.
fn providers() -> Vec<Box<dyn UsageProvider>> {
    vec![
        Box::new(ClaudeCodeProvider::new()),
        Box::new(CodexProvider::new()),
    ]
}

/// 도구별 한도(분모). OAuth 연동 전까지는 None.
/// TODO: Keychain OAuth로 Claude Code 공식 한도 수신 (P0 다음 단계).
fn default_limit(_tool: &str) -> Option<u64> {
    None
}

/// 사용 가능한 모든 도구의 런웨이 상태를 반환.
#[tauri::command]
fn get_runway() -> Vec<RunwayStatus> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    providers()
        .iter()
        .filter(|p| p.available())
        .map(|p| runway::compute(p.as_ref(), now_ms, default_limit(p.tool_name())))
        .collect()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![get_runway])
        .setup(|app| {
            // 메뉴바 tray — Token Runway의 기본 폼팩터.
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
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

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
