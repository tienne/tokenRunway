//! 알림함 — `trw`로 들어온 작업 완료 알림의 큐와 영속.
//!
//! 저장 위치: `~/.token-runway/inbox.json`
//!
//! 읽음 처리는 "항목을 눌러 랜딩했다"는 뜻이다. 리스트를 열어보는 것만으로는
//! 읽음이 되지 않는다 — 배지가 곧 "아직 확인 안 한 완료 건수"여야 하기 때문이다.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

/// 보관 상한. 넘으면 오래된 것부터 버린다.
const MAX_ITEMS: usize = 200;

/// 읽은 항목의 보관 기간. 읽었다고 바로 지우지 않는 이유는 랜딩을 한 번 하고도
/// "아까 그거 뭐였지" 하고 되돌아갈 일이 있어서다.
const READ_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    /// 작업이 끝났다.
    Done,
    /// 권한 대기 등으로 멈춰 사람을 기다린다.
    Blocked,
    /// 실패로 끝났다.
    Failed,
}

impl Default for Level {
    fn default() -> Self {
        Level::Done
    }
}

/// 알림을 눌렀을 때 돌아갈 곳.
///
/// 임의 셸 명령을 담지 않는다 — 소켓으로 들어온 값이 그대로 실행기가 되면
/// 훅 스크립트가 오염됐을 때 그대로 통로가 된다. 알려진 종류만 받고 명령은
/// `landing`이 조립한다.
#[derive(Debug, Clone, Serialize, Deserialize)]
// enum의 `rename_all`은 variant 이름만 바꾼다. 필드까지 camelCase로 내보내려면
// `rename_all_fields`가 따로 필요하다 — 없으면 `worktreeId`가 파싱되지 않는다.
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Target {
    /// Orca 워크트리의 터미널 탭.
    Orca {
        worktree_id: String,
        #[serde(default)]
        worktree_path: Option<String>,
        /// 발송 시점의 탭. 탭이 닫혀 있으면 같은 워크트리의 다른 터미널로 폴백한다.
        #[serde(default)]
        tab_id: Option<String>,
    },
    /// Paseo 에이전트.
    Paseo {
        agent_id: String,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// 그 밖의 경로 — 에디터/Finder로 연다.
    Path { path: String },
    /// 갈 데가 없는 알림.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub level: Level,
    /// 리스트 한 줄에 띄울 출처 — 워크트리 이름이나 브랜치.
    #[serde(default)]
    pub context: Option<String>,
    pub target: Target,
    pub created_ms: i64,
    #[serde(default)]
    pub read: bool,
}

/// `trw`가 소켓으로 보내는 요청 본문. `InboxItem`과 달리 id·시각·읽음은 앱이 채운다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyRequest {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default = "target_none")]
    pub target: Target,
}

fn target_none() -> Target {
    Target::None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InboxFile {
    version: u32,
    items: Vec<InboxItem>,
}

impl Default for InboxFile {
    fn default() -> Self {
        Self {
            version: 1,
            items: Vec::new(),
        }
    }
}

static INBOX: LazyLock<Mutex<InboxFile>> = LazyLock::new(|| Mutex::new(load_from_disk()));

fn inbox_path() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("inbox.json"))
}

fn load_from_disk() -> InboxFile {
    let Some(path) = inbox_path() else {
        return InboxFile::default();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return InboxFile::default();
    };
    match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(_) => {
            crate::atomicfile::preserve_corrupt(&path);
            InboxFile::default()
        }
    }
}

fn save(file: &InboxFile) {
    if let Some(path) = inbox_path() {
        if let Ok(json) = serde_json::to_string_pretty(file) {
            let _ = crate::atomicfile::write_atomic(&path, &json);
        }
    }
}

/// 오래된 읽음 항목과 상한 초과분을 정리. 읽지 않은 건 나이와 무관하게 남긴다 —
/// 배지가 사라지지 않는 대신 놓친 알림이 조용히 없어지지도 않는다.
fn prune(items: &mut Vec<InboxItem>, now_ms: i64) {
    items.retain(|i| !i.read || now_ms - i.created_ms < READ_RETENTION_MS);
    if items.len() > MAX_ITEMS {
        let drop = items.len() - MAX_ITEMS;
        items.drain(0..drop);
    }
}

fn new_id(now_ms: i64) -> String {
    // 알림 하나를 가리키기만 하면 되므로 시각 + 프로세스 카운터로 충분하다.
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{now_ms:x}-{n:x}")
}

/// 알림을 큐에 넣고 만들어진 항목을 돌려준다.
pub fn add(req: NotifyRequest) -> InboxItem {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let item = InboxItem {
        id: new_id(now_ms),
        title: req.title,
        body: req.body,
        level: req.level,
        context: req.context,
        target: req.target,
        created_ms: now_ms,
        read: false,
    };
    if let Ok(mut f) = INBOX.lock() {
        f.items.push(item.clone());
        prune(&mut f.items, now_ms);
        save(&f);
    }
    item
}

/// 최신순 전체 목록.
pub fn all() -> Vec<InboxItem> {
    let Ok(f) = INBOX.lock() else {
        return Vec::new();
    };
    let mut items = f.items.clone();
    items.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
    items
}

pub fn unread_count() -> usize {
    INBOX
        .lock()
        .map(|f| f.items.iter().filter(|i| !i.read).count())
        .unwrap_or(0)
}

/// 항목 하나를 찾아 읽음으로 바꾸고 그 항목을 돌려준다(랜딩에 target이 필요하다).
pub fn mark_read(id: &str) -> Option<InboxItem> {
    let mut f = INBOX.lock().ok()?;
    let item = f.items.iter_mut().find(|i| i.id == id)?;
    item.read = true;
    let out = item.clone();
    save(&f);
    Some(out)
}

pub fn mark_all_read() {
    if let Ok(mut f) = INBOX.lock() {
        for i in f.items.iter_mut() {
            i.read = true;
        }
        save(&f);
    }
}

pub fn remove(id: &str) {
    if let Ok(mut f) = INBOX.lock() {
        f.items.retain(|i| i.id != id);
        save(&f);
    }
}

/// 항목 하나를 id로 조회 — 랜딩 전 target을 읽는 데 쓴다.
pub fn get(id: &str) -> Option<InboxItem> {
    INBOX
        .lock()
        .ok()?
        .items
        .iter()
        .find(|i| i.id == id)
        .cloned()
}

pub fn clear() {
    if let Ok(mut f) = INBOX.lock() {
        f.items.clear();
        save(&f);
    }
}
