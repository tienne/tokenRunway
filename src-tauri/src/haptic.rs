//! MX Master 4 진동 — Logi 플러그인(`logi-plugin/`)이 여는 소켓에 이벤트 이름을 보낸다.
//!
//! 진동 모터는 Logi Actions SDK 플러그인 안에서만 열려 있어 앱이 직접 울릴 수 없다.
//! 플러그인이 없거나 마우스가 꺼져 있으면 연결이 실패하고 조용히 넘어간다 — 진동은
//! OS 알림에 얹는 덤이라 실패가 알림 흐름을 막으면 안 된다.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::activity::{ActivityState, AgentActivity};
use crate::settings;

/// 플러그인 `HapticEvents`와 이름이 같아야 한다. 진동 패턴은 플러그인의
/// `events/extra/eventMapping.yaml`에서 고른다.
#[derive(Clone, Copy)]
pub enum Haptic {
    TaskDone,
    TurnDone,
    QuotaLow,
    EtaDanger,
    ResetSoon,
}

impl Haptic {
    fn event_name(self) -> &'static str {
        match self {
            Haptic::TaskDone => "taskDone",
            Haptic::TurnDone => "turnDone",
            Haptic::QuotaLow => "quotaLow",
            Haptic::EtaDanger => "etaDanger",
            Haptic::ResetSoon => "resetSoon",
        }
    }

    /// 쿨다운 안이라도 직전보다 높은 쪽은 울린다. 오르카 세션을 여럿 돌리면 턴 종료가
    /// 수시로 울려서, 같은 등급으로만 막으면 정작 trw 완료나 경보가 묻힌다.
    fn priority(self) -> u8 {
        match self {
            Haptic::TurnDone => 0,
            Haptic::TaskDone | Haptic::ResetSoon => 1,
            Haptic::QuotaLow | Haptic::EtaDanger => 2,
        }
    }
}

const WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// 진동은 겹쳐 와도 한 번만 울린다. 에이전트는 `trw notify`를 부른 뒤 턴을 마쳐서 완료 알림과
/// 턴 종료가 몇 초 간격으로 붙어 오고, 계정이 여럿이면 경보도 같은 루프에서 연달아 나온다.
/// OS 알림은 그대로 다 뜨니 진동만 줄여도 놓치는 건 없다.
const COOLDOWN: Duration = Duration::from_secs(15);

static LAST_FIRED: Mutex<Option<(Instant, u8)>> = Mutex::new(None);

/// 도구별로 마지막에 울린 턴 종료 시각 — `JustDone`은 8초 동안 유지되고 그동안
/// 1.5초마다 폴링되므로, 같은 턴에 여러 번 울리지 않게 `since_ms`로 가른다.
static SEEN_TURN_ENDS: LazyLock<Mutex<HashMap<String, i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 경보 루프와 IPC 스레드에서 부르므로 소켓 쓰기는 따로 스레드로 뺀다.
pub fn fire(haptic: Haptic) {
    if !settings::haptics_enabled() || !claim(&LAST_FIRED, Instant::now(), haptic.priority()) {
        return;
    }
    std::thread::spawn(move || {
        let _ = send(haptic.event_name());
    });
}

/// pet이 손을 흔드는 순간(턴 종료)에 같이 울린다. `muted`여도 본 턴은 기록해 둔다 —
/// 안 그러면 방해금지가 풀리는 순간 그 사이 끝난 턴이 한꺼번에 울린다.
pub fn on_activity(activities: &[AgentActivity], muted: bool) {
    let Ok(mut seen) = SEEN_TURN_ENDS.lock() else {
        return;
    };
    let mut fresh = false;
    for a in activities {
        if a.state == ActivityState::JustDone
            && seen.insert(a.tool.clone(), a.since_ms) != Some(a.since_ms)
        {
            fresh = true;
        }
    }
    drop(seen);
    if !fresh || muted || !settings::haptics_on_turn_end() {
        return;
    }
    fire(Haptic::TurnDone);
}

fn claim(last_fired: &Mutex<Option<(Instant, u8)>>, now: Instant, priority: u8) -> bool {
    let Ok(mut last) = last_fired.lock() else {
        return false;
    };
    if last.is_some_and(|(t, p)| now.duration_since(t) < COOLDOWN && priority <= p) {
        return false;
    }
    *last = Some((now, priority));
    true
}

fn send(name: &str) -> std::io::Result<()> {
    let Some(path) = settings::app_dir().map(|d| d.join("haptic.sock")) else {
        return Ok(());
    };
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.write_all(format!("{name}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_drops_bursts_within_cooldown() {
        let slot = Mutex::new(None);
        let t0 = Instant::now();
        assert!(claim(&slot, t0, 1));
        assert!(!claim(&slot, t0 + Duration::from_secs(3), 1));
        assert!(!claim(&slot, t0 + COOLDOWN - Duration::from_millis(1), 1));
        assert!(claim(&slot, t0 + COOLDOWN, 1));
    }

    #[test]
    fn claim_lets_higher_priority_through_once() {
        let slot = Mutex::new(None);
        let t0 = Instant::now();
        let (turn, task) = (Haptic::TurnDone.priority(), Haptic::TaskDone.priority());
        assert!(claim(&slot, t0, turn));
        assert!(claim(&slot, t0 + Duration::from_secs(5), task));
        assert!(!claim(&slot, t0 + Duration::from_secs(8), turn));
        assert!(!claim(&slot, t0 + Duration::from_secs(9), task));
    }
}
