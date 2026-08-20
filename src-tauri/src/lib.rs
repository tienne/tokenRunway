mod analytics;
mod atomicfile;
mod i18n;
mod pet;
mod providers;
mod rollup;
mod runway;
mod settings;

use settings::Settings;

use serde::Serialize;

use providers::antigravity::AntigravityProvider;
use providers::claude_code::ClaudeCodeProvider;
use providers::codex::CodexProvider;
use providers::gemini::GeminiProvider;
use providers::grok::GrokProvider;
use providers::{RunwayStatus, UsageProvider};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use tauri_plugin_positioner::{Position, WindowExt};

/// 백그라운드 경보 체크 주기.
const ALERT_CHECK_SECS: u64 = 60;

/// 트레이 모래시계 아이콘 — 평소(단색 template, 윗모래 가득) / 위험(빨강).
const TRAY_NORMAL_ICON: &[u8] = include_bytes!("../icons/anim/level-20@2x.png");
const TRAY_ALERT_ICON: &[u8] = include_bytes!("../icons/anim/danger@2x.png");
/// 위험 펄스용 흐릿한 빨강 모래시계 (alert ↔ dim 교차).
const TRAY_ALERT_DIM_ICON: &[u8] = include_bytes!("../icons/anim/danger-dim@2x.png");

/// 모래시계 윗모래 레벨 0(다 흐름)~20(가득) — 잔여율 5% 단위. 단색 template.
const TRAY_LEVEL_FRAMES: [&[u8]; 21] = [
    include_bytes!("../icons/anim/level-00@2x.png"),
    include_bytes!("../icons/anim/level-01@2x.png"),
    include_bytes!("../icons/anim/level-02@2x.png"),
    include_bytes!("../icons/anim/level-03@2x.png"),
    include_bytes!("../icons/anim/level-04@2x.png"),
    include_bytes!("../icons/anim/level-05@2x.png"),
    include_bytes!("../icons/anim/level-06@2x.png"),
    include_bytes!("../icons/anim/level-07@2x.png"),
    include_bytes!("../icons/anim/level-08@2x.png"),
    include_bytes!("../icons/anim/level-09@2x.png"),
    include_bytes!("../icons/anim/level-10@2x.png"),
    include_bytes!("../icons/anim/level-11@2x.png"),
    include_bytes!("../icons/anim/level-12@2x.png"),
    include_bytes!("../icons/anim/level-13@2x.png"),
    include_bytes!("../icons/anim/level-14@2x.png"),
    include_bytes!("../icons/anim/level-15@2x.png"),
    include_bytes!("../icons/anim/level-16@2x.png"),
    include_bytes!("../icons/anim/level-17@2x.png"),
    include_bytes!("../icons/anim/level-18@2x.png"),
    include_bytes!("../icons/anim/level-19@2x.png"),
    include_bytes!("../icons/anim/level-20@2x.png"),
];
const TRAY_LEVEL_MAX: u8 = 20;

/// 트레이 모드. Danger > Charge > Static 우선.
const TRAY_MODE_STATIC: u8 = 0; // 잔여율 레벨 정적 표시 (애니메이션 없음)
const TRAY_MODE_CHARGE: u8 = 1; // 업데이트 설치 중 (충전 차오름)
const TRAY_MODE_DANGER: u8 = 2; // 위험 경보 (빨강 펄스)
static TRAY_ANIM_MODE: AtomicU8 = AtomicU8::new(TRAY_MODE_STATIC);

/// 현재 배터리 레벨(0~20) — update_tray_title이 잔여율로 갱신.
static TRAY_LEVEL: AtomicU8 = AtomicU8::new(TRAY_LEVEL_MAX);

fn set_tray_mode(mode: u8) {
    TRAY_ANIM_MODE.store(mode, Ordering::Relaxed);
}

/// 직전 위험 상태 — 변할 때만 모드를 전환(중복 set 방지).
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
        Box::new(GrokProvider::new()),
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

/// 요금제 사다리의 한 티어 — 이 플랜을 쓸 때 내 주간 사용량의 예상 소진율.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanTier {
    plan: String,
    /// 이 티어에서 예상 주간 사용률 (%). 추정치.
    projected_util: f64,
    /// 현재 사용 중인 플랜인지.
    current: bool,
    /// 추천 플랜인지.
    recommended: bool,
}

/// 역산한 5시간 세션 한도 추정치 (엔터프라이즈 등 추천 N/A 플랜용).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LimitEstimate {
    /// 역산한 5시간 절대 토큰 한도 (추정). = 사용량 ÷ (사용률/100).
    limit_tokens: u64,
    /// 현재 5시간 윈도우 사용량.
    used_tokens: u64,
    /// 현재 5시간 윈도우 메시지 수.
    messages: u64,
}

/// 사용량 기반 요금제 추천 (히스토리 전용). 배수를 아는 플랜만 채워짐.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanAdvice {
    current_plan: String,
    recommended_plan: String,
    /// "upgrade" | "downgrade" | "keep" | "managed" | "na".
    direction: String,
    /// 관리형 플랜(Enterprise/Team) 여부 — 개인 전환 대상이 아니라 환산은 참고용.
    managed: bool,
    /// 티어별 예상 소진율 (Pro→Max 20x 순).
    tiers: Vec<PlanTier>,
    /// 관리형(엔터프라이즈 등)일 때 함께 보여줄 추정 5시간 한도.
    estimate: Option<LimitEstimate>,
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
    /// 사용량 기반 요금제 추천. 배수를 아는 플랜(Claude Pro/Max)만 채워짐.
    plan_advice: Option<PlanAdvice>,
    /// 현재 주간 윈도우의 일별 소진 분해. 주간 한도가 없는 도구는 빈 배열.
    weekly_days: Vec<providers::WeeklyDay>,
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

/// "YYYY-MM-DD" 두 날짜 사이의 모든 날짜(양끝 포함).
pub(crate) fn date_range(start: &str, end: &str) -> Vec<String> {
    use chrono::NaiveDate;
    let (Ok(mut cur), Ok(last)) = (
        NaiveDate::parse_from_str(start, "%Y-%m-%d"),
        NaiveDate::parse_from_str(end, "%Y-%m-%d"),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while cur <= last {
        out.push(cur.format("%Y-%m-%d").to_string());
        let Some(next) = cur.succ_opt() else { break };
        cur = next;
    }
    out
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

/// 플랜명 → Pro 대비 배수. "Pro"→1, "Max 5x"→5, "Max 20x"→20. 모르면 None.
///
/// Anthropic이 이름에 배수를 인코딩("Max 5x" = Pro의 5배)하므로, 이는 임의 추정이
/// 아니라 공식 명칭에 근거한 값이다. 배수를 모르는 플랜(Codex Plus/Pro 등)은 None.
fn plan_multiplier(plan: &str) -> Option<f64> {
    let p = plan.trim();
    if p.eq_ignore_ascii_case("pro") {
        return Some(1.0);
    }
    if p.to_lowercase().contains("max") {
        let digits: String = p.chars().filter(|c| c.is_ascii_digit()).collect();
        return digits.parse::<f64>().ok();
    }
    None
}

/// 사용량 기반 요금제 추천.
///
/// 현재 플랜의 주간 사용률로 한도(용량)를 역산하고, 이름의 배수 비율로 다른 티어의
/// 용량을 추정해 각 티어에서의 예상 소진율을 낸다. 여유롭게(≤75%) 담기는 가장 저렴한
/// 티어를 추천한다. 배수를 아는 플랜만 Some을 반환한다.
///
/// `peak_weekly_util`은 선택 기간에 관측된 최대 주간 사용률이다. 평균만 보면
/// "평소엔 한가하지만 마감 주에 몰아 쓰는" 사용자에게 하향을 권하게 되므로,
/// 하향 판단에는 평균이 아니라 최악의 주를 기준으로 삼는다.
fn compute_plan_advice(
    official: &crate::providers::OfficialUsage,
    this_week_usage: u64,
    typical_weekly: f64,
    peak_weekly_util: f64,
    provider: &dyn crate::providers::UsageProvider,
    now_ms: i64,
) -> Option<PlanAdvice> {
    let plan = official.plan.as_deref()?;
    // 표시명이 소비자 사다리에 없으면(Enterprise/Team) 관리형 — 개인 전환 대상이 아니다.
    let managed = plan_multiplier(plan).is_none();
    // 사다리 앵커 배수: rateLimitTier(좌석 등급, 엔터프라이즈도 존재) 우선, 없으면 표시명.
    let anchor = official.rate_limit_multiplier.or_else(|| plan_multiplier(plan));

    let u = official.seven_day_utilization;
    // 신뢰 가드: 배수 없음/사용률 낮음/사용량 0이면 사다리 계산 불가.
    let ladder_ok =
        anchor.is_some() && u >= 3.0 && this_week_usage > 0 && typical_weekly > 0.0;

    if !ladder_ok {
        // 관리형은 최소한 추정 5시간 한도라도 보여준다. 소비자는 표시 안 함.
        if managed {
            return Some(PlanAdvice {
                current_plan: plan.to_string(),
                recommended_plan: String::new(),
                direction: "na".to_string(),
                managed: true,
                tiers: Vec::new(),
                estimate: estimate_5h_limit(official, provider, now_ms),
            });
        }
        return None;
    }

    let m_cur = anchor.unwrap();
    // Pro(1x) 주간 한도 = 현재 등급 용량 ÷ 등급 배수.
    let cap_1x = (this_week_usage as f64 / (u / 100.0)) / m_cur;
    if cap_1x <= 0.0 {
        return None;
    }

    const LADDER: [(&str, f64); 3] = [("Pro", 1.0), ("Max 5x", 5.0), ("Max 20x", 20.0)];
    const COMFORT: f64 = 75.0;
    /// 하향해도 최악의 주가 이 이하로 담겨야 안전하다고 본다.
    const PEAK_COMFORT: f64 = 90.0;

    let mut tiers: Vec<PlanTier> = LADDER
        .iter()
        .map(|(name, mult)| {
            let util = (typical_weekly / (cap_1x * mult)) * 100.0;
            PlanTier {
                plan: name.to_string(),
                projected_util: (util * 10.0).round() / 10.0,
                // 관리형은 실제로 그 티어에 있는 게 아니므로 current 표시 안 함.
                current: !managed && (mult - m_cur).abs() < 0.01,
                recommended: false,
            }
        })
        .collect();

    // 지금보다 낮은 티어를 권하려면 관측된 최악의 주도 담겨야 한다.
    // 사용률 이력이 아직 없으면(설치 직후) 하향 판단은 보류한다.
    let downgrade_safe = |mult: f64| {
        if mult >= m_cur {
            return true;
        }
        peak_weekly_util > 0.0 && peak_weekly_util * m_cur / mult <= PEAK_COMFORT
    };

    // 편안한(≤75%) 가장 저렴한(배수 작은) 티어. 없으면 가장 큰 티어.
    let rec_idx = tiers
        .iter()
        .enumerate()
        .position(|(i, tr)| tr.projected_util <= COMFORT && downgrade_safe(LADDER[i].1))
        .unwrap_or(tiers.len() - 1);
    tiers[rec_idx].recommended = true;

    // 관리형은 전환 방향이 없다 — 개인 플랜 환산(참고용)만.
    let direction = if managed {
        "managed".to_string()
    } else {
        let cur_idx = tiers.iter().position(|tr| tr.current)?;
        if rec_idx > cur_idx {
            "upgrade"
        } else if rec_idx < cur_idx {
            "downgrade"
        } else {
            "keep"
        }
        .to_string()
    };

    Some(PlanAdvice {
        current_plan: plan.to_string(),
        recommended_plan: tiers[rec_idx].plan.clone(),
        direction,
        managed,
        tiers,
        estimate: if managed {
            estimate_5h_limit(official, provider, now_ms)
        } else {
            None
        },
    })
}

/// 5시간 세션의 절대 토큰 한도를 역산 추정.
///
/// Anthropic은 절대 한도를 비공개하므로 `5시간 사용량 ÷ (사용률/100)`으로 역산한다.
/// 사용률이 낮으면(분모가 작으면) 부정확해 최소 임계치 미만이면 None.
fn estimate_5h_limit(
    official: &crate::providers::OfficialUsage,
    provider: &dyn crate::providers::UsageProvider,
    now_ms: i64,
) -> Option<LimitEstimate> {
    let util = official.five_hour_utilization;
    if util < 3.0 {
        return None;
    }
    let window_secs = provider.window_secs();
    // 공식 사용률이 가리키는 구간은 롤링이 아니라 리셋 시각 기준 고정 윈도우다.
    // 롤링 합계로 나누면 경계가 어긋난 만큼 한도가 부풀려진다.
    let window_start = chrono::DateTime::parse_from_rfc3339(&official.five_hour_resets_at)
        .ok()
        .map(|d| d.timestamp_millis() - window_secs * 1000)
        .filter(|start| *start <= now_ms)
        .unwrap_or(now_ms - window_secs * 1000);
    let samples = provider.collect_samples(window_start);
    let mut used_tokens = 0u64;
    let mut messages = 0u64;
    for s in samples.iter().filter(|s| s.timestamp_ms >= window_start) {
        used_tokens += s.amount;
        messages += 1;
    }
    if used_tokens == 0 {
        return None;
    }
    let limit_tokens = (used_tokens as f64 / (util / 100.0)).round() as u64;
    Some(LimitEstimate {
        limit_tokens,
        used_tokens,
        messages,
    })
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
            // 오늘 엔트리에는 실시간 폴링이 기록해둔 사용률이 들어있다 —
            // 사용량만 갈아끼우고 사용률은 살린다.
            if let Some(prev) = all.get(&today) {
                today_roll.peak_five_hour_util = prev.peak_five_hour_util;
                today_roll.peak_seven_day_util = prev.peak_seven_day_util;
            }
            let today_usage = today_roll.usage;
            all.insert(today.clone(), today_roll);

            // 사용하지 않은 날이 빠지면 추세 그래프가 압축돼 왜곡되고, 주간 평균의
            // 분모도 활동일 수가 돼 사용량이 과대 추정된다. 달력의 빈 날을 0으로 채운다.
            let range_start = if days == 0 {
                all.keys().next().cloned().unwrap_or_else(|| today.clone())
            } else {
                window_start_date.clone()
            };
            let dates = date_range(&range_start, &today);
            let in_window: Vec<&rollup::DayRollup> =
                dates.iter().filter_map(|d| all.get(d)).collect();

            let days_vec: Vec<DayStat> = dates
                .iter()
                .map(|d| {
                    let r = all.get(d);
                    DayStat {
                        date: d.get(5..).unwrap_or(d).replace('-', "/"),
                        usage: r.map_or(0, |x| x.usage),
                        count: r.map_or(0, |x| x.count),
                        cost: r.map_or(0.0, |x| x.cost),
                    }
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
                .filter(|d| d.usage > 0)
                .map(|d| (d.date.clone(), d.usage))
                .unwrap_or_default();

            // 하향 추천 가드용 — 기간 내 관측된 최악의 주간 사용률.
            let peak_weekly_util = in_window
                .iter()
                .map(|r| r.peak_seven_day_util)
                .fold(0.0f64, f64::max);

            // 모델·시간대: 윈도우 내 합산
            let mut model_map: HashMap<String, (u64, f64, u64)> = HashMap::new();
            let mut hourly = vec![0u64; 24];
            for r in &in_window {
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

            // 요금제 추천 — 선택 기간의 평균 주간 사용량(달력 기준) 대비 티어별 소진율.
            // days_vec은 빈 날까지 포함하므로 분모가 실제 달력 일수다.
            let typical_weekly = if !days_vec.is_empty() {
                total_usage as f64 / days_vec.len() as f64 * 7.0
            } else {
                0.0
            };
            // 캐시된 값이라 저렴하다 — 주간 분해와 요금제 추천이 함께 쓴다.
            let official = p.official_usage();

            // 주간 한도를 날짜별로 쪼갠 소진 분해. 주간 윈도우가 없는 도구는 빈 배열이라
            // 프론트가 블록 자체를 그리지 않는다.
            let weekly_days = official
                .as_ref()
                .map(|u| runway::weekly_days(p.tool_name(), u, now_ms, today_usage))
                .unwrap_or_default();

            // 요금제 사다리(Pro/Max 5x/Max 20x)는 Claude Code 전용이다.
            // 다른 도구(Codex "Pro"/"Plus" 등)에 적용하면 엉뚱한 Claude 티어를 추천한다.
            let plan_advice = if p.tool_name() == "Claude Code" {
                official.as_ref().and_then(|u| {
                    compute_plan_advice(
                        u,
                        this_week_usage,
                        typical_weekly,
                        peak_weekly_util,
                        p.as_ref(),
                        now_ms,
                    )
                })
            } else {
                None
            };

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
                plan_advice,
                weekly_days,
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
///
/// provider 파싱과 (캐시 만료 시) OAuth 호출이 걸려 있어 동기 command로 두면
/// 메인 이벤트 루프가 멈춰 팝오버가 프리즈된다. blocking 풀에서 실행한다.
#[tauri::command]
async fn get_runway() -> Vec<RunwayStatus> {
    tauri::async_runtime::spawn_blocking(compute_all)
        .await
        .unwrap_or_default()
}

/// 현재 설정을 반환.
#[tauri::command]
fn get_settings() -> Settings {
    settings::get()
}

/// 설정을 저장 (디스크 + 전역). 언어 변경에 대비해 트레이/제목을 다시 현지화.
#[tauri::command]
fn set_settings(app: AppHandle, mut settings: Settings) {
    settings.pet_scale = settings
        .pet_scale
        .clamp(settings::PET_SCALE_MIN, settings::PET_SCALE_MAX);
    let pet_enabled = settings.pet_enabled;
    let pet_scale = settings.pet_scale;
    settings::set(settings);
    refresh_localized_ui(&app);
    if let Some(win) = app.get_webview_window("pet") {
        let _ = if pet_enabled { win.show() } else { win.hide() };
        // 창을 먼저 키워야 프론트가 그 안에 큰 스프라이트를 그릴 수 있다.
        let _ = win.set_size(pet_window_size(pet_scale));
    }
    // 다른 창(대시보드)이 즉시 언어·설정을 다시 읽도록 브로드캐스트.
    let _ = app.emit("settings-changed", ());
}

/// 커스텀 pet 번들 폴더를 검증·복사한다. 파일 I/O가 있어 blocking 풀에서 실행.
#[tauri::command]
async fn import_pet_bundle(source_dir: String) -> Result<pet::PetBundle, String> {
    tauri::async_runtime::spawn_blocking(move || pet::import_bundle(std::path::PathBuf::from(source_dir)))
        .await
        .map_err(|e| e.to_string())?
}

/// 저장해둔 커스텀 pet 번들 하나를 삭제한다.
#[tauri::command]
async fn delete_pet_bundle(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || pet::delete_bundle(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// 데스크톱 pet 클릭 시 메인 팝오버를 연다.
#[tauri::command]
fn open_main_window(app: AppHandle) {
    toggle_popover(&app, true);
}

/// pet 우클릭 컨텍스트 메뉴(숨기기/전환/불러오기)를 만들어 커서 위치에 띄운다.
#[tauri::command]
fn show_pet_context_menu(app: AppHandle) -> Result<(), String> {
    let Some(win) = app.get_webview_window("pet") else {
        return Err("pet window missing".into());
    };
    let menu = build_pet_context_menu(&app).map_err(|e| e.to_string())?;
    win.popup_menu(&menu).map_err(|e| e.to_string())
}

/// pet 관련 메뉴 항목(숨기기/표시 토글 + 전환 서브메뉴) 구성 — pet 우클릭 메뉴와 트레이
/// 메뉴가 공유한다. 매번 새 인스턴스를 만든다: 같은 MenuItem을 두 메뉴 트리에 동시에
/// 붙일 수 없어서다. 클릭 이벤트는 어느 메뉴에서 눌렀든 `Builder::on_menu_event`
/// 전역 핸들러 하나로 들어온다(트레이 전용 핸들러와 별개 스트림이 아니라 같은
/// `global_event_listeners`를 공유하는 것을 소스로 확인했다).
fn build_pet_menu_entries(
    app: &AppHandle,
) -> tauri::Result<(MenuItem<tauri::Wry>, Submenu<tauri::Wry>)> {
    let lang = i18n::current();
    let s = settings::get();

    let toggle_label = if s.pet_enabled {
        lang.menu_pet_hide()
    } else {
        lang.menu_pet_show()
    };
    let toggle_item = MenuItem::with_id(app, "pet-toggle", toggle_label, true, None::<&str>)?;

    let builtin_item = CheckMenuItem::with_id(
        app,
        "pet-select:builtin",
        lang.menu_pet_builtin(),
        true,
        s.active_pet_bundle_id.is_none(),
        None::<&str>,
    )?;
    let mut change_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> =
        vec![Box::new(builtin_item)];
    for b in &s.pet_bundles {
        let checked = s.active_pet_bundle_id.as_deref() == Some(b.id.as_str());
        let item = CheckMenuItem::with_id(
            app,
            format!("pet-select:{}", b.id),
            &b.name,
            true,
            checked,
            None::<&str>,
        )?;
        change_items.push(Box::new(item));
    }
    change_items.push(Box::new(PredefinedMenuItem::separator(app)?));
    let import_item =
        MenuItem::with_id(app, "pet-import", lang.menu_pet_import(), true, None::<&str>)?;
    change_items.push(Box::new(import_item));

    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        change_items.iter().map(|b| b.as_ref()).collect();
    let change_submenu = Submenu::with_items(app, lang.menu_pet_change(), true, &refs)?;

    Ok((toggle_item, change_submenu))
}

/// pet 우클릭 메뉴 구성: 숨기기/표시 토글 + 전환 서브메뉴(기본 펫/저장된 번들들/불러오기).
fn build_pet_context_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let (toggle_item, change_submenu) = build_pet_menu_entries(app)?;
    Menu::with_items(app, &[&toggle_item, &change_submenu])
}

/// pet이 설 수 있는 영역 하나(논리 px). 프론트의 Area와 같은 모양.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PetArea {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

/// 연결된 모니터 전부의 작업영역에서 pet이 설 수 있는 사각형을 만든다.
///
/// 프론트의 `availableMonitors()`는 창 권한에 걸려 조용히 실패할 수 있어(그러면 주
/// 모니터만 남아 드래그로 옮긴 pet이 되돌아온다) Rust에서 직접 읽는다.
/// `pet_size`만큼 오른쪽·아래를 줄여 pet 전체가 화면에 들어오게 한다.
#[tauri::command]
fn pet_areas(app: AppHandle, pet_size: f64) -> Vec<PetArea> {
    let monitors = match app.available_monitors() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[pet] available_monitors 실패: {e}");
            return Vec::new();
        }
    };
    monitors
        .iter()
        .map(|m| {
            let sf = m.scale_factor();
            let w = m.work_area();
            let x = w.position.x as f64 / sf;
            let y = w.position.y as f64 / sf;
            PetArea {
                min_x: x,
                max_x: x + w.size.width as f64 / sf - pet_size,
                min_y: y,
                max_y: y + w.size.height as f64 / sf - pet_size - 8.0,
            }
        })
        .collect()
}

/// pet 창의 기준 한 변(논리 px) — 배율 1.0일 때 크기. 프론트의 PET_BASE_SIZE와 같아야 한다.
const PET_WINDOW_BASE: f64 = 100.0;

/// 배율을 적용한 pet 창 크기.
fn pet_window_size(scale: f64) -> tauri::LogicalSize<f64> {
    let side = PET_WINDOW_BASE * scale;
    tauri::LogicalSize::new(side, side)
}

/// 데스크톱 pet 오버레이 창을 생성 (이미 있으면 아무 것도 안 함). 화면 전체를 돌아다니는
/// 작은 투명 창 — settings/history와 달리 항상 떠 있어야 하므로 앱 시작 시 1회 생성한다.
fn create_pet_window(app: &AppHandle) {
    if app.get_webview_window("pet").is_some() {
        return;
    }
    let s = settings::get();
    let visible = s.pet_enabled;
    let size = pet_window_size(s.pet_scale);
    if let Err(e) = WebviewWindowBuilder::new(app, "pet", WebviewUrl::App("index.html".into()))
        .title("")
        .inner_size(size.width, size.height)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .visible_on_all_workspaces(true)
        .visible(visible)
        .build()
    {
        eprintln!("[pet] window creation failed: {e}");
    }
}

/// 종료 직전 pet 창의 현재 위치(논리 px)를 설정에 저장 — 다음 실행에서 이어서 시작한다.
/// 프론트가 관여할 필요 없이 Rust가 실제 OS 창 위치를 직접 읽는다.
fn save_pet_position(app: &AppHandle) {
    let Some(win) = app.get_webview_window("pet") else {
        return;
    };
    let Ok(pos) = win.outer_position() else {
        return;
    };
    let sf = win.scale_factor().unwrap_or(1.0);
    let mut s = settings::get();
    s.pet_last_x = Some(pos.x as f64 / sf);
    s.pet_last_y = Some(pos.y as f64 / sf);
    settings::set(s);
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
        // 창을 닫아도 파괴하지 않고 숨기므로 프론트가 재마운트되지 않는다 — 그동안
        // pet 우클릭 메뉴 등에서 바뀐 값을 다시 읽도록 알려준다.
        let _ = app.emit("settings-changed", ());
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
    let update_item =
        MenuItem::with_id(app, "check_update", lang.menu_check_update(), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", lang.menu_quit(), true, None::<&str>)?;
    let (pet_toggle, pet_change) = build_pet_menu_entries(app)?;
    Menu::with_items(
        app,
        &[
            &show,
            &history_item,
            &settings_item,
            &pet_toggle,
            &pet_change,
            &update_item,
            &quit,
        ],
    )
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

/// pet 창에 보내는 경보 알림 — OS 알림 권한이 없어도 pet이 대신 눈에 띄게 반응하도록.
/// OS 알림과 같은 판정(중복방지·방해금지 시간대 포함) 순간에 함께 쏜다.
#[derive(Clone, Serialize)]
struct PetAlertEvent {
    tool: String,
    kind: String, // "low" | "eta" | "reset"
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
        // 리셋이 소진보다 먼저 오면 실제로는 바닥나지 않는다. verdict가 이미
        // 그 판정을 해뒀으니 "리셋 전 소진"일 때만 ETA 경보를 울린다.
        let runs_out_first = s.verdict.as_ref().is_some_and(|v| v.level == "danger");
        let eta_hit = eta_threshold > 0.0
            && runs_out_first
            && s.eta_minutes.is_some_and(|e| e <= eta_threshold);
        let should_alert = pct_hit || eta_hit;

        if should_alert && !was_alerted {
            let lang = i18n::current();
            // ETA만 걸렸으면 시간 중심 메시지, 그 외엔 잔여율 메시지
            let body = if eta_hit && !pct_hit {
                lang.alert_eta(s.eta_minutes.unwrap_or(0.0), pct)
            } else {
                lang.alert_low(pct)
            };
            let _ = app
                .notification()
                .builder()
                .title(lang.alert_title(&s.tool))
                .body(body)
                .show();
            // 익명: 어떤 종류 경보가 떴는지 메타만 (값 X)
            let kind = if eta_hit && !pct_hit { "eta" } else { "low" };
            analytics::track("alert_fired", serde_json::json!({ "kind": kind }));
            let _ = app.emit(
                "pet-alert",
                PetAlertEvent {
                    tool: s.tool.clone(),
                    kind: kind.to_string(),
                },
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
            // 가정한 한도로 만든 추정치(Gemini)가 공식 잔여율을 밀어내고 트레이를
            // 점거하면 안 된다. 공식 데이터가 있으면 그쪽이 항상 이긴다.
            .min_by(|a, b| {
                a.is_estimate.cmp(&b.is_estimate).then_with(|| {
                    a.percent_remaining
                        .unwrap_or(f64::MAX)
                        .total_cmp(&b.percent_remaining.unwrap_or(f64::MAX))
                })
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

    // 잔여율 → 배터리 레벨(0~20). 잔여율이 없으면 풀로 표시.
    let level = match pct {
        Some(p) => ((p / 100.0 * TRAY_LEVEL_MAX as f64).round() as i64)
            .clamp(0, TRAY_LEVEL_MAX as i64) as u8,
        None => TRAY_LEVEL_MAX,
    };
    TRAY_LEVEL.store(level, Ordering::Relaxed);

    // 위험 상태가 바뀌면 모드 전환 (충전 중이면 그대로 둠).
    if TRAY_DANGER.swap(danger, Ordering::Relaxed) != danger
        && TRAY_ANIM_MODE.load(Ordering::Relaxed) != TRAY_MODE_CHARGE
    {
        set_tray_mode(if danger {
            TRAY_MODE_DANGER
        } else {
            TRAY_MODE_STATIC
        });
    }

    // 정적 모드면 잔여율 레벨을 직접 그린다 (애니메이터는 쉬는 상태).
    let draw_level = TRAY_ANIM_MODE.load(Ordering::Relaxed) == TRAY_MODE_STATIC;

    // 트레이 UI 갱신은 메인 스레드에서.
    let app_for_tray = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(tray) = app_for_tray.tray_by_id("main") else {
            return;
        };
        let _ = tray.set_title(title);
        if draw_level {
            if let Ok(img) = tauri::image::Image::from_bytes(TRAY_LEVEL_FRAMES[level as usize]) {
                let _ = tray.set_icon(Some(img));
            }
            let _ = tray.set_icon_as_template(true);
        }
    });
}

/// 트레이 아이콘 애니메이터 — 모드에 따라 프레임을 순환 교체한다.
/// 별도 스레드에서 돌며 set_icon은 메인 스레드로 위임. (창이 닫혀도 동작)
fn spawn_tray_animator(app: AppHandle) {
    std::thread::spawn(move || {
        let mut charge = 0usize;
        let mut pulse_on = true;
        let mut last_mode = u8::MAX;
        loop {
            let mode = TRAY_ANIM_MODE.load(Ordering::Relaxed);
            let mode_changed = mode != last_mode;
            last_mode = mode;

            let (bytes, is_template, sleep_ms): (&[u8], bool, u64) = match mode {
                TRAY_MODE_DANGER => {
                    pulse_on = !pulse_on;
                    let b = if pulse_on {
                        TRAY_ALERT_ICON
                    } else {
                        TRAY_ALERT_DIM_ICON
                    };
                    (b, false, 550)
                }
                TRAY_MODE_CHARGE => {
                    // 윗모래가 찼다 빠졌다 왕복 (설치 진행 표시).
                    let n = TRAY_LEVEL_FRAMES.len() - 1; // 20
                    charge = if mode_changed {
                        0
                    } else {
                        (charge + 1) % (n * 2)
                    };
                    let idx = if charge <= n { charge } else { n * 2 - charge };
                    (TRAY_LEVEL_FRAMES[idx], true, 70)
                }
                _ => {
                    // STATIC — 모드 진입 직후 1회만 현재 레벨을 그리고 이후엔 쉰다(전력 절약).
                    // 레벨 변화는 update_tray_title이 직접 반영한다.
                    if !mode_changed {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        continue;
                    }
                    let lvl = TRAY_LEVEL.load(Ordering::Relaxed).min(TRAY_LEVEL_MAX) as usize;
                    (TRAY_LEVEL_FRAMES[lvl], true, 200)
                }
            };

            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || {
                let Some(tray) = app2.tray_by_id("main") else {
                    return;
                };
                if let Ok(img) = tauri::image::Image::from_bytes(bytes) {
                    let _ = tray.set_icon(Some(img));
                }
                // template 여부는 모드 전환 시에만 (빨강 위험 ↔ 단색 배터리).
                if mode_changed {
                    let _ = tray.set_icon_as_template(is_template);
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
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
                .title(lang.reset_title(&s.tool))
                .body(lang.alert_reset(mins, pct))
                .show();
            analytics::track("alert_fired", serde_json::json!({ "kind": "reset" }));
            let _ = app.emit(
                "pet-alert",
                PetAlertEvent {
                    tool: s.tool.clone(),
                    kind: "reset".to_string(),
                },
            );
            alerted.insert(s.tool.clone(), true);
        } else if !hit && was {
            // 새 윈도우 시작(리셋 지남) → 재무장
            alerted.insert(s.tool.clone(), false);
        }
    }
}

/// 오늘 관측한 사용률을 롤업에 남긴다.
///
/// 사용률은 과거 JSONL로 되돌려 계산할 수 없어 관측 시점에 기록해야 한다.
/// 이게 쌓여야 요금제 추천이 "한 주만 조용한 것"과 "정말 과투자"를 구분한다.
/// 가정한 한도로 만든 추정치는 남기지 않는다 — 이력으로서 의미가 없다.
fn record_utilizations(statuses: &[RunwayStatus]) {
    for s in statuses {
        if s.is_estimate || (s.percent_remaining.is_none() && s.seven_day_remaining.is_none()) {
            continue;
        }
        rollup::record_utilization(
            &s.tool,
            s.percent_remaining.map_or(0.0, |p| 100.0 - p),
            s.seven_day_remaining.map_or(0.0, |p| 100.0 - p),
        );
    }
}

/// 백그라운드에서 주기적으로 경보 + 트레이 타이틀 갱신 (창이 닫혀 있어도 동작).
fn spawn_alert_loop(app: AppHandle) {
    std::thread::spawn(move || loop {
        let statuses = compute_all();
        update_tray_title(&app, &statuses);
        check_alerts(&app, &statuses);
        check_reset_alerts(&app, &statuses);
        record_utilizations(&statuses);
        // 일별 롤업 누적 (첫 회만 백필로 무겁고 이후는 어제치 증분).
        rollup::update(&providers());
        std::thread::sleep(Duration::from_secs(ALERT_CHECK_SECS));
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // SETTINGS/ROLLUP static이 처음 건드려지기 전에 옛 데이터 위치를 새 위치로 옮긴다.
    settings::migrate_data_dir();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_runway,
            get_settings,
            set_settings,
            get_available_tools,
            get_stats,
            open_settings_window,
            pet_areas,
            open_history_window,
            track_event,
            import_pet_bundle,
            delete_pet_bundle,
            open_main_window,
            show_pet_context_menu
        ])
        // pet 우클릭 메뉴와 트레이 메뉴 둘 다 이 하나의 전역 핸들러로 들어온다 — Tauri는
        // TrayIconBuilder::on_menu_event와 Builder::on_menu_event를 같은
        // `global_event_listeners`에 등록해 모든 리스너가 모든 메뉴 이벤트를 받는다(소스 확인).
        // 그래서 트레이 쪽 핸들러는 이 id들을 몰라도 되고, 여기 한 곳만 안다.
        // pet-* 액션 후엔 트레이 메뉴 라벨/체크마크가 바로 갱신되도록 refresh_localized_ui를 부른다.
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if id == "pet-toggle" {
                let mut s = settings::get();
                s.pet_enabled = !s.pet_enabled;
                let enabled = s.pet_enabled;
                settings::set(s);
                if let Some(w) = app.get_webview_window("pet") {
                    let _ = if enabled { w.show() } else { w.hide() };
                }
                refresh_localized_ui(app);
                let _ = app.emit("settings-changed", ());
            } else if let Some(target) = id.strip_prefix("pet-select:") {
                let mut s = settings::get();
                s.active_pet_bundle_id = if target == "builtin" {
                    None
                } else {
                    Some(target.to_string())
                };
                settings::set(s);
                refresh_localized_ui(app);
                let _ = app.emit("settings-changed", ());
            } else if id == "pet-import" {
                use tauri_plugin_dialog::DialogExt;
                let app2 = app.clone();
                app.dialog().file().pick_folder(move |folder| {
                    let Some(fp) = folder else { return };
                    let Ok(path) = fp.into_path() else { return };
                    let app3 = app2.clone();
                    tauri::async_runtime::spawn(async move {
                        let result =
                            tauri::async_runtime::spawn_blocking(move || pet::import_bundle(path))
                                .await;
                        // 에러는 조용히 무시 — 이 메뉴는 빠른 추가용이고, 상세 에러 메시지는
                        // 설정 화면의 "폴더에서 불러오기" 버튼(같은 커맨드, UI 있음)에서 본다.
                        if let Ok(Ok(bundle)) = result {
                            let mut s = settings::get();
                            s.pet_bundles.push(bundle.clone());
                            s.active_pet_bundle_id = Some(bundle.id);
                            settings::set(s);
                            refresh_localized_ui(&app3);
                            let _ = app3.emit("settings-changed", ());
                        }
                    });
                });
            }
        })
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
            // 시작 직후 애니메이터가 프레임으로 덮어쓴다.
            let tray_icon = tauri::image::Image::from_bytes(TRAY_NORMAL_ICON)?;

            TrayIconBuilder::with_id("main")
                .icon(tray_icon)
                .icon_as_template(true)
                .tooltip("Token Runway")
                .menu(&menu)
                // 좌클릭은 메뉴 대신 팝오버 토글에 쓴다.
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => {
                        save_pet_position(app);
                        app.exit(0);
                    }
                    "show" => toggle_popover(app, true),
                    "settings" => open_settings(app),
                    "history" => open_history(app),
                    "check_update" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            run_update(app, true).await;
                        });
                    }
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

            // 데스크톱 pet 오버레이 — 화면을 돌아다니는 항상-표시 창.
            create_pet_window(&app.handle().clone());

            // 백그라운드 경보 루프 시작 (창이 닫혀도 메뉴바 상주 상태로 동작)
            spawn_alert_loop(app.handle().clone());

            // 트레이 아이콘 애니메이터 (상시 활주 / 위험 펄스 / 업데이트 흐름)
            spawn_tray_animator(app.handle().clone());

            // 시작 시 업데이트 확인 (있으면 알림만, 설치는 트레이 메뉴에서)
            let update_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                run_update(update_handle, false).await;
            });

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

/// 업데이트 확인. `install`이면 발견 시 다운로드·설치 후 재시작(메뉴용),
/// 아니면 발견 알림만(시작 시 백그라운드용).
async fn run_update(app: AppHandle, install: bool) {
    let lang = i18n::current();
    let Ok(updater) = app.updater() else {
        return;
    };
    match updater.check().await {
        Ok(Some(update)) => {
            if install {
                let _ = app
                    .notification()
                    .builder()
                    .title("Token Runway")
                    .body(lang.update_installing())
                    .show();
                // 설치 중엔 트레이를 충전 차오름으로 — 진행 표시.
                set_tray_mode(TRAY_MODE_CHARGE);
                if update
                    .download_and_install(|_, _| {}, || {})
                    .await
                    .is_ok()
                {
                    app.restart();
                }
                // 실패 시 직전 상태로 복귀 (재시작했다면 여기 도달 안 함).
                set_tray_mode(if TRAY_DANGER.load(Ordering::Relaxed) {
                    TRAY_MODE_DANGER
                } else {
                    TRAY_MODE_STATIC
                });
            } else {
                let title = format!("{} v{}", lang.update_available_title(), update.version);
                let _ = app
                    .notification()
                    .builder()
                    .title(title)
                    .body(lang.update_available_body())
                    .show();
            }
        }
        // 최신 버전 / 실패는 수동 확인(메뉴)일 때만 알림 — 시작 시엔 조용히 넘어감
        Ok(None) if install => {
            let _ = app
                .notification()
                .builder()
                .title("Token Runway")
                .body(lang.update_uptodate())
                .show();
        }
        Err(_) if install => {
            let _ = app
                .notification()
                .builder()
                .title("Token Runway")
                .body(lang.update_failed())
                .show();
        }
        _ => {}
    }
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
