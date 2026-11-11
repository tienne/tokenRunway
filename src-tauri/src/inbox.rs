//! 알림함 — `trw`로 들어온 작업 완료 알림의 큐와 영속.
//!
//! 저장 위치: `~/.token-runway/inbox.json`
//!
//! 읽음 처리는 "항목을 눌러 랜딩했다"는 뜻이다. 리스트를 열어보는 것만으로는
//! 읽음이 되지 않는다 — 배지가 곧 "아직 확인 안 한 완료 건수"여야 하기 때문이다.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
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
    Waiting,
    Interrupted,
    TurnEnd,
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
pub struct InboxQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<InboxOption>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<InboxQuestion>,
    #[serde(default)]
    pub level: Level,
    /// 리스트 한 줄에 띄울 출처 — 워크트리 이름이나 브랜치.
    #[serde(default)]
    pub context: Option<String>,
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    pub created_ms: i64,
    #[serde(default)]
    pub read: bool,
    /// 같은 에이전트 세션에서 온 알림을 한 줄로 묶는 키(`claude:<session_id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Stop 훅이 턴마다 남긴 줄 — OS 알림과 진동 없이 알림함에만 쌓인다.
    #[serde(default)]
    pub quiet: bool,
}

fn target_path(target: &Target) -> Option<&str> {
    match target {
        Target::Orca { worktree_path, .. } => worktree_path.as_deref(),
        Target::Paseo { cwd, .. } => cwd.as_deref(),
        Target::Path { path } => Some(path),
        Target::None => None,
    }
}

fn project_root(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors() {
        let dot_git = ancestor.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let contents = fs::read_to_string(&dot_git).ok()?;
            ancestor.join(contents.trim().strip_prefix("gitdir:")?.trim())
        } else {
            continue;
        };
        let common_dir = match fs::read_to_string(git_dir.join("commondir")) {
            Ok(relative) => git_dir.join(relative.trim()),
            Err(_) => return ancestor.canonicalize().ok(),
        }.canonicalize().ok()?;
        // 연결된 워크트리도 공통 Git 디렉터리를 따라 원본 프로젝트로 돌아간다.
        return if common_dir.file_name().is_some_and(|name| name == ".git") {
            common_dir.parent().map(Path::to_path_buf)
        } else {
            Some(common_dir)
        };
    }
    None
}

fn resolve_project(target: &Target) -> Option<String> {
    project_root(Path::new(target_path(target)?)).map(|path| path.to_string_lossy().into_owned())
}

/// `trw`가 소켓으로 보내는 요청 본문. `InboxItem`과 달리 id·시각·읽음은 앱이 채운다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyRequest {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<InboxQuestion>,
    #[serde(default)]
    pub level: Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Level>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default = "target_none")]
    pub target: Target,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub quiet: bool,
    /// `trw`가 보낸 시각. 스풀에서 늦게 꺼낸 알림도 실제로 끝난 시각으로 남기려고 받는다.
    #[serde(default)]
    pub sent_ms: Option<i64>,
}

/// 에이전트가 `trw notify`로 직접 알린 직후엔 Stop 훅 줄이 그걸 덮지 않는다 — 에이전트가 쓴
/// 제목이 훅이 응답 첫 줄에서 뽑은 제목보다 낫다.
const KEEP_EXPLICIT_MS: i64 = 2 * 60 * 1000;

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
    match decode_inbox(&raw) {
        Ok(f) => f,
        Err(_) => {
            crate::atomicfile::preserve_corrupt(&path);
            InboxFile::default()
        }
    }
}

fn decode_inbox(raw: &str) -> Result<InboxFile, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    if let Some(items) = value.get_mut("items").and_then(|items| items.as_array_mut()) {
        for item in items {
            if let Some(status) = item.get("status").and_then(|value| value.as_str()) {
                if matches!(status, "waiting" | "interrupted" | "turnEnd") {
                    item["level"] = status.into();
                }
            }
        }
    }
    serde_json::from_value(value)
}

fn encode_inbox(file: &InboxFile) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(file)?;
    if let Some(items) = value.get_mut("items").and_then(|items| items.as_array_mut()) {
        for item in items {
            let legacy = match item.get("level").and_then(|value| value.as_str()) {
                Some("waiting") => "blocked",
                Some("interrupted") => "failed",
                Some("turnEnd") => "done",
                _ => continue,
            };
            // 구버전도 알림함을 읽을 수 있게 새 종류는 별도 필드에 보관한다.
            item["status"] = item["level"].clone();
            item["level"] = legacy.into();
        }
    }
    serde_json::to_string_pretty(&value)
}

fn save(file: &InboxFile) {
    if let Some(path) = inbox_path() {
        if let Ok(json) = encode_inbox(file) {
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

/// 알림을 큐에 넣고 만들어진 항목을 돌려준다. Stop 훅 줄이 직전 명시 알림에 밀려 버려지면 None.
///
/// 키가 같은 항목은 세션당 한 줄만 남긴다 — 턴마다 쌓이면 모니터링 루프 하나가 목록을 다 채운다.
pub fn add(req: NotifyRequest) -> Option<InboxItem> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut item = InboxItem {
        id: new_id(now_ms),
        title: req.title,
        body: req.body,
        prompt: req.prompt,
        questions: req.questions,
        level: req.status.unwrap_or(req.level),
        context: req.context,
        project_path: resolve_project(&req.target),
        target: req.target,
        created_ms: req.sent_ms.filter(|&t| t > 0 && t <= now_ms).unwrap_or(now_ms),
        read: false,
        key: req.key,
        quiet: req.quiet,
    };
    let mut f = INBOX.lock().ok()?;
    if !merge_by_key(&mut f.items, &mut item) {
        return None;
    }
    f.items.push(item.clone());
    prune(&mut f.items, now_ms);
    save(&f);
    Some(item)
}

/// 같은 키의 이전 줄을 지운다. 새 줄을 넣지 말아야 하면 false.
fn merge_by_key(items: &mut Vec<InboxItem>, new: &mut InboxItem) -> bool {
    let Some(key) = new.key.as_deref() else {
        return true;
    };
    let same = |i: &InboxItem| i.key.as_deref() == Some(key);
    if new.quiet && new.level == Level::Waiting && new.questions.is_empty() {
        if let Some(previous) = items.iter().rev().find(|item| same(item) && item.level == Level::Waiting && !item.read) {
            new.questions = previous.questions.clone();
            if new.prompt.is_none() { new.prompt = previous.prompt.clone(); }
        }
    }
    if new.quiet
        && matches!(new.level, Level::Done | Level::TurnEnd)
        && items
            .iter()
            .any(|i| same(i) && !i.quiet && !i.read && new.created_ms - i.created_ms < KEEP_EXPLICIT_MS)
    {
        return false;
    }
    items.retain(|i| !same(i));
    true
}

/// 최신순 전체 목록.
pub fn all() -> Vec<InboxItem> {
    let mut items = match INBOX.lock() {
        Ok(f) => f.items.clone(),
        Err(_) => return Vec::new(),
    };
    let mut projects = HashMap::new();
    for item in &mut items {
        if item.project_path.is_none() {
            if let Some(path) = target_path(&item.target) {
                item.project_path = projects.entry(path.to_owned())
                    .or_insert_with(|| resolve_project(&item.target)).clone();
            }
        }
    }
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

pub fn mark_read_ids(ids: &[String]) {
    if let Ok(mut f) = INBOX.lock() {
        mark_matching_read(&mut f.items, ids);
        save(&f);
    }
}

fn mark_matching_read(items: &mut [InboxItem], ids: &[String]) {
    for item in items {
        if ids.contains(&item.id) {
            item.read = true;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, quiet: bool, created_ms: i64) -> InboxItem {
        InboxItem {
            id: format!("{created_ms}"),
            title: String::new(),
            body: None,
            prompt: None,
            questions: Vec::new(),
            level: Level::default(),
            context: None,
            target: Target::None,
            project_path: None,
            created_ms,
            read: false,
            key: Some(key.to_string()),
            quiet,
        }
    }

    #[test]
    fn linked_worktrees_and_nested_folders_share_original_project() {
        let root = std::env::temp_dir().join(format!("trw-project-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let repo = root.join("project");
        let worktree = root.join("feature");
        let git_dir = repo.join(".git/worktrees/feature");
        fs::create_dir_all(&git_dir).unwrap();
        fs::create_dir_all(worktree.join("src/components")).unwrap();
        fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        fs::write(worktree.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        let expected = repo.canonicalize().unwrap();
        assert_eq!(project_root(&repo), Some(expected.clone()));
        assert_eq!(project_root(&worktree.join("src/components")), Some(expected));
        assert_eq!(project_root(&root.join("unrelated")), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_notifications_accept_missing_project_path() {
        let mut value = serde_json::to_value(item("a", false, 1)).unwrap();
        value.as_object_mut().unwrap().remove("projectPath");
        let parsed: InboxItem = serde_json::from_value(value).unwrap();
        assert!(parsed.project_path.is_none());
    }

    #[test]
    fn urgent_hook_notice_replaces_recent_explicit_completion() {
        for level in [Level::Waiting, Level::Blocked, Level::Interrupted, Level::Failed] {
            let mut items = vec![item("claude:a", false, 0)];
            let mut incoming = item("claude:a", true, 1_000);
            incoming.level = level;
            assert!(merge_by_key(&mut items, &mut incoming));
            assert!(items.is_empty());
        }
        let mut items = vec![item("claude:a", false, 0)];
        let mut incoming = item("claude:a", true, 1_000);
        incoming.level = Level::TurnEnd;
        assert!(!merge_by_key(&mut items, &mut incoming));
    }

    #[test]
    fn new_statuses_keep_the_disk_format_readable_by_older_versions() {
        for (level, legacy) in [(Level::Waiting, "blocked"), (Level::Interrupted, "failed"), (Level::TurnEnd, "done")] {
            let mut entry = item("a", true, 1);
            entry.level = level;
            let file = InboxFile { version: 1, items: vec![entry] };
            let json = encode_inbox(&file).unwrap();
            let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(raw["items"][0]["level"], legacy);
            assert!(raw["items"][0].get("status").is_some());
            assert_eq!(decode_inbox(&json).unwrap().items[0].level, level);
            assert!(serde_json::from_str::<InboxFile>(&json).is_ok());
        }
    }

    #[test]
    fn questions_survive_storage_and_old_notifications_default_to_empty() {
        let mut entry = item("a", true, 1);
        assert!(serde_json::to_value(&entry).unwrap().get("questions").is_none());
        entry.level = Level::Waiting;
        entry.questions = vec![InboxQuestion {
            question: "어느 환경으로 진행할까요?".into(),
            options: vec![InboxOption { label: "스테이징".into(), description: Some("검증 환경".into()) }],
            multi_select: false,
        }];
        let encoded = encode_inbox(&InboxFile { version: 1, items: vec![entry] }).unwrap();
        let parsed = decode_inbox(&encoded).unwrap();
        assert_eq!(parsed.items[0].questions[0].options[0].label, "스테이징");
        assert_eq!(parsed.items[0].questions[0].options[0].description.as_deref(), Some("검증 환경"));
    }

    #[test]
    fn repeated_waiting_notice_keeps_pending_question_details() {
        let mut previous = item("claude:a", true, 0);
        previous.level = Level::Waiting;
        previous.questions = vec![InboxQuestion { question: "어느 환경인가요?".into(), options: vec![], multi_select: false }];
        let mut items = vec![previous];
        let mut incoming = item("claude:a", true, 1);
        incoming.level = Level::Waiting;
        assert!(merge_by_key(&mut items, &mut incoming));
        assert_eq!(incoming.questions[0].question, "어느 환경인가요?");
    }

    #[test]
    fn prompt_survives_storage_and_is_optional_for_legacy_notifications() {
        let mut entry = item("a", true, 1);
        assert!(serde_json::to_value(&entry).unwrap().get("prompt").is_none());
        entry.prompt = Some("알림을 프로젝트별로 묶어 주세요.\n기존 이동 기능은 유지해 주세요.".into());
        let json = encode_inbox(&InboxFile { version: 1, items: vec![entry.clone()] }).unwrap();
        assert_eq!(decode_inbox(&json).unwrap().items[0].prompt, entry.prompt);
    }

    #[test]
    fn requests_accept_legacy_levels_and_preserve_precise_status() {
        for (status, legacy) in [("waiting", "blocked"), ("interrupted", "failed"), ("turnEnd", "done")] {
            let request: NotifyRequest = serde_json::from_value(serde_json::json!({
                "title": "혼합 버전 알림", "level": legacy, "status": status
            })).unwrap();
            assert_eq!(serde_json::to_value(request.status.unwrap_or(request.level)).unwrap(), status);
        }
        let request: NotifyRequest = serde_json::from_str(r#"{"title":"이전 알림","level":"done"}"#).unwrap();
        assert_eq!(request.status, None);
        assert_eq!(request.level, Level::Done);
    }

    #[test]
    fn notification_levels_round_trip() {
        for value in ["done", "waiting", "blocked", "interrupted", "failed", "turnEnd"] {
            let level: Level = serde_json::from_value(value.into()).unwrap();
            assert_eq!(serde_json::to_value(level).unwrap(), value);
        }
    }

    #[test]
    fn group_read_leaves_other_and_new_notifications_unread() {
        let mut items = vec![item("a", false, 1), item("b", false, 2), item("a", false, 3)];
        mark_matching_read(&mut items, &["1".into(), "missing".into()]);
        assert!(items[0].read);
        assert!(!items[1].read);
        assert!(!items[2].read);
    }

    #[test]
    fn hook_line_replaces_previous_hook_line_of_same_session() {
        let mut items = vec![item("claude:a", true, 0), item("claude:b", true, 0)];
        assert!(merge_by_key(&mut items, &mut item("claude:a", true, 10_000)));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.as_deref(), Some("claude:b"));
    }

    #[test]
    fn hook_line_does_not_override_fresh_explicit_notify() {
        let mut items = vec![item("claude:a", false, 0)];
        assert!(!merge_by_key(&mut items, &mut item("claude:a", true, 5_000)));
        assert_eq!(items.len(), 1);
        assert!(merge_by_key(&mut items, &mut item("claude:a", true, KEEP_EXPLICIT_MS)));
        assert!(items.is_empty());
    }
}
