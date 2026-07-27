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
