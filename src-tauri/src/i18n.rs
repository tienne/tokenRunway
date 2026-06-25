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
    pub fn settings_title(self) -> &'static str {
        match self {
            Lang::Ko => "Token Runway 설정",
            Lang::En => "Token Runway Settings",
        }
    }
    pub fn menu_history(self) -> &'static str {
        match self {
            Lang::Ko => "히스토리...",
            Lang::En => "History...",
        }
    }
    pub fn menu_check_update(self) -> &'static str {
        match self {
            Lang::Ko => "업데이트 확인...",
            Lang::En => "Check for Updates...",
        }
    }
    /// 시작 시 새 버전 감지 알림 제목 (뒤에 버전 붙임)
    pub fn update_available_title(self) -> &'static str {
        match self {
            Lang::Ko => "새 버전 사용 가능",
            Lang::En => "Update available",
        }
    }
    pub fn update_available_body(self) -> &'static str {
        match self {
            Lang::Ko => "트레이 메뉴 '업데이트 확인'에서 설치하세요.",
            Lang::En => "Install it from the tray menu — 'Check for Updates'.",
        }
    }
    pub fn update_installing(self) -> &'static str {
        match self {
            Lang::Ko => "업데이트 설치 중... 완료되면 자동 재시작됩니다.",
            Lang::En => "Installing update... the app will restart when done.",
        }
    }
    pub fn update_uptodate(self) -> &'static str {
        match self {
            Lang::Ko => "이미 최신 버전입니다.",
            Lang::En => "You're on the latest version.",
        }
    }
    pub fn update_failed(self) -> &'static str {
        match self {
            Lang::Ko => "업데이트 확인 실패 — 네트워크를 확인하세요.",
            Lang::En => "Update check failed — check your network.",
        }
    }
    pub fn history_title(self) -> &'static str {
        match self {
            Lang::Ko => "Token Runway 히스토리",
            Lang::En => "Token Runway History",
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
