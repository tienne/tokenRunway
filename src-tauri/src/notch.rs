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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const LABEL: &str = "notch";

/// 노치 폭을 못 읽을 때 쓰는 값. 14·16인치 MacBook Pro 노치 폭이 이 정도다.
const FALLBACK_NOTCH_WIDTH: f64 = 184.0;
/// 접힌 섬이 노치 양옆으로 메뉴바를 덮는 폭.
const WING: f64 = 84.0;
const EXPANDED_WIDTH: f64 = 380.0;
const EXPANDED_HEIGHT: f64 = 236.0;
const POLL_MS: u64 = 40;
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
    expanded: NSRect,
    notch_width: f64,
    bar_height: f64,
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
}

static GEOM: Mutex<Option<Geom>> = Mutex::new(None);
static POLL_RUNNING: AtomicBool = AtomicBool::new(false);
static WATCHING: AtomicBool = AtomicBool::new(false);

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
        for screen in screens.iter() {
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
            return Some(layout(frame, notch_width, insets.top));
        }
        None
    }
}

fn layout(screen: NSRect, notch_width: f64, bar_height: f64) -> Geom {
    let top = screen.origin.y + screen.size.height;
    let cx = screen.origin.x + screen.size.width / 2.0;
    let island_w = notch_width + WING * 2.0;
    let win_w = island_w.max(EXPANDED_WIDTH);
    let exp_w = EXPANDED_WIDTH.max(island_w);
    Geom {
        window: rect(cx - win_w / 2.0, top - EXPANDED_HEIGHT, win_w, EXPANDED_HEIGHT),
        island: rect(cx - island_w / 2.0, top - bar_height, island_w, bar_height),
        expanded: rect(cx - exp_w / 2.0, top - EXPANDED_HEIGHT, exp_w, EXPANDED_HEIGHT),
        notch_width,
        bar_height,
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
        let built = WebviewWindowBuilder::new(&handle, LABEL, WebviewUrl::App("index.html".into()))
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
        let w = match built {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[notch] window creation failed: {e}");
                return;
            }
        };
        if let Err(e) = make_panel(&w) {
            eprintln!("[notch] panel conversion failed: {e}");
            let _ = w.close();
            return;
        }
        place(&w, &g);
        let _ = w.set_ignore_cursor_events(true);
        let _ = w.show();
        spawn_poll(handle.clone());
    });
}

/// 토글을 끄면 창을 닫는다. 커서 폴링 스레드는 창이 사라진 걸 보고 스스로 끝난다.
pub fn close(app: &AppHandle) {
    *GEOM.lock().unwrap() = None;
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = handle.get_webview_window(LABEL) {
            let _ = w.close();
        }
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
    Full,
}

/// 지금 모드에서 클릭을 받는 영역.
fn hit_rect(g: &Geom, mode: Mode) -> NSRect {
    match mode {
        Mode::Collapsed => g.island,
        Mode::Full => g.expanded,
    }
}

/// 커서 위치로 다음 모드를 정한다. 접힌 섬에 들어오면 펼치고, 펼친 영역을 벗어나야
/// 접는다 — 같은 rect로 판정하면 펼치자마자 아래로 내려간 커서가 밖으로 잡혀 깜빡인다.
fn next_mode(g: &Geom, mode: Mode, p: NSPoint) -> Mode {
    if rect_contains(&hit_rect(g, mode), p) {
        Mode::Full
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
        let next = next_mode(&g, mode, p);
        if next != mode {
            mode = next;
            let _ = win.emit("notch-mode", mode);
        }
        let ignore = !rect_contains(&hit_rect(&g, mode), p);
        if ignore != ignoring {
            ignoring = ignore;
            let _ = win.set_ignore_cursor_events(ignore);
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

#[tauri::command]
pub fn notch_layout() -> NotchLayout {
    layout_for_front()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0);
        let on_island = NSPoint::new(756.0, 970.0);
        let below_island = NSPoint::new(756.0, 900.0);
        let far = NSPoint::new(100.0, 500.0);
        assert_eq!(next_mode(&g, Mode::Collapsed, below_island), Mode::Collapsed);
        assert_eq!(next_mode(&g, Mode::Collapsed, on_island), Mode::Full);
        assert_eq!(next_mode(&g, Mode::Full, below_island), Mode::Full);
        assert_eq!(next_mode(&g, Mode::Full, far), Mode::Collapsed);
    }

    #[test]
    fn island_hugs_the_top_center_of_the_screen() {
        let g = layout(rect(0.0, 0.0, 1512.0, 982.0), 200.0, 32.0);
        assert_eq!(g.island.size.width, 200.0 + WING * 2.0);
        assert_eq!(g.island.origin.y + g.island.size.height, 982.0);
        assert_eq!(g.island.origin.x + g.island.size.width / 2.0, 756.0);
        assert_eq!(g.window.origin.y + g.window.size.height, 982.0);
    }

    #[test]
    fn island_sits_on_a_secondary_screen_offset() {
        let g = layout(rect(-1512.0, 300.0, 1512.0, 982.0), 184.0, 37.0);
        assert!(rect_contains(&g.island, NSPoint::new(-756.0, 300.0 + 982.0 - 10.0)));
        assert!(!rect_contains(&g.island, NSPoint::new(-756.0, 300.0 + 982.0 - 60.0)));
        assert!(rect_contains(&g.expanded, NSPoint::new(-756.0, 300.0 + 982.0 - 60.0)));
    }
}
