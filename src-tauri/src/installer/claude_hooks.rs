//! Claude `settings.json`에 `trw hook claude` 훅을 넣고 뺀다.
//!
//! 규칙은 `codex_hooks`와 같다 — 우리 표시가 있는 그룹만 건드리고, 항상 `hooks.<이벤트>`
//! 배열 끝에 붙인다. 오르카가 같은 파일에 `agent-hooks/claude-hook.sh` 훅을 두고,
//! 사용자 훅과 `statusLine` 설치기도 같은 파일을 고친다. 설치 기록은 `TAG`로 나눠
//! statusLine 기록과 섞이지 않게 한다.

use super::codex_hooks;
use super::template::{Plan, Record, MARKER};
use std::path::Path;

/// 훅을 거는 이벤트. `trw hook claude`가 상태로 옮기는 목록과 같다.
///
/// SubagentStop은 걸지 않는다 — 서브에이전트가 끝나도 부모 턴은 아직 돈다.
/// PermissionRequest도 걸지 않는다 — 권한 대기는 Notification(`permission_prompt`)이 알려준다.
/// PostToolUse·PostToolUseFailure는 도구가 끝난 뒤 다시 `working`으로 시각을 갱신해, 권한을
/// 승인한 뒤 `waiting`이 남지 않게 하고 긴 턴에서 상태가 낡지 않게 한다.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// 설치 기록 태그. statusLine 설치기가 같은 settings.json을 태그 없이 쓴다.
pub const TAG: &str = "claude-hooks";

/// 훅 명령. config dir을 인자로 박는다 — 오르카가 프로필마다 `CLAUDE_CONFIG_DIR`을 바꾸는데,
/// 훅 프로세스 환경변수만 믿으면 어느 계정 세션인지 못 가르는 경우가 있다.
pub fn command(trw: &Path, event: &str, config_dir: &str) -> String {
    format!(
        ": {MARKER}; {} hook claude {event} --config-dir {}",
        super::template::shell_quote(&trw.to_string_lossy()),
        super::template::shell_quote(config_dir)
    )
}

pub fn plan_install(
    file: &Path,
    config_dir: &str,
    trw: &Path,
    record: Option<Record>,
) -> Result<Plan, String> {
    codex_hooks::plan_install_groups(
        file,
        EVENTS,
        |event| command(trw, event, config_dir),
        record,
        Some(TAG),
    )
}

pub fn plan_uninstall(file: &Path, record: Option<Record>) -> Result<Plan, String> {
    codex_hooks::plan_uninstall_groups(file, record, Some(TAG))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::{statusline, template};
    use std::fs;
    use std::path::PathBuf;

    /// 오르카가 실제로 쓰는 모양 — 도구 이벤트는 `matcher: "*"`, 나머지는 matcher가 없다.
    /// 사용자 훅(Bash matcher, resume matcher)과 statusLine 외 설정도 함께 둔다.
    const ORCA_SETTINGS: &str = r#"{
  "env": {
    "FOO": "1"
  },
  "permissions": {
    "allow": [
      "Bash(git status)"
    ]
  },
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "if [ -z \"${HOME-}\" ]; then :; else /bin/sh \"$HOME/.orca/agent-hooks/claude-hook.sh\"; fi",
            "timeout": 10
          }
        ]
      },
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "~/.claude/hooks/detect-gh-pr.sh"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "if [ -z \"${HOME-}\" ]; then :; else /bin/sh \"$HOME/.orca/agent-hooks/claude-hook.sh\"; fi",
            "timeout": 10
          }
        ]
      },
      {
        "hooks": [
          {
            "type": "command",
            "command": "\"$HOME/.local/bin/trw\" hook stop"
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "matcher": "resume",
        "hooks": [
          {
            "type": "command",
            "command": "python3 ~/.claude/hooks/resume-context-warn.py"
          }
        ]
      }
    ]
  }
}
"#;

    const TRW: &str = "/Users/me/.local/bin/trw";

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("trw-claude-hooks-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(d.join("app")).unwrap();
            Tmp(d)
        }
        fn app(&self) -> PathBuf {
            self.0.join("app")
        }
        fn file(&self) -> PathBuf {
            self.0.join("settings.json")
        }
        fn key(&self) -> String {
            statusline::config_key(&self.0)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn install_hooks(tmp: &Tmp) {
        let file = tmp.file();
        let record = template::record_or_inherited(&tmp.app(), &file, Some(TAG));
        let plan = plan_install(&file, &tmp.key(), Path::new(TRW), record).unwrap();
        template::apply(&plan, &tmp.app()).unwrap();
    }

    fn uninstall_hooks(tmp: &Tmp) {
        let file = tmp.file();
        let record = template::record_or_inherited(&tmp.app(), &file, Some(TAG));
        let plan = plan_uninstall(&file, record).unwrap();
        template::apply(&plan, &tmp.app()).unwrap();
    }

    fn install_statusline(tmp: &Tmp) {
        let file = tmp.file();
        let record = template::record_or_inherited(&tmp.app(), &file, None);
        let sp = statusline::plan_install(&file, &tmp.key(), Path::new(TRW), record).unwrap();
        statusline::apply(&sp, &tmp.app(), true).unwrap();
    }

    fn uninstall_statusline(tmp: &Tmp) {
        let file = tmp.file();
        let record = template::record_or_inherited(&tmp.app(), &file, None);
        let original = statusline::original_of(&tmp.app(), &tmp.key());
        let sp = statusline::plan_uninstall(&file, &tmp.key(), record, original).unwrap();
        statusline::apply(&sp, &tmp.app(), false).unwrap();
    }

    fn read(tmp: &Tmp) -> Option<String> {
        fs::read_to_string(tmp.file()).ok()
    }

    #[test]
    fn install_then_uninstall_is_byte_identical_with_orca_entries() {
        let tmp = Tmp::new("orca");
        fs::write(tmp.file(), ORCA_SETTINGS).unwrap();
        install_hooks(&tmp);
        let installed = read(&tmp).unwrap();
        for ev in EVENTS {
            assert!(installed.contains(&format!("hook claude {ev} --config-dir")), "{ev}");
        }
        // 두 번 깔아도 그대로다.
        let record = template::record_or_inherited(&tmp.app(), &tmp.file(), Some(TAG));
        let again = plan_install(&tmp.file(), &tmp.key(), Path::new(TRW), record).unwrap();
        assert!(!again.changes());

        uninstall_hooks(&tmp);
        assert_eq!(read(&tmp).unwrap(), ORCA_SETTINGS);
    }

    #[test]
    fn our_groups_go_last_and_others_are_untouched() {
        let tmp = Tmp::new("order");
        fs::write(tmp.file(), ORCA_SETTINGS).unwrap();
        let plan = plan_install(&tmp.file(), &tmp.key(), Path::new(TRW), None).unwrap();
        let before: serde_json::Value = serde_json::from_str(ORCA_SETTINGS).unwrap();
        let after: serde_json::Value = serde_json::from_str(plan.after.as_deref().unwrap()).unwrap();
        for ev in ["PreToolUse", "Stop", "SessionStart"] {
            let old = before["hooks"][ev].as_array().unwrap();
            let new = after["hooks"][ev].as_array().unwrap();
            assert_eq!(&new[..old.len()], &old[..], "{ev}: 기존 그룹 순서와 내용 유지");
            assert_eq!(new.len(), old.len() + 1);
            let ours = new.last().unwrap();
            assert!(ours.get("matcher").is_none(), "matcher 생략 = 전부 매칭");
            assert!(ours["hooks"][0]["command"].as_str().unwrap().contains(MARKER));
        }
        assert_eq!(after["env"], before["env"]);
        assert_eq!(after["permissions"], before["permissions"]);
        assert!(after["hooks"].get("SubagentStop").is_none());
    }

    #[test]
    fn missing_file_is_created_and_removed() {
        let tmp = Tmp::new("missing");
        install_hooks(&tmp);
        assert!(tmp.file().exists());
        uninstall_hooks(&tmp);
        assert!(!tmp.file().exists());
    }

    #[test]
    fn coexists_with_statusline_in_every_order() {
        // (설치 순서, 제거 순서). true = hooks.
        let orders = [
            ([true, false], [true, false]),
            ([true, false], [false, true]),
            ([false, true], [true, false]),
            ([false, true], [false, true]),
        ];
        let originals: [(&str, Option<&str>); 4] = [
            ("orca", Some(ORCA_SETTINGS)),
            ("missing", None),
            ("empty", Some("{}")),
            ("spaced", Some("{\n}\n")),
        ];
        for (name, original) in originals {
            for (i, (installs, removes)) in orders.iter().enumerate() {
                let tmp = Tmp::new(&format!("mix-{name}-{i}"));
                if let Some(text) = original {
                    fs::write(tmp.file(), text).unwrap();
                }
                for hooks in installs {
                    if *hooks { install_hooks(&tmp) } else { install_statusline(&tmp) }
                }
                let both = read(&tmp).unwrap();
                assert!(both.contains("hook claude Stop"), "{name}/{i}");
                assert!(both.contains("statusline\""), "{name}/{i}");
                let mut halves = Vec::new();
                for hooks in removes {
                    if *hooks { uninstall_hooks(&tmp) } else { uninstall_statusline(&tmp) }
                    halves.push(read(&tmp));
                }
                let first = halves[0].as_deref().unwrap();
                // 하나만 뺐을 땐 다른 쪽이 그대로 남아 있어야 한다.
                if removes[0] {
                    assert!(!first.contains("hook claude") && first.contains("statusline\""), "{name}/{i}");
                } else {
                    assert!(first.contains("hook claude") && !first.contains("statusline\""), "{name}/{i}");
                }
                assert_eq!(read(&tmp).as_deref(), original, "{name}/{i}");
            }
        }
    }

    #[test]
    fn corrupt_settings_are_kept_and_install_stops() {
        let tmp = Tmp::new("corrupt");
        fs::write(tmp.file(), "{\"hooks\": {").unwrap();
        assert!(plan_install(&tmp.file(), &tmp.key(), Path::new(TRW), None).is_err());
        assert_eq!(read(&tmp).unwrap(), "{\"hooks\": {");
        let copies = fs::read_dir(&tmp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.corrupt-"))
            .count();
        assert_eq!(copies, 1);
    }

    #[test]
    fn change_after_planning_blocks_the_write() {
        let tmp = Tmp::new("race");
        fs::write(tmp.file(), ORCA_SETTINGS).unwrap();
        let plan = plan_install(&tmp.file(), &tmp.key(), Path::new(TRW), None).unwrap();
        // 오르카가 그사이 자기 훅을 다시 썼다.
        let changed = ORCA_SETTINGS.replace("\"timeout\": 10", "\"timeout\": 11");
        fs::write(tmp.file(), &changed).unwrap();
        assert!(template::apply(&plan, &tmp.app()).is_err());
        assert_eq!(read(&tmp).unwrap(), changed);
    }

    #[test]
    fn writes_leave_a_backup() {
        let tmp = Tmp::new("backup");
        fs::write(tmp.file(), ORCA_SETTINGS).unwrap();
        let plan = plan_install(&tmp.file(), &tmp.key(), Path::new(TRW), None).unwrap();
        assert!(plan.diff().lines().any(|l| l.starts_with('+') && l.contains(MARKER)));
        template::apply(&plan, &tmp.app()).unwrap();
        let backups: Vec<_> = fs::read_dir(&tmp.0)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0].path()).unwrap(), ORCA_SETTINGS);
    }
}
