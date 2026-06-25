mod analytics;
mod i18n;
mod providers;
mod rollup;
mod runway;
mod settings;

use settings::Settings;

use providers::antigravity::AntigravityProvider;
use providers::claude_code::ClaudeCodeProvider;
use providers::codex::CodexProvider;
use providers::gemini::GeminiProvider;
use providers::{RunwayStatus, UsageProvider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_positioner::{Position, WindowExt};

/// 백그라운드 경보 체크 주기.
const ALERT_CHECK_SECS: u64 = 60;

/// 트레이 아이콘 — 평소(단색 template) / 위험(빨강).
const TRAY_NORMAL_ICON: &[u8] = include_bytes!("../icons/tray@2x.png");
const TRAY_ALERT_ICON: &[u8] = include_bytes!("../icons/tray-alert@2x.png");

/// 직전 위험 상태 — 변할 때만 아이콘을 교체(깜빡임 방지).
static TRAY_DANGER: AtomicBool = AtomicBool::new(false);

/// 도구별 소진 경보 발사 여부 (임계치 아래에서 1회만, 회복 시 리셋).
static ALERTED: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 도구별 리셋 임박 경보 발사 여부 (윈도우당 1회).
static RESET_ALERTED: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 등록된 모든 provider 목록. 새 도구는 여기에 추가한다.
fn providers() -> Vec<Box<dyn UsageProvider>> {
    vec![
        Box::new(ClaudeCodeProvider::new()),
        Box::new(CodexProvider::new()),
        Box::new(GeminiProvider::new()),
        Box::new(AntigravityProvider::new()),
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
    let hide_inactive = settings::hide_inactive();
    providers()
        .iter()
        .filter(|p| p.available() && !disabled.iter().any(|d| d == p.tool_name()))
        .map(|p| runway::compute(p.as_ref(), now_ms, default_limit(p.tool_name())))
        // 사용 중인 도구만 표시 옵션: 현재 윈도우 사용량이 0이면 숨김
        .filter(|s| !hide_inactive || s.window_usage > 0)
        .collect()
}

/// 도구 가용성 정보 (설정 UI의 on/off 토글용).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolInfo {
    tool: String,
    available: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DayStat {
    date: String, // "MM/DD"
    usage: u64,
    count: u64,
    cost: f64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsage {
    model: String, // 단축 표시명 ("opus-4-8")
    usage: u64,
    cost: f64,
    count: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolStats {
    tool: String,
    unit: String,
    /// 선택 기간 일별 사용량 (오름차순).
    days: Vec<DayStat>,
    total_usage: u64,
    total_cost: f64,
    /// 활동일(사용량>0) 평균.
    avg_usage: u64,
    peak_date: String,
    peak_usage: u64,
    /// 모델별 분해 (사용량 내림차순). Claude만 채워짐.
    models: Vec<ModelUsage>,
    /// 시간대(0~23시)별 사용량.
    hourly: Vec<u64>,
    /// 최근 7일 vs 직전 7일 비교.
    this_week_usage: u64,
    last_week_usage: u64,
    this_week_cost: f64,
    last_week_cost: f64,
}

/// epoch millis → 로컬 "YYYY-MM-DD".
pub(crate) fn local_date_full(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// epoch millis → 로컬 시(0~23).
pub(crate) fn local_hour(ms: i64) -> usize {
    use chrono::{TimeZone, Timelike};
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.hour() as usize)
        .unwrap_or(0)
}

/// 모델 문자열 단축 ("claude-opus-4-8" → "opus-4-8", 날짜 suffix 제거).
pub(crate) fn short_model(m: &str) -> String {
    let base = m.strip_prefix("claude-").unwrap_or(m);
    let parts: Vec<&str> = base
        .split('-')
        .filter(|p| !(p.len() == 8 && p.chars().all(|c| c.is_ascii_digit())))
        .collect();
    if parts.is_empty() {
        m.to_string()
    } else {
        parts.join("-")
    }
}

/// 최근 `days`일 통계 — 일별·모델별·시간대별·주간 비교 (JSONL 원본 집계, 영속 저장 없음).
///
/// 수백 MB 파싱이 걸릴 수 있어 blocking 스레드 풀에서 실행한다.
/// 동기 command로 두면 메인 이벤트 루프(웹뷰 페인트 포함)가 멈춰 UI가 프리즈된다.
#[tauri::command]
async fn get_stats(days: u32) -> Vec<ToolStats> {
    tauri::async_runtime::spawn_blocking(move || compute_stats(days))
        .await
        .unwrap_or_default()
}

/// 통계 집계 — 과거는 영속 롤업, 오늘(미확정)은 실시간 파싱해 병합한다.
/// `days == 0`이면 롤업 전체 기간.
fn compute_stats(days: u32) -> Vec<ToolStats> {
    use std::collections::{BTreeMap, HashMap};

    let now_ms = chrono::Utc::now().timestamp_millis();
    let day = 24 * 3600 * 1000i64;
    let today = local_date_full(now_ms);
    let today_start = rollup::date_start_ms(&today).unwrap_or(now_ms - day);

    // 날짜 문자열 비교 경계 (YYYY-MM-DD는 사전순=시간순).
    let window_start_date = if days == 0 {
        String::new() // 전체
    } else {
        local_date_full(now_ms - (days as i64 - 1) * day)
    };
    let week_start_date = local_date_full(now_ms - 6 * day); // 최근 7일(오늘 포함)
    let prev_week_start_date = local_date_full(now_ms - 13 * day); // 직전 7일

    let rollup = rollup::get();
    let disabled = settings::disabled_tools();

    providers()
        .iter()
        .filter(|p| p.available() && !disabled.iter().any(|d| d == p.tool_name()))
        .map(|p| {
            // 과거(롤업) + 오늘(실시간) 통합 일별 맵
            let mut all: BTreeMap<String, rollup::DayRollup> =
                rollup.get(p.tool_name()).cloned().unwrap_or_default();

            // 오늘 실시간 집계 (롤업엔 오늘이 없다)
            let mut today_roll = rollup::DayRollup {
                hourly: vec![0; 24],
                ..Default::default()
            };
            for s in p.collect_samples(today_start) {
                if local_date_full(s.timestamp_ms) != today {
                    continue;
                }
                today_roll.usage += s.amount;
                today_roll.count += 1;
                today_roll.cost += s.cost_usd;
                today_roll.hourly[local_hour(s.timestamp_ms)] += s.amount;
                if let Some(m) = &s.model {
                    let me = today_roll.models.entry(short_model(m)).or_default();
                    me.0 += s.amount;
                    me.1 += s.cost_usd;
                    me.2 += 1;
                }
            }
            all.insert(today.clone(), today_roll);

            // 선택 기간 윈도우 내 날들
            let in_window: Vec<(&String, &rollup::DayRollup)> = all
                .iter()
                .filter(|(d, _)| d.as_str() >= window_start_date.as_str())
                .collect();

            let days_vec: Vec<DayStat> = in_window
                .iter()
                .map(|(d, r)| DayStat {
                    date: d.get(5..).unwrap_or(d).replace('-', "/"),
                    usage: r.usage,
                    count: r.count,
                    cost: r.cost,
                })
                .collect();

            let total_usage: u64 = days_vec.iter().map(|d| d.usage).sum();
            let total_cost: f64 = days_vec.iter().map(|d| d.cost).sum();
            let active_days = days_vec.iter().filter(|d| d.usage > 0).count() as u64;
            let avg_usage = if active_days > 0 {
                total_usage / active_days
            } else {
                0
            };
            let (peak_date, peak_usage) = days_vec
                .iter()
                .max_by_key(|d| d.usage)
                .map(|d| (d.date.clone(), d.usage))
                .unwrap_or_default();

            // 모델·시간대: 윈도우 내 합산
            let mut model_map: HashMap<String, (u64, f64, u64)> = HashMap::new();
            let mut hourly = vec![0u64; 24];
            for (_, r) in &in_window {
                for (m, (u, c, n)) in &r.models {
                    let e = model_map.entry(m.clone()).or_default();
                    e.0 += u;
                    e.1 += c;
                    e.2 += n;
                }
                for (h, v) in r.hourly.iter().enumerate().take(24) {
                    hourly[h] += v;
                }
            }
            let mut models: Vec<ModelUsage> = model_map
                .into_iter()
                .map(|(model, (usage, cost, count))| ModelUsage {
                    model,
                    usage,
                    cost,
                    count,
                })
                .collect();
            models.sort_by(|a, b| b.usage.cmp(&a.usage));

            // 주간 비교 (기간 토글과 무관, 항상 최근 7일 vs 직전 7일)
            let (mut this_week_usage, mut last_week_usage) = (0u64, 0u64);
            let (mut this_week_cost, mut last_week_cost) = (0f64, 0f64);
            for (d, r) in &all {
                if d.as_str() > today.as_str() {
                    continue;
                }
                if d.as_str() >= week_start_date.as_str() {
                    this_week_usage += r.usage;
                    this_week_cost += r.cost;
                } else if d.as_str() >= prev_week_start_date.as_str() {
                    last_week_usage += r.usage;
                    last_week_cost += r.cost;
                }
            }

            ToolStats {
                tool: p.tool_name().to_string(),
                unit: p.unit().to_string(),
                days: days_vec,
                total_usage,
                total_cost,
                avg_usage,
                peak_date,
                peak_usage,
                models,
                hourly,
                this_week_usage,
                last_week_usage,
                this_week_cost,
                last_week_cost,
            }
        })
        .collect()
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

/// 설정을 저장 (디스크 + 전역). 언어 변경에 대비해 트레이/제목을 다시 현지화.
#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) {
    settings::set(settings);
    refresh_localized_ui(&app);
    // 다른 창(대시보드)이 즉시 언어·설정을 다시 읽도록 브로드캐스트.
    let _ = app.emit("settings-changed", ());
}

/// 창을 닫을 때 파괴하지 않고 숨긴다 — 재오픈 시 웹뷰 재부팅(OS 로딩) 회피.
fn hide_on_close(win: &tauri::WebviewWindow) {
    let w = win.clone();
    win.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = w.hide();
        }
    });
}

/// 설정 창을 열거나 포커스. 같은 프론트(label로 화면 분기)를 일반 창으로 띄운다.
fn open_settings(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    if let Ok(win) = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title(i18n::current().settings_title())
        .inner_size(420.0, 640.0)
        .min_inner_size(420.0, 480.0)
        .resizable(true)
        .title_bar_style(tauri::TitleBarStyle::Visible)
        .build()
    {
        hide_on_close(&win);
    }
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    open_settings(&app);
}

/// 히스토리 창을 열거나 포커스.
fn open_history(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("history") {
        let _ = win.show();
        let _ = win.set_focus();
        // 숨겨뒀던 창 재사용 시 마운트가 다시 안 일어나므로 갱신을 트리거.
        let _ = win.emit("history-refresh", ());
        return;
    }
    if let Ok(win) = WebviewWindowBuilder::new(app, "history", WebviewUrl::App("index.html".into()))
        .title(i18n::current().history_title())
        .inner_size(460.0, 600.0)
        .min_inner_size(420.0, 480.0)
        .resizable(true)
        .title_bar_style(tauri::TitleBarStyle::Visible)
        .build()
    {
        hide_on_close(&win);
    }
}

#[tauri::command]
fn open_history_window(app: AppHandle) {
    open_history(&app);
}

/// 프론트에서 익명 이벤트 전송 (opt-in일 때만 실제 전송됨).
/// 주의: properties에 토큰 값·잔여율 등 내용은 절대 넣지 말 것 (행동 메타만).
#[tauri::command]
fn track_event(event: String, properties: Option<serde_json::Value>) {
    analytics::track(&event, properties.unwrap_or_else(|| serde_json::json!({})));
}

/// 현재 언어로 트레이 메뉴를 구성.
fn build_tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let lang = i18n::current();
    let show = MenuItem::with_id(app, "show", lang.menu_open(), true, None::<&str>)?;
    let history_item =
        MenuItem::with_id(app, "history", lang.menu_history(), true, None::<&str>)?;
    let settings_item =
        MenuItem::with_id(app, "settings", lang.menu_settings(), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", lang.menu_quit(), true, None::<&str>)?;
    Menu::with_items(app, &[&show, &history_item, &settings_item, &quit])
}

/// 언어 변경 등으로 트레이 메뉴·설정 창 제목을 현재 언어로 다시 적용.
fn refresh_localized_ui(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if let Ok(menu) = build_tray_menu(&app) {
            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_menu(Some(menu));
            }
        }
        if let Some(win) = app.get_webview_window("settings") {
            let _ = win.set_title(i18n::current().settings_title());
        }
    });
}

/// 임계치 이하로 떨어진 도구에 네이티브 알림을 발사 (도구별 1회, 회복 시 재무장).
/// 현재 시각이 방해금지 시간대인지 (자정을 넘는 범위도 처리).
fn is_quiet_now() -> bool {
    use chrono::Timelike;
    let s = settings::get();
    if !s.quiet_enabled {
        return false;
    }
    let h = chrono::Local::now().hour();
    let (start, end) = (s.quiet_start_hour, s.quiet_end_hour);
    if start <= end {
        h >= start && h < end
    } else {
        // 예: 22~08시 (자정 넘김)
        h >= start || h < end
    }
}

fn check_alerts(app: &AppHandle, statuses: &[RunwayStatus]) {
    if !settings::notifications_enabled() || is_quiet_now() {
        return;
    }
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
            let lang = i18n::current();
            // ETA만 걸렸으면 시간 중심 메시지, 그 외엔 잔여율 메시지
            let body = if eta_hit && !pct_hit {
                lang.alert_eta(&s.tool, s.eta_minutes.unwrap_or(0.0), pct)
            } else {
                lang.alert_low(&s.tool, pct)
            };
            let _ = app
                .notification()
                .builder()
                .title(lang.alert_title())
                .body(body)
                .show();
            // 익명: 어떤 종류 경보가 떴는지 메타만 (값 X)
            analytics::track(
                "alert_fired",
                serde_json::json!({ "kind": if eta_hit && !pct_hit { "eta" } else { "low" } }),
            );
            alerted.insert(s.tool.clone(), true);
        } else if !should_alert && was_alerted {
            // 리셋 등으로 회복 → 다음 소진 시 다시 알림 가능하게 재무장
            alerted.insert(s.tool.clone(), false);
        }
    }
}

/// 트레이 타이틀 + 아이콘 갱신.
/// 설정에 지정 도구가 있으면 그 도구를, 없으면 잔여율 최저(가장 임박)를 표시.
/// 임계치 이하면 빨간 경고 아이콘으로 교체한다.
fn update_tray_title(app: &AppHandle, statuses: &[RunwayStatus]) {
    // 표시 대상 도구: 지정값 우선, 없으면 잔여율 최저(가장 임박).
    let status = match settings::tray_tool() {
        Some(tool) => statuses.iter().find(|s| s.tool == tool),
        None => statuses
            .iter()
            .filter(|s| s.percent_remaining.is_some())
            .min_by(|a, b| {
                a.percent_remaining
                    .unwrap_or(f64::MAX)
                    .total_cmp(&b.percent_remaining.unwrap_or(f64::MAX))
            }),
    };
    let pct = status.and_then(|s| s.percent_remaining);
    // 설정에 따라 비율·시간을 조합. 둘 다면 2줄(\n)로 시도.
    let show_pct = settings::tray_show_percent();
    let show_reset = settings::tray_show_reset();
    let title = status.and_then(|s| {
        let mut parts: Vec<String> = Vec::new();
        if show_pct {
            if let Some(p) = s.percent_remaining {
                parts.push(format!("{p:.0}%"));
            }
        }
        if show_reset {
            if let Some(mins) = s.resets_at.as_deref().and_then(minutes_until) {
                if mins >= 1.0 {
                    parts.push(fmt_tray_duration(mins));
                }
            }
        }
        if parts.is_empty() {
            None
        } else {
            // macOS 메뉴바는 1줄만 렌더 — 공백으로 구분 (예: "23% 2.5h").
            Some(parts.join(" "))
        }
    });
    let danger = pct.is_some_and(|p| p <= settings::alert_threshold());

    // 트레이 UI 갱신은 메인 스레드에서.
    let app_for_tray = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = app_for_tray.tray_by_id("main") else {
            return;
        };
        let _ = tray.set_title(title);

        // 위험 상태가 바뀔 때만 아이콘 교체 (위험=빨강 컬러, 평소=단색 template).
        if TRAY_DANGER.swap(danger, Ordering::Relaxed) != danger {
            let bytes = if danger { TRAY_ALERT_ICON } else { TRAY_NORMAL_ICON };
            if let Ok(img) = tauri::image::Image::from_bytes(bytes) {
                let _ = tray.set_icon(Some(img));
            }
            let _ = tray.set_icon_as_template(!danger);
        }
    });
}

/// 트레이용 짧은 시간 표기 (예: 150분 → "2.5h", 120분 → "2h", 45분 → "45m").
fn fmt_tray_duration(mins: f64) -> String {
    if mins >= 60.0 {
        let h = mins / 60.0;
        if h.fract().abs() < 0.05 {
            format!("{h:.0}h")
        } else {
            format!("{h:.1}h")
        }
    } else {
        format!("{mins:.0}m")
    }
}

/// RFC3339 시각까지 남은 분(分). 과거면 음수.
fn minutes_until(iso: &str) -> Option<f64> {
    let reset = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
    let now = chrono::Utc::now().timestamp();
    Some((reset.timestamp() - now) as f64 / 60.0)
}

/// 리셋 임박 + 잔여 충분(버려질 토큰 많음) 시 "지금 활용" 알림.
fn check_reset_alerts(app: &AppHandle, statuses: &[RunwayStatus]) {
    if !settings::notifications_enabled() || is_quiet_now() {
        return;
    }
    let reset_min = settings::reset_alert_minutes();
    if reset_min <= 0.0 {
        return;
    }
    let threshold = settings::alert_threshold();
    let Ok(mut alerted) = RESET_ALERTED.lock() else {
        return;
    };
    for s in statuses {
        let Some(pct) = s.percent_remaining else {
            continue;
        };
        let Some(resets_at) = &s.resets_at else {
            continue;
        };
        let Some(mins) = minutes_until(resets_at) else {
            continue;
        };

        // 리셋이 임박했고 아직 여유가 있으면(잔여 > 소진 임계치) = 곧 사라질 토큰이 많음.
        let hit = mins > 0.0 && mins <= reset_min && pct > threshold;
        let was = alerted.get(&s.tool).copied().unwrap_or(false);

        if hit && !was {
            let lang = i18n::current();
            let _ = app
                .notification()
                .builder()
                .title(lang.reset_title())
                .body(lang.alert_reset(&s.tool, mins, pct))
                .show();
            analytics::track("alert_fired", serde_json::json!({ "kind": "reset" }));
            alerted.insert(s.tool.clone(), true);
        } else if !hit && was {
            // 새 윈도우 시작(리셋 지남) → 재무장
            alerted.insert(s.tool.clone(), false);
        }
    }
}

/// 백그라운드에서 주기적으로 경보 + 트레이 타이틀 갱신 (창이 닫혀 있어도 동작).
fn spawn_alert_loop(app: AppHandle) {
    std::thread::spawn(move || loop {
        let statuses = compute_all();
        update_tray_title(&app, &statuses);
        check_alerts(&app, &statuses);
        check_reset_alerts(&app, &statuses);
        // 일별 롤업 누적 (첫 회만 백필로 무겁고 이후는 어제치 증분).
        rollup::update(&providers());
        std::thread::sleep(Duration::from_secs(ALERT_CHECK_SECS));
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_runway,
            get_settings,
            set_settings,
            get_available_tools,
            get_stats,
            open_settings_window,
            open_history_window,
            track_event
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

            // 우클릭 메뉴 (열기/설정/종료). 언어 변경 시 set_settings에서 재생성.
            let menu = build_tray_menu(&app.handle().clone())?;

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
                    "history" => open_history(app),
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

            // 익명: 실행 + 감지된 도구 수(이름 X)만
            let tool_count = providers().iter().filter(|p| p.available()).count();
            analytics::track(
                "app_launched",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "tool_count": tool_count,
                }),
            );

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
