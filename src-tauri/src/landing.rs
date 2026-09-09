//! 알림을 눌렀을 때 원래 세션으로 되돌려보내는 어댑터.
//!
//! Orca와 Paseo 모두 URL scheme(`orca://`, `paseo://`)은 pair·app 정도만 라우팅해서
//! 세션 딥링크로 못 쓴다. 그래서 각 CLI를 호출한다.
//!
//! 앱은 LaunchAgent나 Finder에서 뜨면 PATH가 최소라 `orca`/`paseo`를 이름으로
//! 부를 수 없다. 후보 경로를 직접 훑는다.

use crate::inbox::Target;
use std::path::{Path, PathBuf};
use std::process::Command;

const OPEN_BIN: &str = "/usr/bin/open";

fn find_bin(name: &str, extra: &[&str]) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin").join(name));
    }
    for p in ["/usr/local/bin", "/opt/homebrew/bin"] {
        candidates.push(Path::new(p).join(name));
    }
    for p in extra {
        candidates.push(PathBuf::from(p));
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn orca_bin() -> Option<PathBuf> {
    find_bin("orca", &["/Applications/Orca.app/Contents/Resources/bin/orca"])
}

fn paseo_bin() -> Option<PathBuf> {
    find_bin("paseo", &["/Applications/Paseo.app/Contents/Resources/bin/paseo"])
}

/// 앱을 앞으로 가져온다. CLI가 UI 탭을 전환해도 앱 자체가 뒤에 있으면 안 보인다.
fn activate_app(app_name: &str) {
    let _ = Command::new(OPEN_BIN).args(["-a", app_name]).status();
}

/// Orca `terminal list`에서 원하는 터미널의 handle을 찾는다.
///
/// 발송 시점의 handle을 굳혀두지 않는 이유는 터미널이 재생성되면 handle이 바뀌기
/// 때문이다. 그래서 누를 때 다시 찾는다.
fn orca_find_handle(bin: &Path, worktree_id: &str, tab_id: Option<&str>) -> Option<String> {
    let out = Command::new(bin).args(["terminal", "list", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let terminals = v.get("result")?.get("terminals")?.as_array()?;

    let alive = |t: &serde_json::Value| !t.get("orphaned").and_then(|b| b.as_bool()).unwrap_or(false);

    // 1순위: 알림이 온 그 탭.
    if let Some(tab) = tab_id {
        if let Some(h) = terminals
            .iter()
            .filter(|t| alive(t))
            .find(|t| t.get("tabId").and_then(|s| s.as_str()) == Some(tab))
            .and_then(|t| t.get("handle").and_then(|s| s.as_str()))
        {
            return Some(h.to_string());
        }
    }
    // 2순위: 탭이 닫혔으면 같은 워크트리의 아무 살아있는 터미널. 워크트리까지는 데려간다.
    terminals
        .iter()
        .filter(|t| alive(t))
        .find(|t| t.get("worktreeId").and_then(|s| s.as_str()) == Some(worktree_id))
        .and_then(|t| t.get("handle").and_then(|s| s.as_str()))
        .map(|s| s.to_string())
}

/// 워크트리에 살아있는 터미널이 하나도 없을 때 — 파일을 하나 열어 워크트리로 데려간다.
///
/// 여기서 터미널을 새로 만들지는 않는다. 알림 하나 눌렀다고 세션이 생기는 건 과하다.
fn orca_open_worktree(bin: &Path, worktree_id: &str, worktree_path: Option<&str>) {
    // 경로가 안 담겨 온 알림도 있을 수 있다. worktreeId가 `<repo-uuid>::<절대경로>`
    // 꼴이라 거기서 뽑아 쓴다.
    let fallback = worktree_id.split("::").nth(1);
    let Some(root) = worktree_path.or(fallback) else {
        return;
    };
    let candidates = ["README.md", "README", "package.json", "Cargo.toml", ".gitignore"];
    let file = candidates
        .iter()
        .find(|f| Path::new(root).join(f).is_file())
        .copied();
    if let Some(f) = file {
        let _ = Command::new(bin)
            .args(["file", "open", f, "--worktree", &format!("id:{worktree_id}")])
            .status();
    }
}

fn land_orca(worktree_id: &str, worktree_path: Option<&str>, tab_id: Option<&str>) -> Result<(), String> {
    let bin = orca_bin().ok_or("orca CLI를 찾을 수 없습니다")?;
    // `open -a`는 앱을 띄우자마자 리턴해서, 꺼져 있던 경우 런타임이 올라오기 전에
    // `terminal list`를 부르게 된다. `orca open`은 런타임이 닿을 때까지 기다린다
    // (이미 떠 있으면 바로 리턴한다).
    let _ = Command::new(&bin).arg("open").status();
    activate_app("Orca");
    match orca_find_handle(&bin, worktree_id, tab_id) {
        Some(handle) => {
            Command::new(&bin)
                .args(["terminal", "switch", "--terminal", &handle])
                .status()
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        None => {
            orca_open_worktree(&bin, worktree_id, worktree_path);
            Ok(())
        }
    }
}

fn land_paseo(agent_id: &str) -> Result<(), String> {
    let bin = paseo_bin().ok_or("paseo CLI를 찾을 수 없습니다")?;
    activate_app("Paseo");
    Command::new(&bin)
        .args(["agent", "open", agent_id])
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn land_path(path: &str) -> Result<(), String> {
    Command::new(OPEN_BIN)
        .arg(path)
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 알림 하나의 랜딩을 수행. 외부 프로세스를 기다리므로 UI 스레드에서 부르지 않는다.
pub fn land(target: &Target) -> Result<(), String> {
    match target {
        Target::Orca {
            worktree_id,
            worktree_path,
            tab_id,
        } => land_orca(worktree_id, worktree_path.as_deref(), tab_id.as_deref()),
        Target::Paseo { agent_id, .. } => land_paseo(agent_id),
        Target::Path { path } => land_path(path),
        Target::None => Ok(()),
    }
}
