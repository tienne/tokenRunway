//! 작업 완료 알림의 OS 알림 발송 — 클릭까지 받아서 바로 랜딩한다.
//!
//! `tauri-plugin-notification`을 쓰지 않는 이유는 데스크톱 구현이 title/body/icon/sound
//! 만 넘기고 클릭 응답을 버려서다(`desktop.rs`의 `show()`). 그래서 그 밑에 깔린
//! `mac-notification-sys`를 직접 쓴다 — notify-rust가 이미 의존으로 갖고 있어 crate가
//! 늘지 않는다.
//!
//! 잔여율·리셋 경보는 그대로 `tauri-plugin-notification`을 쓴다. 그쪽은 누를 데가 없다.

use crate::inbox::{InboxItem, Level, Target};
use mac_notification_sys::{Notification, NotificationResponse};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 클릭을 기다리는 동안 스레드 하나가 묶인다. 사용자가 무시한 알림은 응답이
/// 영원히 안 올 수 있어서 동시 대기 수를 막는다. 넘치면 클릭 대기 없이 띄운다
/// (알림은 뜨고, 랜딩은 알림함에서 하면 된다).
const MAX_WAITING: usize = 12;
static WAITING: AtomicUsize = AtomicUsize::new(0);

/// 번들 식별자를 알림 시스템에 알린다. 앱 시작 시 한 번 부른다.
/// dev 실행처럼 번들이 아닌 환경에서는 실패하는데, 그때는 알림만 안 뜨고 넘어간다.
pub fn init(bundle_id: &str) {
    if let Err(e) = mac_notification_sys::set_application(bundle_id) {
        // 조용히 삼키면 "알림함에는 쌓이는데 알림은 안 뜬다"를 추적할 수가 없다.
        eprintln!("알림 번들 등록 실패({bundle_id}): {e}");
    }
}

fn level_prefix(level: Level) -> &'static str {
    match level {
        Level::Done => "✅",
        Level::Blocked => "⏸",
        Level::Failed => "⚠️",
    }
}

fn has_landing(target: &Target) -> bool {
    !matches!(target, Target::None)
}

/// 알림을 띄우고, 사용자가 누르면 읽음 처리 + 랜딩까지 한다.
///
/// 클릭 대기가 블로킹이라 항상 별도 스레드에서 돈다. 호출자는 기다리지 않는다.
pub fn show(item: &InboxItem) {
    let title = format!("{} {}", level_prefix(item.level), item.title);
    let subtitle = item.context.clone();
    let body = item.body.clone().unwrap_or_default();
    let target = item.target.clone();
    let id = item.id.clone();

    // 누를 데가 없으면 기다릴 이유도 없다.
    let wait = has_landing(&target) && WAITING.load(Ordering::Relaxed) < MAX_WAITING;
    if wait {
        WAITING.fetch_add(1, Ordering::Relaxed);
    }

    std::thread::spawn(move || {
        let mut n = Notification::new();
        n.title(&title);
        if let Some(s) = subtitle.as_deref() {
            n.subtitle(s);
        }
        n.message(&body);
        n.default_sound();
        if wait {
            n.wait_for_click(true);
        } else {
            n.asynchronous(true);
        }

        let result = n.send();
        if wait {
            WAITING.fetch_sub(1, Ordering::Relaxed);
        }

        match result {
            // 알림을 직접 누른 것만 랜딩으로 본다. 닫기나 무시는 알림함에 남겨둔다.
            Ok(NotificationResponse::Click) | Ok(NotificationResponse::ActionButton(_)) => {
                crate::inbox::mark_read(&id);
                if let Err(e) = crate::landing::land(&target) {
                    eprintln!("알림 랜딩 실패: {e}");
                }
                crate::refresh_inbox_badge();
            }
            Err(e) => eprintln!("알림 발송 실패: {e}"),
            _ => {}
        }
    });
}
