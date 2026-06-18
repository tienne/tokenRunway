//! 익명 사용 통계 (PostHog) — opt-in 전용.
//!
//! 원칙:
//! - **opt-in**: `settings.analytics_enabled`가 true일 때만 전송
//! - **익명**: 랜덤 anon_id만, 개인 식별 정보 없음
//! - **내용 무전송**: 토큰 수치·잔여율 값·프로젝트명 등은 절대 보내지 않음 (행동 메타만)
//!
//! PostHog 프로젝트 키는 빌드 시 `POSTHOG_KEY` 환경변수로 주입한다 (레포에 미포함).
//! 키 없이 빌드하면 analytics는 완전히 비활성(전송 0).

use crate::settings;

const POSTHOG_KEY: Option<&str> = option_env!("POSTHOG_KEY");
const POSTHOG_URL: &str = "https://us.i.posthog.com/capture/";

/// 익명 이벤트 전송. opt-in이 아니거나 키가 없으면 아무것도 하지 않는다.
pub fn track(event: &str, properties: serde_json::Value) {
    let Some(key) = POSTHOG_KEY else {
        return; // 키 미주입 → 비활성
    };
    if !settings::analytics_enabled() {
        return; // opt-in 아님
    }

    let id = settings::anon_id();
    let event = event.to_string();
    // 네트워크는 백그라운드로 (UI/경보 흐름 블로킹 방지).
    std::thread::spawn(move || {
        let body = serde_json::json!({
            "api_key": key,
            "event": event,
            "distinct_id": id,
            "properties": properties,
        });
        let _ = ureq::post(POSTHOG_URL)
            .set("Content-Type", "application/json")
            .send_json(body);
    });
}
