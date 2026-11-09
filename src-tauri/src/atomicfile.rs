//! 설정·롤업 같은 영속 상태의 원자적 저장.
//!
//! `fs::write`는 도중에 죽으면 파일이 반쯤 쓰인 채 남는다. 롤업은 원본 JSONL이
//! 사라진 뒤의 유일한 히스토리라 한 번 깨지면 복구할 수 없으므로, 임시 파일에
//! 쓰고 rename으로 교체한다(같은 파일시스템 내 rename은 원자적).

use std::fs;
use std::io;
use std::path::Path;

/// 임시 파일에 쓴 뒤 rename으로 교체한다. 부모 디렉토리는 없으면 만든다.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 읽었지만 파싱에 실패한 파일을 `.corrupt`로 옮겨 보존한다.
///
/// 조용히 덮어쓰면 사용자가 잃은 줄도 모르고 히스토리가 날아간다. 남겨두면
/// 최소한 수동 복구 여지가 있다.
pub fn preserve_corrupt(path: &Path) {
    if !path.exists() {
        return;
    }
    let backup = path.with_extension("corrupt");
    let _ = fs::rename(path, backup);
}

/// 사용자 소유 파일을 원자적으로 바꾼다.
///
/// dotfiles 저장소에 두고 심링크로 거는 사용자가 많다. 링크 자리에 rename하면 링크가
/// 일반 파일로 바뀌어 저장소와 끊기므로 링크가 가리키는 실제 파일을 바꾼다. 권한도
/// 원래 파일 것을 그대로 옮긴다.
pub fn write_atomic_preserving(path: &Path, contents: &str) -> io::Result<()> {
    let real = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = real.parent() {
        fs::create_dir_all(dir)?;
    }
    let name = real
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = real.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, contents)?;
    if let Ok(meta) = fs::metadata(&real) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    match fs::rename(&tmp, &real) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 깨진 사용자 파일의 사본을 `.corrupt-<시각>`으로 남긴다. 원본은 옮기지 않는다 —
/// 에이전트가 그 파일을 설정으로 읽고 있어서 치우면 설정을 통째로 잃는다.
pub fn preserve_corrupt_copy(path: &Path) -> Option<std::path::PathBuf> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dst = path.with_file_name(format!("{name}.corrupt-{ts}"));
    fs::copy(path, &dst).ok()?;
    Some(dst)
}
