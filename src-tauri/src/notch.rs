//! 노치 양옆에 붙는 상태 표시 창.
//!
//! Tauri 창은 tao가 만든 `TaoWindow`(NSWindow 하위 클래스)라서 그대로는 메뉴바 위에
//! 못 올라가고, 클릭하면 앱이 활성화돼 앞 앱의 포커스를 빼앗는다. 그래서 만든 직후
//! 클래스를 NSPanel 하위 클래스로 바꿔치기해 nonactivatingPanel로 쓴다.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{class, msg_send, sel};
use objc2_foundation::{NSArray, NSEdgeInsets, NSPoint, NSRect, NSSize};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const LABEL: &str = "notch";

/// 노치 폭을 못 읽을 때 쓰는 값. 14·16인치 MacBook Pro 노치 폭이 이 정도다.
const FALLBACK_NOTCH_WIDTH: f64 = 184.0;
/// 접힌 섬이 노치 양옆으로 메뉴바를 덮는 폭.
const WING: f64 = 84.0;
const EXPANDED_WIDTH: f64 = 420.0;
/// 펼친 섬이 자랄 수 있는 최대 높이. 창은 이 크기로 고정하고, 실제로 클릭을 받는 높이는
/// 프론트가 잰 내용 높이(`FIT_HEIGHT`)를 따른다 — 도구가 하나뿐인데 빈 검은 판이 내려오지 않게.
const EXPANDED_MAX_HEIGHT: f64 = 560.0;
/// 노치 옆에 띄우는 창과 펼친 섬, 메뉴바, 화면 끝 사이 여백.
const BESIDE_GAP: f64 = 8.0;
/// 프론트가 아직 내용 높이를 알려주기 전에 쓰는 값.
const DEFAULT_FIT_HEIGHT: u32 = 236;
const POLL_MS: u64 = 40;
/// 완료 알림이 오면 섬 아래로 한 줄을 펼쳐두는 시간과 그 줄 높이.
const NOTICE_MS: i64 = 6_000;
const NOTICE_HEIGHT: f64 = 30.0;
/// 모니터를 꽂거나 뺐는지 다시 재는 주기. Tauri에 디스플레이 변경 이벤트가 없다.
const SCREEN_SYNC: Duration = Duration::from_secs(5);

/// 메뉴바(NSMainMenuWindowLevel = 24)보다 위. pet 창(always_on_top = floating, 3)과
/// 레벨이 갈려 서로 가리지 않는다.
const PANEL_LEVEL: isize = 24 + 3;
const NS_NONACTIVATING_PANEL: usize = 1 << 7;
const CAN_JOIN_ALL_SPACES: usize = 1 << 0;
const STATIONARY: usize = 1 << 4;
const IGNORES_CYCLE: usize = 1 << 6;
const FULL_SCREEN_NONE: usize = 1 << 9;

/// 창과 섬의 위치. 전부 Cocoa 전역 좌표(주 화면 좌하단 원점, pt)다 —
/// `NSEvent.mouseLocation`이 같은 좌표계라 변환 없이 비교한다.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Geom {
    window: NSRect,
    island: NSRect,
    notice: NSRect,
    expanded: NSRect,
    notch_width: f64,
    bar_height: f64,
    /// 메뉴바가 있는 주 화면인지. 트레이 숫자를 숨길지 정할 때 쓴다.
    on_primary: bool,
    /// 노치가 있는 화면 전체. 창을 노치 옆에 띄울 때 화면 밖으로 안 나가게 자른다.
    screen: NSRect,
}

/// 프론트가 섬을 그릴 때 쓰는 치수(창 기준 pt).
#[derive(Serialize, Clone, Copy, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct NotchLayout {
    pub notch_width: f64,
    pub wing: f64,
    pub bar_height: f64,
    pub expanded_width: f64,
    pub expanded_height: f64,
    pub notice_height: f64,
}

static GEOM: Mutex<Option<Geom>> = Mutex::new(None);
static POLL_RUNNING: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);
static NOTICE_UNTIL: AtomicI64 = AtomicI64::new(0);
static FIT_HEIGHT: AtomicU32 = AtomicU32::new(DEFAULT_FIT_HEIGHT);
/// 트레이에 마지막으로 알린 `Presence`. 바뀔 때만 트레이를 다시 그리게 한다.
static LAST_PRESENCE: AtomicU8 = AtomicU8::new(u8::MAX);

/// 노치 섬이 지금 어디에 떠 있는지. 트레이 타이틀이 숫자를 숨길지 이걸 보고 정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// 창이 없다 — 노치 없는 화면, 덮개 닫음, 설정에서 끔.
    Absent,
    /// 노치 화면이 보조 화면이다(외부 모니터가 주 화면).
    Secondary,
    /// 메뉴바가 있는 주 화면이 노치 화면이다.
    Primary,
}

/// 지금 섬 상태. 창 생성에 실패하면 `GEOM`을 비우므로 `GEOM`만 보면 된다.
pub fn presence() -> Presence {
    match *GEOM.lock().unwrap() {
        None => Presence::Absent,
        Some(g) if g.on_primary => Presence::Primary,
        Some(_) => Presence::Secondary,
    }
}

/// 섬 상태가 바뀌었으면 트레이 타이틀을 다시 정하게 한다. 화면 배치는 감시 스레드가
/// 이미 5초마다 재므로 트레이 쪽에서 따로 폴링하지 않는다.
fn publish_presence(app: &AppHandle) {
    let now = presence() as u8;
    if LAST_PRESENCE.swap(now, Ordering::SeqCst) != now {
        crate::refresh_tray_title(app);
    }
}
fn rect_contains(r: &NSRect, p: NSPoint) -> bool {
    p.x >= r.origin.x
        && p.x <= r.origin.x + r.size.width
        && p.y >= r.origin.y
        && p.y <= r.origin.y + r.size.height
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// 노치가 있는 화면을 찾아 창과 섬의 위치를 잰다. 노치가 없으면 None — v1은 그 경우
/// 창을 아예 만들지 않는다. AppKit 화면 API라 메인 스레드에서만 부른다.
fn measure() -> Option<Geom> {
    unsafe {
        let screens: Retained<NSArray<AnyObject>> = msg_send![class!(NSScreen), screens];
        // `screens`의 0번이 메뉴바가 있는 주 화면이다(AppKit 문서 규약).
        for (index, screen) in screens.iter().enumerate() {
            // safeAreaInsets·auxiliaryTop*Area는 macOS 12부터 있다. 없는 셀렉터를 부르면
            // 예외로 앱이 죽는다.
            let ok: bool = msg_send![&*screen, respondsToSelector: sel!(auxiliaryTopRightArea)];
            if !ok {
                return None;
            }
            let insets: NSEdgeInsets = msg_send![&*screen, safeAreaInsets];
            if insets.top <= 0.0 {
                continue;
            }
            let frame: NSRect = msg_send![&*screen, frame];
            let left: NSRect = msg_send![&*screen, auxiliaryTopLeftArea];
            let right: NSRect = msg_send![&*screen, auxiliaryTopRightArea];
            let measured = frame.size.width - left.size.width - right.size.width;
            let notch_width = if measured.is_finite() && measured > 0.0 && measured < frame.size.width
            {
                measured
            } else {
                FALLBACK_NOTCH_WIDTH
            };
            return Some(layout(frame, notch_width, insets.top, index == 0));
        }
        None
    }
}

fn layout(screen: NSRect, notch_width: f64, bar_height: f64, on_primary: bool) -> Geom {
    let top = screen.origin.y + screen.size.height;
    let cx = screen.origin.x + screen.size.width / 2.0;
    let island_w = notch_width + WING * 2.0;
    let win_w = island_w.max(EXPANDED_WIDTH);
    let exp_w = EXPANDED_WIDTH.max(island_w);
    Geom {
        window: rect(cx - win_w / 2.0, top - EXPANDED_MAX_HEIGHT, win_w, EXPANDED_MAX_HEIGHT),
        island: rect(cx - island_w / 2.0, top - bar_height, island_w, bar_height),
        notice: rect(
            cx - exp_w / 2.0,
            top - bar_height - NOTICE_HEIGHT,
            exp_w,
            bar_height + NOTICE_HEIGHT,
        ),
        expanded: rect(cx - exp_w / 2.0, top - EXPANDED_MAX_HEIGHT, exp_w, EXPANDED_MAX_HEIGHT),
        notch_width,
        bar_height,
        on_primary,
        screen,
    }
}

pub fn layout_for_front() -> NotchLayout {
    let g = GEOM.lock().unwrap().clone();
    match g {
        Some(g) => NotchLayout {
            notch_width: g.notch_width,
            wing: WING,
            bar_height: g.bar_height,
            expanded_width: g.expanded.size.width,
            expanded_height: g.expanded.size.height,
            notice_height: NOTICE_HEIGHT,
        },
        None => NotchLayout::default(),
    }
}

// --- NSPanel 바꿔치기 ---

extern "C-unwind" fn no(_: &AnyObject, _: Sel) -> Bool {
    Bool::NO
}

/// NSWindow 기본 구현은 창을 메뉴바 아래로 끌어내린다. 메뉴바 위에 놓으려면 받은 rect를
/// 그대로 돌려줘야 한다.
extern "C-unwind" fn keep_frame(_: &AnyObject, _: Sel, frame: NSRect, _screen: *mut AnyObject) -> NSRect {
    frame
}

fn panel_class() -> &'static AnyClass {
    static CLS: OnceLock<&'static AnyClass> = OnceLock::new();
    CLS.get_or_init(|| {
        let mut b = ClassBuilder::new(c"TRWNotchPanel", class!(NSPanel))
            .expect("TRWNotchPanel은 한 번만 등록한다");
        unsafe {
            // 포커스를 받지 않는다 — 클릭이 와도 앞 앱의 키 창이 그대로 남는다.
            b.add_method(sel!(canBecomeKeyWindow), no as extern "C-unwind" fn(_, _) -> _);
            b.add_method(sel!(canBecomeMainWindow), no as extern "C-unwind" fn(_, _) -> _);
            b.add_method(
                sel!(constrainFrameRect:toScreen:),
                keep_frame as extern "C-unwind" fn(_, _, _, _) -> _,
            );
        }
        // tao의 TaoWindow가 붙이는 ivar. 클래스를 바꾼 뒤에도 tao가 이 이름으로 읽으므로
        // 같은 이름·타입을 둬 인스턴스 크기와 오프셋을 맞춘다.
        b.add_ivar::<Bool>(c"focusable");
        b.register()
    })
}

/// 창을 nonactivating NSPanel로 바꾼다. 메인 스레드에서만 부른다.
fn make_panel(win: &WebviewWindow) -> Result<(), String> {
    let ptr = win.ns_window().map_err(|e| e.to_string())? as *mut AnyObject;
    let obj = unsafe { ptr.as_ref() }.ok_or("ns_window가 비어 있음")?;
    let new_cls = panel_class();
    let old_cls = obj.class();
    // 크기가 다르면 바꿔치기가 메모리를 깨뜨린다. tao가 ivar를 바꾸면 여기서 멈춘다.
    if old_cls.instance_size() != new_cls.instance_size() {
        return Err(format!(
            "instance size mismatch: {} {} vs {}",
            old_cls.name().to_string_lossy(),
            old_cls.instance_size(),
            new_cls.instance_size()
        ));
    }
    unsafe {
        AnyObject::set_class(obj, new_cls);
        let mask: usize = msg_send![obj, styleMask];
        let _: () = msg_send![obj, setStyleMask: mask | NS_NONACTIVATING_PANEL];
        let _: () = msg_send![obj, setLevel: PANEL_LEVEL];
        // FullScreenAuxiliary를 빼야 전체화면 앱의 스페이스에 따라 들어가지 않는다.
        let behavior = CAN_JOIN_ALL_SPACES | STATIONARY | IGNORES_CYCLE | FULL_SCREEN_NONE;
        let _: () = msg_send![obj, setCollectionBehavior: behavior];
        // NSPanel 기본값은 앱이 비활성일 때 숨는 것인데, 이 앱은 거의 항상 비활성이다.
        let _: () = msg_send![obj, setHidesOnDeactivate: false];
        let _: () = msg_send![obj, setBecomesKeyOnlyIfNeeded: true];
        let _: () = msg_send![obj, setHasShadow: false];
    }
    Ok(())
}

/// 노치 옆에 띄울 창(폭 `w`)의 좌상단. 펼친 섬 오른쪽에 붙이고, 화면을 넘으면 화면
/// 안으로 당긴다. 섬 바로 아래에 두면 커서를 내리는 동안 펼친 섬이 창을 덮는다.
/// 펼친 섬이 화면 가운데라 왼쪽 공간은 오른쪽과 같아서 왼쪽으로 넘기지 않는다.
fn beside_origin(g: &Geom, w: f64) -> NSPoint {
    let s = g.screen;
    let top = s.origin.y + s.size.height - g.bar_height - BESIDE_GAP;
    let right_x = g.expanded.origin.x + g.expanded.size.width + BESIDE_GAP;
    let max_x = s.origin.x + s.size.width - BESIDE_GAP - w;
    let x = right_x.min(max_x).max(s.origin.x);
    NSPoint::new(x, top)
}

/// 창을 노치 옆으로 옮긴다. 섬이 없으면(노치 없는 화면·꺼짐) false를 돌려주고 손대지
/// 않는다. AppKit 호출이라 메인 스레드에서 부른다.
pub fn place_beside(win: &WebviewWindow) -> bool {
    let Some(g) = *GEOM.lock().unwrap() else {
        return false;
    };
    let Ok(ptr) = win.ns_window() else { return false };
    let Some(obj) = (unsafe { (ptr as *mut AnyObject).as_ref() }) else {
        return false;
    };
    unsafe {
        let frame: NSRect = msg_send![obj, frame];
        let origin = beside_origin(&g, frame.size.width);
        let _: () = msg_send![obj, setFrameTopLeftPoint: origin];
    }
    true
}

fn place(win: &WebviewWindow, g: &Geom) {
    let Ok(ptr) = win.ns_window() else { return };
    let Some(obj) = (unsafe { (ptr as *mut AnyObject).as_ref() }) else {
        return;
    };
    unsafe {
        let _: () = msg_send![obj, setFrame: g.window, display: true];
    }
}

/// 노치 창을 만들거나 다시 배치한다. 노치 화면이 없으면 창을 닫는다.
fn ensure(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        place_or_build(&handle);
        publish_presence(&handle);
    });
}

fn place_or_build(handle: &AppHandle) {
    // 생성과 닫기가 모두 메인 스레드에서 차례로 돌아서, 여기서 다시 확인하면
    // 토글을 끈 직후 감시 스레드가 창을 되살리는 일이 없다.
    let measured = if crate::settings::notch_enabled() { measure() } else { None };
    let Some(g) = measured else {
        *GEOM.lock().unwrap() = None;
        if let Some(w) = handle.get_webview_window(LABEL) {
            let _ = w.close();
        }
        return;
    };
    let prev = GEOM.lock().unwrap().replace(g);
    if let Some(w) = handle.get_webview_window(LABEL) {
        if prev != Some(g) {
            place(&w, &g);
        }
        spawn_poll(handle.clone());
        return;
    }
    let built = WebviewWindowBuilder::new(handle, LABEL, WebviewUrl::App("index.html".into()))
        .title("")
        .inner_size(g.window.size.width, g.window.size.height)
        .decorations(false)
        .transparent(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .accept_first_mouse(true)
        .visible(false)
        .build();
    // 창을 못 만들면 `GEOM`을 비워 트레이가 섬이 떠 있다고 착각하지 않게 한다.
    let w = match built {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[notch] window creation failed: {e}");
            *GEOM.lock().unwrap() = None;
            return;
        }
    };
    if let Err(e) = make_panel(&w) {
        eprintln!("[notch] panel conversion failed: {e}");
        *GEOM.lock().unwrap() = None;
        let _ = w.close();
        return;
    }
    place(&w, &g);
    let _ = w.set_ignore_cursor_events(true);
    let _ = w.show();
    spawn_poll(handle.clone());
}

/// 토글을 끄면 창을 닫는다. 커서 폴링 스레드는 창이 사라진 걸 보고 스스로 끝난다.
pub fn close(app: &AppHandle) {
    *GEOM.lock().unwrap() = None;
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = handle.get_webview_window(LABEL) {
            let _ = w.close();
        }
        publish_presence(&handle);
    });
}

/// 설정에 맞춰 창을 띄우거나 닫는다.
pub fn sync(app: &AppHandle) {
    if crate::settings::notch_enabled() {
        ensure(app);
        spawn_watch(app.clone());
    } else {
        close(app);
    }
}

/// 켜져 있는 동안 화면 배치를 다시 잰다. 덮개를 닫고 외장 모니터만 쓰다가 다시 열면
/// 노치 화면이 돌아오므로, 창이 없어도 감시는 계속한다. 배치가 그대로면 아무것도 안 한다.
fn spawn_watch(app: AppHandle) {
    if WATCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || loop {
        while crate::settings::notch_enabled() {
            std::thread::sleep(SCREEN_SYNC);
            ensure(&app);
        }
        WATCHING.store(false, Ordering::SeqCst);
        // 멈추는 사이 다시 켜졌는데 새 스레드가 swap에서 밀려났을 수 있다.
        if !crate::settings::notch_enabled() || WATCHING.swap(true, Ordering::SeqCst) {
            break;
        }
    });
}

/// 섬이 펼쳐진 정도. 프론트는 이 값만 받아 그린다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Collapsed,
    Notice,
    Full,
}

/// 펼친 섬이 실제로 덮는 영역. 위쪽은 화면 끝에 붙이고 높이만 내용에 맞춘다.
fn fitted_expanded(g: &Geom, fit: f64) -> NSRect {
    let top = g.expanded.origin.y + g.expanded.size.height;
    let h = fit.clamp(g.bar_height, g.expanded.size.height);
    rect(g.expanded.origin.x, top - h, g.expanded.size.width, h)
}

/// 지금 모드에서 클릭을 받는 영역.
fn hit_rect(g: &Geom, mode: Mode, fit: f64) -> NSRect {
    match mode {
        Mode::Collapsed => g.island,
        Mode::Notice => g.notice,
        Mode::Full => fitted_expanded(g, fit),
    }
}

/// 커서 위치로 다음 모드를 정한다. 접힌 섬에 들어오면 펼치고, 펼친 영역을 벗어나야
/// 접는다 — 같은 rect로 판정하면 펼치자마자 아래로 내려간 커서가 밖으로 잡혀 깜빡인다.
fn next_mode(g: &Geom, mode: Mode, p: NSPoint, notice: bool, fit: f64) -> Mode {
    if rect_contains(&hit_rect(g, mode, fit), p) {
        Mode::Full
    } else if notice {
        Mode::Notice
    } else {
        Mode::Collapsed
    }
}

/// 커서가 섬 위에 있을 때만 창이 클릭을 받게 한다. 판정을 프론트 IPC로 왕복하면
/// 그 사이 들어온 클릭이 뒤 창으로 새거나 섬에서 사라진다.
fn spawn_poll(app: AppHandle) {
    if POLL_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || loop {
        poll_until_closed(&app);
        POLL_RUNNING.store(false, Ordering::SeqCst);
        // 끝나는 사이 창이 다시 생겼는데 새 스레드가 swap에서 밀려났을 수 있다.
        let reopened = app.get_webview_window(LABEL).is_some() && GEOM.lock().unwrap().is_some();
        if !reopened || POLL_RUNNING.swap(true, Ordering::SeqCst) {
            break;
        }
    });
}

fn poll_until_closed(app: &AppHandle) {
    let mut mode = Mode::Collapsed;
    let mut ignoring = true;
    while crate::settings::notch_enabled() {
        let Some(win) = app.get_webview_window(LABEL) else {
            return;
        };
        let Some(g) = *GEOM.lock().unwrap() else { return };
        let p: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
        let notice = chrono::Utc::now().timestamp_millis() < NOTICE_UNTIL.load(Ordering::Relaxed);
        let fit = FIT_HEIGHT.load(Ordering::Relaxed) as f64;
        let next = next_mode(&g, mode, p, notice, fit);
        if next != mode {
            mode = next;
            let _ = app.emit_to(LABEL, "notch-mode", mode);
        }
        let ignore = !rect_contains(&hit_rect(&g, mode, fit), p);
        if ignore != ignoring {
            ignoring = ignore;
            let _ = win.set_ignore_cursor_events(ignore);
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// 알림 한 줄. 랜딩 대상 경로 같은 건 싣지 않는다 — 누르면 id로 `inbox_land`를 탄다.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Notice<'a> {
    id: &'a str,
    title: &'a str,
    context: Option<&'a str>,
}

/// 완료 알림이 오면 섬을 잠깐 펼쳐 한 줄을 보여준다. 접는 건 폴링 스레드가 시간을 보고 한다.
pub fn announce(app: &AppHandle, item: &crate::inbox::InboxItem) {
    if !crate::settings::notch_enabled() || GEOM.lock().unwrap().is_none() {
        return;
    }
    let notice = Notice {
        id: &item.id,
        title: &item.title,
        context: item.context.as_deref(),
    };
    let _ = app.emit_to(LABEL, "notch-notice", notice);
    NOTICE_UNTIL.store(
        chrono::Utc::now().timestamp_millis() + NOTICE_MS,
        Ordering::Relaxed,
    );
}

/// 알림 줄을 눌러 랜딩했으면 남은 시간을 기다리지 않고 접는다.
#[tauri::command]
pub fn notch_dismiss() {
    NOTICE_UNTIL.store(0, Ordering::Relaxed);
}

#[tauri::command]
pub fn notch_layout() -> NotchLayout {
    layout_for_front()
}

/// 프론트가 펼친 섬의 내용 높이를 알려준다. 클릭을 받는 영역을 그 높이에 맞춘다.
#[tauri::command]
pub fn notch_fit(height: f64) {
    if height.is_finite() && height > 0.0 {
        let h = height.ceil().min(EXPANDED_MAX_HEIGHT) as u32;
        FIT_HEIGHT.store(h, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIT: f64 = DEFAULT_FIT_HEIGHT as f64;

    /// tao의 TaoWindow와 같은 모양(NSWindow + focusable)을 만들어 크기를 비교한다.
    /// 다르면 클래스 바꿔치기가 메모리를 깨뜨리므로 프로토타입의 전제가 무너진다.
    #[test]
    fn panel_class_matches_tao_window_size() {
        let mut b = ClassBuilder::new(c"TRWTestTaoWindow", class!(NSWindow)).unwrap();
        b.add_ivar::<Bool>(c"focusable");
        let tao_like = b.register();
        assert_eq!(tao_like.instance_size(), panel_class().instance_size());
    }

    #[test]
    fn expands_on_the_island_and_collapses_only_outside_the_panel() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        let on_island = NSPoint::new(756.0, 970.0);
        let below_island = NSPoint::new(756.0, 900.0);
        let far = NSPoint::new(100.0, 500.0);
        assert_eq!(next_mode(&g, Mode::Collapsed, below_island, false, FIT), Mode::Collapsed);
        assert_eq!(next_mode(&g, Mode::Collapsed, on_island, false, FIT), Mode::Full);
        assert_eq!(next_mode(&g, Mode::Full, below_island, false, FIT), Mode::Full);
        assert_eq!(next_mode(&g, Mode::Full, far, false, FIT), Mode::Collapsed);
    }

    #[test]
    fn notice_line_is_clickable_and_folds_back_when_it_expires() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        let on_line = NSPoint::new(756.0, 982.0 - 32.0 - 10.0);
        let far = NSPoint::new(100.0, 500.0);
        assert_eq!(next_mode(&g, Mode::Collapsed, far, true, FIT), Mode::Notice);
        assert_eq!(next_mode(&g, Mode::Notice, on_line, true, FIT), Mode::Full);
        // 알림 시간이 지나도 커서가 펼친 영역 안이면 그대로 둔다.
        assert_eq!(next_mode(&g, Mode::Full, on_line, false, FIT), Mode::Full);
        assert_eq!(next_mode(&g, Mode::Notice, far, false, FIT), Mode::Collapsed);
    }

    #[test]
    fn island_hugs_the_top_center_of_the_screen() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        assert_eq!(g.island.size.width, 200.0 + WING * 2.0);
        assert_eq!(g.island.origin.y + g.island.size.height, 982.0);
        assert_eq!(g.island.origin.x + g.island.size.width / 2.0, 756.0);
        assert_eq!(g.window.origin.y + g.window.size.height, 982.0);
    }

    #[test]
    fn island_sits_on_a_secondary_screen_offset() {
        let g = layout(rect(-1512.0, 300.0, 1512.0, 982.0), 184.0, 37.0, false);
        assert!(rect_contains(&g.island, NSPoint::new(-756.0, 300.0 + 982.0 - 10.0)));
        assert!(!rect_contains(&g.island, NSPoint::new(-756.0, 300.0 + 982.0 - 60.0)));
        assert!(rect_contains(&g.expanded, NSPoint::new(-756.0, 300.0 + 982.0 - 60.0)));
    }

    #[test]
    fn expanded_hit_area_follows_the_content_height() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        let short = 120.0;
        let inside = NSPoint::new(756.0, 982.0 - 100.0);
        let below_content = NSPoint::new(756.0, 982.0 - 200.0);
        assert_eq!(next_mode(&g, Mode::Full, inside, false, short), Mode::Full);
        // 내용 아래 빈 자리는 펼친 창 안이어도 섬 밖이라 접히고 클릭이 통과해야 한다.
        assert_eq!(next_mode(&g, Mode::Full, below_content, false, short), Mode::Collapsed);
        // 최대 높이를 넘겨 알려와도 창 크기 안으로 자른다.
        let huge = fitted_expanded(&g, 10_000.0);
        assert_eq!(huge.size.height, EXPANDED_MAX_HEIGHT);
        assert_eq!(huge.origin.y + huge.size.height, 982.0);
    }

    #[test]
    fn side_windows_open_right_of_the_panel_and_stay_on_screen() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        let o = beside_origin(&g, 460.0);
        assert_eq!(o.x, g.expanded.origin.x + g.expanded.size.width + BESIDE_GAP);
        assert_eq!(o.y, 982.0 - 32.0 - BESIDE_GAP);
        // 오른쪽 공간이 모자라면 화면 안으로 당긴다.
        let narrow = layout(rect(0.0, 0.0, 1100.0, 800.0), 200.0, 32.0, true);
        assert_eq!(beside_origin(&narrow, 460.0).x, 1100.0 - BESIDE_GAP - 460.0);
    }

    #[test]
    fn side_windows_stay_on_a_secondary_screen() {
        let g = layout(rect(-1512.0, 300.0, 1512.0, 982.0), 184.0, 37.0, false);
        let o = beside_origin(&g, 420.0);
        assert!(o.x >= -1512.0 && o.x + 420.0 <= 0.0);
        assert_eq!(o.y, 300.0 + 982.0 - 37.0 - BESIDE_GAP);
    }

    #[test]
    fn window_is_tall_enough_for_the_largest_panel() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0, true);
        assert_eq!(g.window.size.height, EXPANDED_MAX_HEIGHT);
        assert!(g.window.size.width >= g.expanded.size.width);
    }
}
