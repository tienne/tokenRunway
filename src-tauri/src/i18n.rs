//! Rust 측 다국어 (트레이 메뉴 + OS 알림).
//!
//! UI 정적 텍스트는 프론트(src/i18n.ts)가, Rust 생성 텍스트(메뉴·알림)는 여기서 담당.
//! 언어는 설정값 우선, 없으면 시스템 로케일 자동 감지.

#[derive(Clone, Copy)]
pub enum Lang {
    Ko,
    En,
}

/// 현재 언어 — 설정값(ko/en) 우선, 없으면 시스템 로케일.
pub fn current() -> Lang {
    match crate::settings::language().as_deref() {
        Some("ko") => Lang::Ko,
        Some("en") => Lang::En,
        _ => match sys_locale::get_locale() {
            Some(l) if l.to_lowercase().starts_with("ko") => Lang::Ko,
            _ => Lang::En,
        },
    }
}

impl Lang {
    pub fn menu_open(self) -> &'static str {
        match self {
            Lang::Ko => "열기",
            Lang::En => "Open",
        }
    }
    pub fn menu_settings(self) -> &'static str {
        match self {
            Lang::Ko => "설정...",
            Lang::En => "Settings...",
        }
    }
    pub fn menu_quit(self) -> &'static str {
        match self {
            Lang::Ko => "종료",
            Lang::En => "Quit",
        }
    }
    pub fn alert_title(self) -> &'static str {
        match self {
            Lang::Ko => "🛬 Token Runway 경보",
            Lang::En => "🛬 Token Runway Alert",
        }
    }
    pub fn reset_title(self) -> &'static str {
        match self {
            Lang::Ko => "🛬 토큰 리셋 임박",
            Lang::En => "🛬 Token Reset Soon",
        }
    }
    /// 잔여율 경보 본문.
    pub fn alert_low(self, tool: &str, pct: f64) -> String {
        match self {
            Lang::Ko => format!("{tool} 런웨이 {pct:.0}% 남음"),
            Lang::En => format!("{tool}: {pct:.0}% remaining"),
        }
    }
    /// 예상 소진 경보 본문.
    pub fn alert_eta(self, tool: &str, mins: f64, pct: f64) -> String {
        match self {
            Lang::Ko => format!("{tool} 약 {mins:.0}분 후 소진 ({pct:.0}% 남음)"),
            Lang::En => format!("{tool}: runs out in ~{mins:.0} min ({pct:.0}% left)"),
        }
    }
    /// 리셋 임박 본문.
    pub fn alert_reset(self, tool: &str, mins: f64, pct: f64) -> String {
        match self {
            Lang::Ko => format!("{tool} 약 {mins:.0}분 후 리셋 — {pct:.0}% 남음, 지금 더 써도 됩니다"),
            Lang::En => {
                format!("{tool} resets in ~{mins:.0} min — {pct:.0}% left, use it now")
            }
        }
    }
}
