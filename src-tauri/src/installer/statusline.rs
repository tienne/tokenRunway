//! Claude `settings.json`의 `statusLine`을 `trw statusline`으로 바꾸고 되돌린다.
//!
//! statusLine은 하나뿐이라 우리가 자리를 차지하면 원래 명령이 밀려난다. 그래서 원래 값을
//! `~/.token-runway/statusline.json`에 config dir별로 남기고, `trw`가 그 명령을 대신
//! 돌려 출력을 그대로 내보낸다. 제거하면 남겨둔 원문을 그 자리에 다시 넣는다.

use super::template::{self, J, Kind, Loaded, Plan, Record, MARKER};
use crate::atomicfile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// 원래 statusLine. `trw`는 `command`만 읽고, 앱은 제거할 때 `raw`를 되돌려 넣는다.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Original {
    /// 원래 statusLine이 명령형이었으면 그 명령.
    pub command: Option<String>,
    /// 원래 값의 원문. 키가 없었으면 None.
    pub raw: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Originals {
    #[serde(default)]
    originals: BTreeMap<String, Original>,
}

/// settings.json 하나를 어떻게 바꿀지 + 원래 statusLine을 어떻게 남길지.
pub struct StatuslinePlan {
    pub plan: Plan,
    pub config_dir: String,
    /// Some이면 이 값을 남기고, None이면 남긴 값을 지운다.
    pub original: Option<Original>,
    /// 원래 statusLine 기록을 건드려야 하는지. 이미 깔려 있으면 기록을 덮지 않는다.
    pub touch_original: bool,
}

pub fn settings_file(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

/// `trw`가 `CLAUDE_CONFIG_DIR`로 찾는 키와 같은 모양으로 맞춘다.
pub fn config_key(config_dir: &Path) -> String {
    let s = config_dir.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() { s.to_string() } else { trimmed.to_string() }
}

pub fn command(trw: &Path) -> String {
    format!(
        ": {MARKER}; {} statusline",
        template::shell_quote(&trw.to_string_lossy())
    )
}

fn value(trw: &Path) -> J {
    J::Obj(vec![
        ("type", J::Str("command".into())),
        ("command", J::Str(command(trw))),
    ])
}

fn originals_path(app_dir: &Path) -> PathBuf {
    app_dir.join("statusline.json")
}

fn load_originals(app_dir: &Path) -> Originals {
    let path = originals_path(app_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Originals::default();
    };
    match serde_json::from_str(&raw) {
        Ok(o) => o,
        Err(_) => {
            atomicfile::preserve_corrupt(&path);
            Originals::default()
        }
    }
}


fn save_original(app_dir: &Path, config_dir: &str, original: Option<&Original>) -> Result<(), String> {
    let mut o = load_originals(app_dir);
    match original {
        Some(v) => {
            o.originals.insert(config_dir.to_string(), v.clone());
        }
        None => {
            o.originals.remove(config_dir);
        }
    }
    let raw = serde_json::to_string_pretty(&o).map_err(|e| e.to_string())?;
    atomicfile::write_atomic(&originals_path(app_dir), &raw).map_err(|e| e.to_string())
}

fn root_object(text: &str) -> Result<template::Node, String> {
    let root = template::parse(text)?;
    if !matches!(root.kind, Kind::Object(_)) {
        return Err("settings.json 최상위가 객체가 아니라 건드리지 않았습니다".into());
    }
    Ok(root)
}

pub fn plan_install(
    file: &Path,
    config_dir: &str,
    trw: &Path,
    record: Option<Record>,
) -> Result<StatuslinePlan, String> {
    let Loaded { text, stamp } = template::load(file)?;
    let before = text.clone();
    let mut record = record.unwrap_or_default();
    let text = match text {
        Some(t) => t,
        None => {
            record.created_file = true;
            "{}\n".to_string()
        }
    };
    let unit = template::detect_unit(&text);
    let root = root_object(&text)?;
    let (after, original, touch) = match root.member("statusLine") {
        Some((_, m)) => {
            let raw = &text[m.value.start..m.value.end];
            if raw.contains(MARKER) {
                // 이미 우리 것이다. 원래 값 기록을 덮으면 진짜 원래 명령을 잃는다.
                (text.clone(), None, false)
            } else {
                let parsed: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
                let command = (parsed.get("type").and_then(|t| t.as_str()) == Some("command"))
                    .then(|| parsed.get("command").and_then(|c| c.as_str()).map(str::to_string))
                    .flatten();
                let indent = template::line_indent(&text, m.key_start);
                let replaced = template::replace_node(&text, &m.value, &value(trw).render(&indent, &unit));
                let original = Original {
                    command,
                    raw: Some(raw.to_string()),
                };
                (replaced, Some(original), true)
            }
        }
        None => {
            let (added, inner) = template::append_child(&text, &root, &unit, |ind| {
                format!("\"statusLine\": {}", value(trw).render(ind, &unit))
            });
            record.note_created(vec!["statusLine".into()]);
            record.note_inner(&[], inner);
            (added, Some(Original::default()), true)
        }
    };
    Ok(StatuslinePlan {
        plan: Plan {
            path: file.to_path_buf(),
            before,
            after: Some(after),
            record: Some(record),
            tag: None,
            stamp,
        },
        config_dir: config_dir.to_string(),
        original,
        touch_original: touch,
    })
}

pub fn plan_uninstall(
    file: &Path,
    config_dir: &str,
    record: Option<Record>,
    original: Option<Original>,
) -> Result<StatuslinePlan, String> {
    let Loaded { text, stamp } = template::load(file)?;
    let before = text.clone();
    let record = record.unwrap_or_default();
    let done = |after: Option<String>, stamp| StatuslinePlan {
        plan: Plan {
            path: file.to_path_buf(),
            before: before.clone(),
            after,
            record: None,
            tag: None,
            stamp,
        },
        config_dir: config_dir.to_string(),
        original: None,
        touch_original: true,
    };
    let Some(text) = text else {
        return Ok(done(None, stamp));
    };
    let root = root_object(&text)?;
    let Some((idx, m)) = root.member("statusLine") else {
        return Ok(done(Some(text), stamp));
    };
    if !text[m.value.start..m.value.end].contains(MARKER) {
        // 사용자가 그사이 다른 statusLine으로 바꿨다. 그 값을 지키고 기록만 정리한다.
        return Ok(done(Some(text), stamp));
    }
    let restored = match original.and_then(|o| o.raw) {
        Some(raw) => template::replace_node(&text, &m.value, &raw),
        None => template::remove_child(&text, &root, idx, record.inner(&[])),
    };
    let root = template::parse(&restored)?;
    let after = if record.created_file && root.is_empty_container() {
        None
    } else {
        Some(restored)
    };
    Ok(done(after, stamp))
}

/// 설치는 원래 값을 먼저 남기고 쓴다 — 반대로 하면 그 사이 불린 `trw`가 원래 명령을
/// 못 찾아 상태줄이 한 번 빈다. 제거는 되돌린 뒤에 기록을 지운다.
pub fn apply(sp: &StatuslinePlan, app_dir: &Path, installing: bool) -> Result<(), String> {
    if installing && sp.touch_original {
        save_original(app_dir, &sp.config_dir, sp.original.as_ref())?;
    }
    template::apply(&sp.plan, app_dir)?;
    if !installing && sp.touch_original {
        save_original(app_dir, &sp.config_dir, None)?;
    }
    Ok(())
}

/// 제거 계획에 넘길 원래 값.
pub fn original_of(app_dir: &Path, config_dir: &str) -> Option<Original> {
    load_originals(app_dir).originals.remove(config_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("trw-statusline-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(d.join("app")).unwrap();
            fs::create_dir_all(d.join("claude")).unwrap();
            Tmp(d)
        }
        fn app(&self) -> PathBuf {
            self.0.join("app")
        }
        fn dir(&self) -> PathBuf {
            self.0.join("claude")
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const TRW: &str = "/Users/me/.local/bin/trw";

    fn install(tmp: &Tmp) -> String {
        let file = settings_file(&tmp.dir());
        let key = config_key(&tmp.dir());
        let sp = plan_install(&file, &key, Path::new(TRW), template::record_for(&tmp.app(), &file)).unwrap();
        apply(&sp, &tmp.app(), true).unwrap();
        fs::read_to_string(&file).unwrap_or_default()
    }

    fn uninstall(tmp: &Tmp) {
        let file = settings_file(&tmp.dir());
        let key = config_key(&tmp.dir());
        let sp = plan_uninstall(
            &file,
            &key,
            template::record_for(&tmp.app(), &file),
            original_of(&tmp.app(), &key),
        )
        .unwrap();
        apply(&sp, &tmp.app(), false).unwrap();
    }

    #[test]
    fn existing_statusline_is_saved_and_restored_byte_for_byte() {
        let tmp = Tmp::new("existing");
        let original = "{\n  \"model\": \"opus\",\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"bash ~/.claude/statusline.sh\",\n    \"padding\": 0\n  },\n  \"hooks\": {}\n}\n";
        let file = settings_file(&tmp.dir());
        fs::write(&file, original).unwrap();

        let installed = install(&tmp);
        assert!(installed.contains(MARKER));
        assert!(installed.contains("\"model\": \"opus\""));
        let key = config_key(&tmp.dir());
        let saved = original_of(&tmp.app(), &key).unwrap();
        assert_eq!(saved.command.as_deref(), Some("bash ~/.claude/statusline.sh"));

        // 두 번 깔아도 원래 명령 기록이 우리 명령으로 덮이지 않는다.
        install(&tmp);
        assert_eq!(original_of(&tmp.app(), &key).unwrap(), saved);

        uninstall(&tmp);
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
        assert!(original_of(&tmp.app(), &key).is_none());
    }

    #[test]
    fn absent_statusline_is_added_and_removed() {
        let tmp = Tmp::new("absent");
        let original = "{\n  \"model\": \"opus\"\n}\n";
        let file = settings_file(&tmp.dir());
        fs::write(&file, original).unwrap();
        install(&tmp);
        uninstall(&tmp);
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn missing_settings_file_is_created_and_removed() {
        let tmp = Tmp::new("missing");
        install(&tmp);
        assert!(settings_file(&tmp.dir()).exists());
        uninstall(&tmp);
        assert!(!settings_file(&tmp.dir()).exists());
    }

    #[test]
    fn other_key_changed_after_planning_blocks_the_write() {
        let tmp = Tmp::new("race");
        let original = "{\n  \"model\": \"opus\"\n}\n";
        let file = settings_file(&tmp.dir());
        fs::write(&file, original).unwrap();
        let key = config_key(&tmp.dir());
        let sp = plan_install(&file, &key, Path::new(TRW), None).unwrap();
        // Claude Code가 그사이 다른 키를 고쳤다.
        let changed = "{\n  \"model\": \"sonnet\"\n}\n";
        fs::write(&file, changed).unwrap();
        assert!(apply(&sp, &tmp.app(), true).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), changed);
    }

    #[test]
    fn user_replaced_statusline_is_left_alone() {
        let tmp = Tmp::new("replaced");
        let file = settings_file(&tmp.dir());
        fs::write(&file, "{\n  \"model\": \"opus\"\n}\n").unwrap();
        install(&tmp);
        let mine = "{\n  \"model\": \"opus\",\n  \"statusLine\": {\"type\": \"command\", \"command\": \"echo hi\"}\n}\n";
        fs::write(&file, mine).unwrap();
        uninstall(&tmp);
        assert_eq!(fs::read_to_string(&file).unwrap(), mine);
    }

    #[test]
    fn symlinked_settings_stay_a_symlink() {
        let tmp = Tmp::new("symlink");
        let real = tmp.0.join("dotfiles-settings.json");
        fs::write(&real, "{\n  \"model\": \"opus\"\n}\n").unwrap();
        std::os::unix::fs::symlink(&real, settings_file(&tmp.dir())).unwrap();
        install(&tmp);
        let link = settings_file(&tmp.dir());
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(fs::read_to_string(&real).unwrap().contains(MARKER));
    }
}
