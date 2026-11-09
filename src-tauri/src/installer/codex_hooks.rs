//! Codex `hooks.json`에 `trw hook codex` 훅을 넣고 뺀다.
//!
//! 오르카가 같은 파일에 자기 훅(`orca-agent-hook-form=1`)을 둔다. 우리 표시가 있는
//! 그룹만 건드리고 나머지는 한 바이트도 바꾸지 않는다. Codex는 훅 신뢰를
//! `hooks.json:<event>:<그룹 번호>:<훅 번호>`로 기억해서, 기존 그룹 사이에 끼우면
//! 오르카 훅 번호가 밀려 신뢰가 풀린다. 그래서 항상 배열 끝에 붙인다.

use super::template::{self, find, J, Kind, Loaded, Plan, Record, MARKER};
use std::path::{Path, PathBuf};

/// 훅을 거는 Codex 이벤트. `trw hook codex`가 상태로 옮기는 목록과 같다.
pub const EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop", "Interrupt"];

const HOOK_TIMEOUT_SECS: i64 = 10;

pub fn hooks_file(home: &Path) -> PathBuf {
    home.join("hooks.json")
}

/// 훅 명령. CODEX_HOME을 인자로 박는다 — 훅 프로세스에 그 환경변수가 안 넘어오는
/// 경우가 있어 상태 파일에 어느 홈인지 남기려면 명령에 들어 있어야 한다.
pub fn command(trw: &Path, event: &str, home: &Path) -> String {
    format!(
        ": {MARKER}; {} hook codex {event} --home {}",
        template::shell_quote(&trw.to_string_lossy()),
        template::shell_quote(&home.to_string_lossy())
    )
}

fn group(command: String) -> J {
    J::Obj(vec![(
        "hooks",
        J::Arr(vec![J::Obj(vec![
            ("type", J::Str("command".into())),
            ("command", J::Str(command)),
            ("timeout", J::Num(HOOK_TIMEOUT_SECS)),
        ])]),
    )])
}

/// 그룹 안의 훅이 전부 우리 것일 때만 우리 그룹이다. 사용자가 우리 훅을 자기 그룹에
/// 옮겨 넣었으면 그 그룹은 지우지 않는다 — 같이 있는 사용자 훅까지 사라진다.
fn is_ours(group_text: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(group_text) else {
        return false;
    };
    let Some(hooks) = v.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    !hooks.is_empty()
        && hooks.iter().all(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains(MARKER))
        })
}

fn path(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

fn file_label(file: &Path) -> String {
    file.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.display().to_string())
}

/// `parent` 객체에 `key` 멤버가 없으면 `empty`로 만든다. 있는데 모양이 다르면 멈춘다.
fn ensure_member(
    text: &str,
    parent: &[String],
    key: &str,
    want_array: bool,
    unit: &str,
    record: &mut Record,
    label: &str,
) -> Result<String, String> {
    let root = template::parse(text)?;
    let node = find(&root, parent).ok_or(format!("{label} 구조를 찾지 못했습니다"))?;
    if !matches!(node.kind, Kind::Object(_)) {
        return Err(format!("{label}의 {} 가 객체가 아닙니다", parent.join(".")));
    }
    if let Some((_, m)) = node.member(key) {
        let ok = match m.value.kind {
            Kind::Array(_) => want_array,
            Kind::Object(_) => !want_array,
            Kind::Scalar => false,
        };
        return if ok {
            Ok(text.to_string())
        } else {
            Err(format!("{label}의 {key} 모양이 예상과 달라 건드리지 않았습니다"))
        };
    }
    let empty = if want_array { J::Arr(vec![]) } else { J::Obj(vec![]) };
    let (out, inner) = template::append_child(text, node, unit, |ind| {
        format!("{}: {}", template::quote(key), empty.render(ind, unit))
    });
    let mut created = parent.to_vec();
    created.push(key.to_string());
    record.note_created(created);
    record.note_inner(parent, inner);
    Ok(out)
}

pub fn plan_install(
    file: &Path,
    home: &Path,
    trw: &Path,
    record: Option<Record>,
) -> Result<Plan, String> {
    plan_install_groups(file, EVENTS, |event| command(trw, event, home), record, None)
}

/// `hooks.<이벤트>` 배열마다 우리 그룹 하나를 끝에 붙인다. Codex `hooks.json`과 Claude
/// `settings.json`이 같은 모양(`hooks.<이벤트>: [{hooks: [{type, command, timeout}]}]`)이라 같이 쓴다.
pub(super) fn plan_install_groups(
    file: &Path,
    events: &[&str],
    command_for: impl Fn(&str) -> String,
    record: Option<Record>,
    tag: Option<&'static str>,
) -> Result<Plan, String> {
    let label = file_label(file);
    let Loaded { text, stamp } = template::load(file)?;
    let before = text.clone();
    let mut record = record.unwrap_or_default();
    let mut text = match text {
        Some(t) => t,
        None => {
            record.created_file = true;
            "{}\n".to_string()
        }
    };
    let unit = template::detect_unit(&text);
    text = ensure_member(&text, &[], "hooks", false, &unit, &mut record, &label)?;
    for event in events {
        let hooks = path(&["hooks"]);
        text = ensure_member(&text, &hooks, event, true, &unit, &mut record, &label)?;
        let arr_path = path(&["hooks", event]);
        let cmd = command_for(event);
        let root = template::parse(&text)?;
        let arr = find(&root, &arr_path).ok_or(format!("{label} 구조를 찾지 못했습니다"))?;
        let Kind::Array(items) = &arr.kind else {
            return Err(format!("{label}의 {event} 가 배열이 아닙니다"));
        };
        match items.iter().find(|n| is_ours(&text[n.start..n.end])) {
            // 이미 있으면 명령이 같을 때 그대로 둔다. trw 경로가 바뀌었으면 그 자리에서 바꾼다.
            Some(existing) => {
                let current = &text[existing.start..existing.end];
                if !current.contains(&template::quote(&cmd)) {
                    let indent = template::line_indent(&text, existing.start);
                    text = template::replace_node(&text, existing, &group(cmd).render(&indent, &unit));
                }
            }
            None => {
                let (out, inner) =
                    template::append_child(&text, arr, &unit, |ind| group(cmd.clone()).render(ind, &unit));
                record.note_inner(&arr_path, inner);
                text = out;
            }
        }
    }
    Ok(Plan {
        path: file.to_path_buf(),
        before,
        after: Some(text),
        record: Some(record),
        tag,
        stamp,
    })
}

pub fn plan_uninstall(file: &Path, record: Option<Record>) -> Result<Plan, String> {
    plan_uninstall_groups(file, record, None)
}

pub(super) fn plan_uninstall_groups(
    file: &Path,
    record: Option<Record>,
    tag: Option<&'static str>,
) -> Result<Plan, String> {
    let label = file_label(file);
    let Loaded { text, stamp } = template::load(file)?;
    let before = text.clone();
    let record = record.unwrap_or_default();
    let Some(mut text) = text else {
        return Ok(Plan {
            path: file.to_path_buf(),
            before: None,
            after: None,
            record: None,
            tag,
            stamp,
        });
    };
    loop {
        let root = template::parse(&text)?;
        if let Some((event, idx)) = find_our_group(&root, &text) {
            let arr_path = path(&["hooks", &event]);
            let arr = find(&root, &arr_path).ok_or(format!("{label} 구조를 찾지 못했습니다"))?;
            text = template::remove_child(&text, arr, idx, record.inner(&arr_path));
            continue;
        }
        // 우리가 만든 멤버가 비었으면 걷어낸다. 깊은 것부터 — 이벤트 배열을 지워야 hooks가 빈다.
        let mut created = record.created.clone();
        created.sort_by_key(|p| std::cmp::Reverse(p.len()));
        let removable = created.iter().find_map(|p| {
            let (key, parent_path) = p.split_last()?;
            let parent = find(&root, parent_path)?;
            let (idx, m) = parent.member(key)?;
            m.value.is_empty_container().then(|| (parent, parent_path, idx))
        });
        match removable {
            Some((parent, parent_path, idx)) => {
                text = template::remove_child(&text, parent, idx, record.inner(parent_path));
            }
            None => break,
        }
    }
    let root = template::parse(&text)?;
    let after = if record.created_file && root.is_empty_container() {
        None
    } else {
        Some(text)
    };
    Ok(Plan {
        path: file.to_path_buf(),
        before,
        after,
        record: None,
        tag,
        stamp,
    })
}

fn find_our_group(root: &template::Node, text: &str) -> Option<(String, usize)> {
    let (_, hooks) = root.member("hooks")?;
    let Kind::Object(events) = &hooks.value.kind else {
        return None;
    };
    events.iter().find_map(|ev| {
        let Kind::Array(items) = &ev.value.kind else {
            return None;
        };
        items
            .iter()
            .position(|n| is_ours(&text[n.start..n.end]))
            .map(|idx| (ev.key.clone(), idx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const ORCA_HOOKS: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": ": orca-agent-hook-form=1; /bin/sh \"${HOME-}/.orca/agent-hooks/codex-hook.sh\" || :",
            "timeout": 10
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": ": orca-agent-hook-form=1; /bin/sh \"${HOME-}/.orca/agent-hooks/codex-hook.sh\" || :",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
"#;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("trw-codex-hooks-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(d.join("app")).unwrap();
            Tmp(d)
        }
        fn app(&self) -> PathBuf {
            self.0.join("app")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn install_uninstall(tmp: &Tmp, file: &Path) {
        let trw = Path::new("/Users/me/.local/bin/trw");
        let home = file.parent().unwrap();
        let plan = plan_install(file, home, trw, template::record_for(&tmp.app(), file)).unwrap();
        template::apply(&plan, &tmp.app()).unwrap();
        let installed = fs::read_to_string(file).unwrap();
        for ev in EVENTS {
            assert!(installed.contains(&format!("hook codex {ev} --home")), "{ev}");
        }
        // 두 번 깔아도 그대로다.
        let again = plan_install(file, home, trw, template::record_for(&tmp.app(), file)).unwrap();
        assert!(!again.changes());
        template::apply(&again, &tmp.app()).unwrap();

        let plan = plan_uninstall(file, template::record_for(&tmp.app(), file)).unwrap();
        template::apply(&plan, &tmp.app()).unwrap();
    }

    #[test]
    fn install_then_uninstall_is_byte_identical_with_orca_entries() {
        let tmp = Tmp::new("orca");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, ORCA_HOOKS).unwrap();
        install_uninstall(&tmp, &file);
        assert_eq!(fs::read_to_string(&file).unwrap(), ORCA_HOOKS);
    }

    #[test]
    fn orca_groups_keep_their_index() {
        let tmp = Tmp::new("index");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, ORCA_HOOKS).unwrap();
        let plan = plan_install(&file, &tmp.0, Path::new("/t/trw"), None).unwrap();
        let after: serde_json::Value = serde_json::from_str(plan.after.as_deref().unwrap()).unwrap();
        let stop = after["hooks"]["Stop"].as_array().unwrap();
        assert!(stop[0]["hooks"][0]["command"].as_str().unwrap().contains("orca-agent-hook-form=1"));
        assert!(stop[1]["hooks"][0]["command"].as_str().unwrap().contains(MARKER));
    }

    #[test]
    fn missing_file_is_created_and_removed() {
        let tmp = Tmp::new("missing");
        let file = tmp.0.join("hooks.json");
        install_uninstall(&tmp, &file);
        assert!(!file.exists());
    }

    #[test]
    fn compact_and_empty_files_round_trip() {
        for (tag, original) in [("empty-obj", "{}"), ("compact", r#"{"hooks":{"Stop":[]}}"#)] {
            let tmp = Tmp::new(tag);
            let file = tmp.0.join("hooks.json");
            fs::write(&file, original).unwrap();
            install_uninstall(&tmp, &file);
            assert_eq!(fs::read_to_string(&file).unwrap(), original, "{tag}");
        }
    }

    #[test]
    fn change_after_planning_blocks_the_write() {
        let tmp = Tmp::new("race");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, ORCA_HOOKS).unwrap();
        let plan = plan_install(&file, &tmp.0, Path::new("/t/trw"), None).unwrap();
        // 오르카가 그사이 자기 훅을 다시 썼다.
        let changed = ORCA_HOOKS.replace("\"timeout\": 10", "\"timeout\": 11");
        fs::write(&file, &changed).unwrap();
        assert!(template::apply(&plan, &tmp.app()).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), changed);
    }

    #[test]
    fn writes_leave_a_backup() {
        let tmp = Tmp::new("backup");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, ORCA_HOOKS).unwrap();
        let plan = plan_install(&file, &tmp.0, Path::new("/t/trw"), None).unwrap();
        assert!(plan.diff().lines().any(|l| l.starts_with('+') && l.contains(MARKER)));
        template::apply(&plan, &tmp.app()).unwrap();
        let backups: Vec<_> = fs::read_dir(&tmp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("hooks.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), ORCA_HOOKS);
    }

    #[test]
    fn corrupt_file_is_kept_and_install_stops() {
        let tmp = Tmp::new("corrupt");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, "{\"hooks\": {").unwrap();
        assert!(plan_install(&file, &tmp.0, Path::new("/t/trw"), None).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "{\"hooks\": {");
        let copies = fs::read_dir(&tmp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("hooks.json.corrupt-"))
            .count();
        assert_eq!(copies, 1);
    }

    #[test]
    fn group_mixed_with_user_hooks_is_not_removed() {
        let mixed = format!(
            r#"{{"hooks":{{"Stop":[{{"hooks":[{{"type":"command","command":": {MARKER}; x"}},{{"type":"command","command":"say hi"}}]}}]}}}}"#
        );
        let tmp = Tmp::new("mixed");
        let file = tmp.0.join("hooks.json");
        fs::write(&file, &mixed).unwrap();
        let plan = plan_uninstall(&file, None).unwrap();
        assert!(!plan.changes());
    }
}
