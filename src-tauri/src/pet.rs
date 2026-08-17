//! 커스텀 pet 스킨 번들 가져오기 — manifest 검증 + 파일 복사.
//! 저장 위치: `~/.token-runway/pet/custom/<id>/` (settings.json과 같은 루트).
//! 번들마다 고유 id로 자기 폴더를 가져서 여러 개를 동시에 저장해두고 전환할 수 있다.
//!
//! 두 가지 번들 포맷을 지원한다:
//! - 우리 자체 포맷: `manifest.json`(states: idle/good/warn/danger → 파일명) + 정지 이미지 4장.
//! - Codex pet 포맷(`pet.json` + `spritesheet.webp`): Codex CLI TUI/Orca가 쓰는 스프라이트시트
//!   번들. 레이아웃(프레임 크기·행별 애니메이션)을 pet.json이 선언하지 않으면 Codex 기본
//!   레이아웃(192×208, 8열, 9개 애니메이션)을 그대로 적용한다 — Orca의
//!   `applyCodexPetDefaults`와 동일 규칙.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED_EXT: [&str; 6] = ["png", "jpg", "jpeg", "gif", "svg", "webp"];
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024; // 파일당 5MB
const MAX_TOTAL_BYTES: u64 = 15 * 1024 * 1024; // 4개 합계 15MB
const MAX_MANIFEST_BYTES: u64 = 64 * 1024; // pet.json/manifest.json은 원래 작다
const MAX_SHEET_BYTES: u64 = 20 * 1024 * 1024; // 스프라이트시트 1장

// Codex pet 기본 레이아웃 (codex-rs/tui/src/pets/model.rs, Orca의 codex-pet-sprite-defaults.ts와 동일).
const CODEX_FRAME: (u32, u32) = (192, 208);
const CODEX_DEFAULT_FPS: f64 = 8.0;
const CODEX_DEFAULT_ANIMATION: &str = "idle";

fn app_state_durations(frames: u32, frame_ms: f64, final_ms: f64) -> Vec<f64> {
    (0..frames)
        .map(|i| if i == frames - 1 { final_ms } else { frame_ms })
        .collect()
}

fn codex_animations() -> HashMap<String, SpriteAnimation> {
    let mut m = HashMap::new();
    m.insert(
        "idle".to_string(),
        SpriteAnimation {
            row: 0,
            frames: 6,
            frame_durations_ms: Some(vec![1680.0, 660.0, 660.0, 840.0, 840.0, 1920.0]),
        },
    );
    m.insert(
        "running-right".to_string(),
        SpriteAnimation {
            row: 1,
            frames: 8,
            frame_durations_ms: Some(app_state_durations(8, 120.0, 220.0)),
        },
    );
    m.insert(
        "running-left".to_string(),
        SpriteAnimation {
            row: 2,
            frames: 8,
            frame_durations_ms: Some(app_state_durations(8, 120.0, 220.0)),
        },
    );
    m.insert(
        "waving".to_string(),
        SpriteAnimation {
            row: 3,
            frames: 4,
            frame_durations_ms: Some(app_state_durations(4, 140.0, 280.0)),
        },
    );
    m.insert(
        "jumping".to_string(),
        SpriteAnimation {
            row: 4,
            frames: 5,
            frame_durations_ms: Some(app_state_durations(5, 140.0, 280.0)),
        },
    );
    m.insert(
        "failed".to_string(),
        SpriteAnimation {
            row: 5,
            frames: 8,
            frame_durations_ms: Some(app_state_durations(8, 140.0, 240.0)),
        },
    );
    m.insert(
        "waiting".to_string(),
        SpriteAnimation {
            row: 6,
            frames: 6,
            frame_durations_ms: Some(app_state_durations(6, 150.0, 260.0)),
        },
    );
    m.insert(
        "running".to_string(),
        SpriteAnimation {
            row: 7,
            frames: 6,
            frame_durations_ms: Some(app_state_durations(6, 120.0, 220.0)),
        },
    );
    m.insert(
        "review".to_string(),
        SpriteAnimation {
            row: 8,
            frames: 6,
            frame_durations_ms: Some(app_state_durations(6, 150.0, 280.0)),
        },
    );
    m
}

fn codex_animations_at_uniform_fps(fps: f64) -> HashMap<String, SpriteAnimation> {
    let frame_ms = 1000.0 / fps;
    codex_animations()
        .into_iter()
        .map(|(name, a)| {
            (
                name,
                SpriteAnimation {
                    row: a.row,
                    frames: a.frames,
                    frame_durations_ms: Some(vec![frame_ms; a.frames as usize]),
                },
            )
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    name: String,
    states: ManifestStates,
}

#[derive(Debug, Deserialize)]
struct ManifestStates {
    idle: String,
    good: String,
    warn: String,
    danger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetBundleStates {
    pub idle: String,
    pub good: String,
    pub warn: String,
    pub danger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpriteAnimation {
    pub row: u32,
    pub frames: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_durations_ms: Option<Vec<f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetSprite {
    pub sheet: String,
    pub frame_width: u32,
    pub frame_height: u32,
    pub columns: u32,
    pub rows: u32,
    pub fps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_animation: Option<String>,
    pub animations: HashMap<String, SpriteAnimation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetBundle {
    pub id: String,
    pub name: String,
    pub dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub states: Option<PetBundleStates>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sprite: Option<PetSprite>,
}

fn custom_root() -> Result<PathBuf, String> {
    crate::settings::app_dir()
        .map(|d| d.join("pet").join("custom"))
        .ok_or_else(|| "pet.err.noConfigDir".to_string())
}

/// 시간 기반 고유 id — 번들마다 자기 폴더(`custom/<id>/`)를 가지므로 충돌 없이 여러 개를 보관한다.
fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pet-{nanos:x}")
}

fn clean_name(raw: &str, id: Option<&str>, fallback: &str) -> String {
    let pick = if raw.trim().is_empty() {
        id.unwrap_or(fallback)
    } else {
        raw
    };
    let cleaned: String = pick.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.chars().take(60).collect()
    }
}

/// 폴더 선택 다이얼로그가 돌려준 경로를 검증·복사한다. blocking I/O — 호출부에서 spawn_blocking.
pub fn import_bundle(source: PathBuf) -> Result<PetBundle, String> {
    let src = source
        .canonicalize()
        .map_err(|_| "pet.err.notFound".to_string())?;
    if !src.is_dir() {
        return Err("pet.err.notFolder".into());
    }

    if src.join("manifest.json").is_file() {
        import_simple_bundle(&src)
    } else if src.join("pet.json").is_file() {
        import_codex_bundle(&src)
    } else {
        Err("pet.err.noManifest".into())
    }
}

/// 우리 자체 포맷: manifest.json의 states(idle/good/warn/danger)가 가리키는 정지 이미지 4장.
fn import_simple_bundle(src: &Path) -> Result<PetBundle, String> {
    let raw = fs::read_to_string(src.join("manifest.json"))
        .map_err(|_| "pet.err.noManifest".to_string())?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|_| "pet.err.badManifest".to_string())?;

    let files = [
        ("idle", &manifest.states.idle),
        ("good", &manifest.states.good),
        ("warn", &manifest.states.warn),
        ("danger", &manifest.states.danger),
    ];

    let mut resolved = Vec::new(); // (state, canonical src path, ext)
    let mut total: u64 = 0;

    for (state, filename) in files {
        // 경로 조작 차단: 같은 폴더 안의 단순 파일명만 허용 (구분자·상위참조 금지).
        if filename.is_empty() || filename.contains(['/', '\\']) || filename == ".." {
            return Err(format!("pet.err.badFileName:{state}"));
        }
        let canon = src
            .join(filename)
            .canonicalize()
            .map_err(|_| format!("pet.err.fileNotFound:{state}"))?;
        // 심볼릭 링크로 번들 폴더 밖을 가리키는 경우 차단.
        if !canon.starts_with(src) {
            return Err(format!("pet.err.outsideBundle:{state}"));
        }
        let ext = canon
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !ALLOWED_EXT.contains(&ext.as_str()) {
            return Err(format!("pet.err.badExt:{state}"));
        }
        let len = fs::metadata(&canon).map(|m| m.len()).unwrap_or(0);
        if len == 0 || len > MAX_FILE_BYTES {
            return Err(format!("pet.err.tooBig:{state}"));
        }
        total += len;
        resolved.push((state, canon, ext));
    }
    if total > MAX_TOTAL_BYTES {
        return Err("pet.err.bundleTooBig".into());
    }

    let id = generate_id();
    let dest = fresh_bundle_dir(&id)?;

    let mut out = HashMap::new();
    for (state, canon, ext) in resolved {
        let dest_file = dest.join(format!("{state}.{ext}"));
        fs::copy(&canon, &dest_file).map_err(|e| e.to_string())?;
        out.insert(state, dest_file.to_string_lossy().to_string());
    }

    Ok(PetBundle {
        id,
        name: clean_name(&manifest.name, None, "Custom"),
        dir: dest.to_string_lossy().to_string(),
        states: Some(PetBundleStates {
            idle: out.remove("idle").unwrap(),
            good: out.remove("good").unwrap(),
            warn: out.remove("warn").unwrap(),
            danger: out.remove("danger").unwrap(),
        }),
        sprite: None,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexManifest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    spritesheet_path: Option<String>,
    #[serde(default)]
    frame: Option<CodexFrame>,
    #[serde(default)]
    fps: Option<f64>,
    #[serde(default)]
    default_animation: Option<String>,
    #[serde(default)]
    animations: Option<HashMap<String, CodexAnimationIn>>,
}

#[derive(Debug, Deserialize)]
struct CodexFrame {
    width: u32,
    height: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexAnimationIn {
    row: u32,
    frames: u32,
    #[serde(default)]
    frame_durations_ms: Option<Vec<f64>>,
}

/// Codex pet 포맷: pet.json + 스프라이트시트. 프레임 레이아웃을 선언하지 않으면
/// Codex 기본 레이아웃(192×208, 8열, 9개 애니메이션)을 적용한다.
fn import_codex_bundle(src: &Path) -> Result<PetBundle, String> {
    let manifest_path = src.join("pet.json");
    let meta = fs::metadata(&manifest_path).map_err(|_| "pet.err.noManifest".to_string())?;
    if meta.len() > MAX_MANIFEST_BYTES {
        return Err("pet.err.badPetJson".into());
    }
    let raw = fs::read_to_string(&manifest_path).map_err(|_| "pet.err.badPetJson".to_string())?;
    let manifest: CodexManifest =
        serde_json::from_str(&raw).map_err(|_| "pet.err.badPetJson".to_string())?;

    let sheet_rel = manifest
        .spritesheet_path
        .clone()
        .unwrap_or_else(|| "spritesheet.webp".to_string());
    if sheet_rel.is_empty() || sheet_rel.contains('\0') || sheet_rel.contains("..") {
        return Err("pet.err.badFileName:sheet".into());
    }
    let sheet_canon = src
        .join(&sheet_rel)
        .canonicalize()
        .map_err(|_| "pet.err.sheetNotFound".to_string())?;
    if !sheet_canon.starts_with(src) {
        return Err("pet.err.outsideBundle:sheet".into());
    }
    let ext = sheet_canon
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "png" && ext != "webp" {
        return Err("pet.err.badSheetExt".into());
    }
    let sheet_len = fs::metadata(&sheet_canon).map(|m| m.len()).unwrap_or(0);
    if sheet_len == 0 || sheet_len > MAX_SHEET_BYTES {
        return Err("pet.err.tooBig:sheet".into());
    }

    // Orca의 applyCodexPetDefaults와 동일 규칙: spritesheetPath가 미지정이거나
    // "spritesheet.webp"로 끝나고, frame/animations를 둘 다 선언 안 했으면 Codex 기본 레이아웃.
    let is_codex_sheet_name = manifest
        .spritesheet_path
        .as_deref()
        .map(|p| p.to_lowercase().ends_with("spritesheet.webp"))
        .unwrap_or(true);
    let should_apply_codex_defaults =
        is_codex_sheet_name && manifest.frame.is_none() && manifest.animations.is_none();

    let (frame, fps, animations): (Option<(u32, u32)>, f64, HashMap<String, SpriteAnimation>) =
        if should_apply_codex_defaults {
            let fps = manifest.fps.unwrap_or(CODEX_DEFAULT_FPS);
            let anims = if manifest.fps.is_none() {
                codex_animations()
            } else {
                codex_animations_at_uniform_fps(fps)
            };
            (Some(CODEX_FRAME), fps, anims)
        } else {
            let fps = manifest.fps.unwrap_or(CODEX_DEFAULT_FPS);
            let anims = manifest
                .animations
                .map(|m| {
                    m.into_iter()
                        .map(|(k, v)| {
                            (
                                k,
                                SpriteAnimation {
                                    row: v.row,
                                    frames: v.frames,
                                    frame_durations_ms: v.frame_durations_ms,
                                },
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                manifest.frame.map(|f| (f.width, f.height)),
                fps,
                anims,
            )
        };

    let default_animation = if should_apply_codex_defaults {
        Some(
            manifest
                .default_animation
                .clone()
                .unwrap_or_else(|| CODEX_DEFAULT_ANIMATION.to_string()),
        )
    } else {
        manifest.default_animation.clone()
    };

    let id = generate_id();
    let dest = fresh_bundle_dir(&id)?;
    let dest_sheet = dest.join(format!("sheet.{ext}"));
    fs::copy(&sheet_canon, &dest_sheet).map_err(|e| e.to_string())?;
    let dest_sheet_str = dest_sheet.to_string_lossy().to_string();

    let name = clean_name(
        manifest.display_name.as_deref().unwrap_or(""),
        manifest.id.as_deref(),
        "Custom",
    );

    let sprite = match frame {
        None => None,
        Some((fw, fh)) => {
            let (sheet_w, sheet_h) = image::image_dimensions(&dest_sheet)
                .map_err(|_| "pet.err.badSheet".to_string())?;
            if fw == 0 || fh == 0 || sheet_w % fw != 0 || sheet_h % fh != 0 {
                return Err("pet.err.badFrameSize".into());
            }
            let columns = sheet_w / fw;
            let rows = sheet_h / fh;
            for (anim_name, a) in &animations {
                if a.row >= rows {
                    return Err(format!("pet.err.badAnimationRow:{anim_name}"));
                }
                if a.frames == 0 || a.frames > columns {
                    return Err(format!("pet.err.badAnimationFrames:{anim_name}"));
                }
                if let Some(durs) = &a.frame_durations_ms {
                    if durs.len() as u32 != a.frames {
                        return Err(format!("pet.err.badAnimationDurations:{anim_name}"));
                    }
                }
            }
            if animations.is_empty() {
                return Err("pet.err.noAnimations".into());
            }
            Some(PetSprite {
                sheet: dest_sheet_str.clone(),
                frame_width: fw,
                frame_height: fh,
                columns,
                rows,
                fps,
                default_animation,
                animations,
            })
        }
    };

    // frame이 없으면(=애니메이션 불가) 같은 원본 이미지를 4개 상태에 그대로 매핑해
    // 정지 이미지 번들처럼 동작시킨다 — 프론트 코드 추가 없이 자연스러운 폴백.
    let states = if sprite.is_none() {
        Some(PetBundleStates {
            idle: dest_sheet_str.clone(),
            good: dest_sheet_str.clone(),
            warn: dest_sheet_str.clone(),
            danger: dest_sheet_str.clone(),
        })
    } else {
        None
    };

    Ok(PetBundle {
        id,
        name,
        dir: dest.to_string_lossy().to_string(),
        states,
        sprite,
    })
}

/// 이 id 전용 폴더를 비우고(있었다면) 새로 만든다 — 다른 번들에는 손대지 않는다.
fn fresh_bundle_dir(id: &str) -> Result<PathBuf, String> {
    let dest = custom_root()?.join(id);
    let _ = fs::remove_dir_all(&dest);
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    Ok(dest)
}

/// 저장해둔 번들 하나를 삭제한다. 이미 없으면 조용히 성공.
pub fn delete_bundle(id: &str) -> Result<(), String> {
    if let Ok(root) = custom_root() {
        let _ = fs::remove_dir_all(root.join(id));
    }
    Ok(())
}
