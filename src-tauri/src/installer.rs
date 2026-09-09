//! `trw` CLI와 에이전트 스킬을 사용자 환경에 설치한다.
//!
//! 둘 다 앱 번들에 들어 있다 — `trw`는 externalBin으로 `Contents/MacOS/trw`,
//! 스킬은 resources로 `Contents/Resources/skills/`. 여기서 하는 일은 그것들을
//! PATH와 에이전트 스킬 디렉토리에 이어주는 것뿐이다.
//!
//! 스킬 배치는 커뮤니티 skills CLI 규약을 따른다 — 실물은 `~/.agents/skills/<이름>`에
//! 두고 에이전트별 디렉토리에는 상대 심링크를 건다. 그러면 한 번 갱신하면 모든
//! 에이전트에 반영되고, 에이전트를 늘려도 심링크만 추가하면 된다.

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 번들에 담긴 스킬 이름. 늘어나면 여기에 추가한다.
const SKILLS: &[&str] = &["trw-notify"];

/// 스킬 심링크를 걸 후보. 홈에 해당 디렉토리가 있는 에이전트만 대상으로 삼는다 —
/// 없는 에이전트까지 만들면 쓰지도 않는 설정 디렉토리를 흩뿌리게 된다.
const AGENT_DIRS: &[&str] = &[".claude", ".codex", ".gemini"];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStatus {
    /// `~/.local/bin/trw`가 있으면 그 경로. 무엇을 가리키는지는 보지 않는다 —
    /// 사용자가 직접 놓은 것이어도 설치된 상태로 본다.
    pub cli_path: Option<String>,
    /// 설치된 스킬 이름.
    pub skills: Vec<String>,
    /// 스킬 심링크가 걸린 에이전트(`.claude` 등에서 점을 뗀 이름).
    pub agents: Vec<String>,
}

fn home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "홈 디렉토리를 찾을 수 없습니다".to_string())
}

/// 번들에 들어 있는 `trw` 실행 파일. externalBin은 앱 실행 파일과 같은
/// 디렉토리에 놓이므로 dev 실행(`target/debug/`)에서도 같은 규칙으로 찾힌다.
fn bundled_cli() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe
        .parent()
        .ok_or_else(|| "실행 파일 경로를 알 수 없습니다".to_string())?;
    let cli = dir.join("trw");
    if cli.is_file() {
        Ok(cli)
    } else {
        Err(format!("번들에서 trw를 찾을 수 없습니다: {}", cli.display()))
    }
}

fn bundled_skills_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("skills");
    if dir.is_dir() {
        Ok(dir)
    } else {
        Err(format!("번들에서 스킬을 찾을 수 없습니다: {}", dir.display()))
    }
}

fn bin_dir() -> Result<PathBuf, String> {
    Ok(home()?.join(".local/bin"))
}

fn shared_skills_dir() -> Result<PathBuf, String> {
    Ok(home()?.join(".agents/skills"))
}

/// 심링크를 만들거나 이미 있는 것을 교체한다. 일반 파일이 자리를 차지하고 있으면
/// 건드리지 않는다 — 사용자가 직접 놓은 것일 수 있고, 지우면 되돌릴 수 없다.
fn link(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    match fs::symlink_metadata(dst) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(dst).map_err(|e| e.to_string())?;
        }
        Ok(_) => {
            return Err(format!(
                "{} 에 심링크가 아닌 파일이 있어 건너뜁니다",
                dst.display()
            ))
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink(src, dst).map_err(|e| e.to_string())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// 홈에 설정 디렉토리가 있는 에이전트만.
fn detected_agents() -> Vec<PathBuf> {
    let Ok(home) = home() else { return Vec::new() };
    AGENT_DIRS
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// `trw`를 PATH에 올리고 스킬을 에이전트에 깐다.
///
/// 되돌리기 쉬운 것만 한다 — 심링크와 복사뿐이고 남의 설정 파일은 건드리지 않는다.
pub fn install(app: &AppHandle) -> Result<InstallStatus, String> {
    let mut notes: Vec<String> = Vec::new();

    // 1) CLI 심링크
    let cli = bundled_cli()?;
    let target = bin_dir()?.join("trw");
    link(&cli, &target)?;

    // 2) 스킬 실물을 공용 디렉토리에 복사
    let src_root = bundled_skills_dir(app)?;
    let shared = shared_skills_dir()?;
    let mut installed: Vec<String> = Vec::new();
    for name in SKILLS {
        let src = src_root.join(name);
        if !src.is_dir() {
            notes.push(format!("{name} 스킬이 번들에 없습니다"));
            continue;
        }
        let dst = shared.join(name);
        // 갱신이므로 기존 내용을 지우고 다시 쓴다. 사용자가 수정했더라도 번들 쪽이
        // 정본이라 덮는 게 맞다.
        let _ = fs::remove_dir_all(&dst);
        copy_dir(&src, &dst)?;
        installed.push((*name).to_string());
    }

    // 3) 에이전트별 심링크
    let mut agents: Vec<String> = Vec::new();
    for agent_dir in detected_agents() {
        let name = agent_dir
            .file_name()
            .map(|s| s.to_string_lossy().trim_start_matches('.').to_string())
            .unwrap_or_default();
        let mut linked_any = false;
        for skill in &installed {
            let dst = agent_dir.join("skills").join(skill);
            // 에이전트 디렉토리에서 공용 디렉토리로 올라가는 상대 경로 —
            // 홈이 옮겨져도 링크가 살아 있게 한다.
            let rel = Path::new("../../.agents/skills").join(skill);
            match link(&rel, &dst) {
                Ok(()) => linked_any = true,
                Err(e) => notes.push(e),
            }
        }
        if linked_any {
            agents.push(name);
        }
    }

    if !notes.is_empty() {
        eprintln!("trw 설치 참고: {}", notes.join(" / "));
    }

    Ok(InstallStatus {
        cli_path: Some(target.to_string_lossy().to_string()),
        skills: installed,
        agents,
    })
}

/// 지금 설치 상태. 설정 화면이 "설치됨"을 판단하는 근거다.
pub fn status() -> InstallStatus {
    let cli_path = bin_dir()
        .ok()
        .map(|d| d.join("trw"))
        .filter(|p| fs::symlink_metadata(p).is_ok())
        .map(|p| p.to_string_lossy().to_string());

    let shared = shared_skills_dir().ok();
    let skills: Vec<String> = SKILLS
        .iter()
        .filter(|name| {
            shared
                .as_ref()
                .map(|s| s.join(name).join("SKILL.md").is_file())
                .unwrap_or(false)
        })
        .map(|s| (*s).to_string())
        .collect();

    let agents: Vec<String> = detected_agents()
        .into_iter()
        .filter(|dir| {
            SKILLS
                .iter()
                .any(|s| fs::symlink_metadata(dir.join("skills").join(s)).is_ok())
        })
        .filter_map(|dir| {
            dir.file_name()
                .map(|s| s.to_string_lossy().trim_start_matches('.').to_string())
        })
        .collect();

    InstallStatus {
        cli_path,
        skills,
        agents,
    }
}
