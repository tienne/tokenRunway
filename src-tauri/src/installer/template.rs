//! 사용자 소유 JSON 설정 파일(Codex `hooks.json`, Claude `settings.json`)에 우리 항목만
//! 넣고 빼는 공용 틀.
//!
//! serde_json으로 읽어 다시 쓰면 키 순서와 들여쓰기가 바뀌고 함께 사는 오르카 항목까지
//! 다시 직렬화된다. 그래서 serde_json은 검증에만 쓰고, 수정은 원문을 구간 단위로 잘라
//! 붙인다. 넣은 텍스트를 그대로 도려내므로 설치 → 제거가 바이트 단위로 원래 파일이 된다.
//!
//! 쓰기 전에 읽은 시점의 mtime과 해시를 다시 확인한다. 그 사이 에이전트나 오르카가
//! 파일을 고쳤으면 쓰지 않고 실패한다 — 덮어쓰면 그 변경이 조용히 사라진다.

use crate::atomicfile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 우리 항목 표시. 명령 앞 주석(`: trw-hook-form=1; ...`)으로 넣고 이게 있는 항목만 건드린다.
pub const MARKER: &str = "trw-hook-form=1";

/// 순서를 지키는 JSON 값. serde_json `Map`은 키를 정렬해서 오르카 항목과 모양이 달라진다.
pub enum J {
    Obj(Vec<(&'static str, J)>),
    Arr(Vec<J>),
    Str(String),
    Num(i64),
}

impl J {
    /// `indent`는 이 값이 놓이는 줄의 들여쓰기, `unit`은 한 단계 들여쓰기.
    pub fn render(&self, indent: &str, unit: &str) -> String {
        let inner = format!("{indent}{unit}");
        match self {
            J::Str(s) => quote(s),
            J::Num(n) => n.to_string(),
            J::Arr(v) if v.is_empty() => "[]".into(),
            J::Obj(v) if v.is_empty() => "{}".into(),
            J::Arr(v) => {
                let items: Vec<String> = v
                    .iter()
                    .map(|x| format!("{inner}{}", x.render(&inner, unit)))
                    .collect();
                format!("[\n{}\n{indent}]", items.join(",\n"))
            }
            J::Obj(v) => {
                let items: Vec<String> = v
                    .iter()
                    .map(|(k, x)| format!("{inner}{}: {}", quote(k), x.render(&inner, unit)))
                    .collect();
                format!("{{\n{}\n{indent}}}", items.join(",\n"))
            }
        }
    }
}

pub fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 셸 작은따옴표 인용. 경로에 공백이나 따옴표가 있어도 인자 하나로 넘어가게 한다.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// --- 구간 파서 ---

/// 원문 위치(`start..end`, 바이트)를 든 JSON 노드.
#[derive(Debug)]
pub struct Node {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

#[derive(Debug)]
pub enum Kind {
    Object(Vec<Member>),
    Array(Vec<Node>),
    Scalar,
}

#[derive(Debug)]
pub struct Member {
    pub key: String,
    pub key_start: usize,
    pub value: Node,
}

impl Node {
    pub fn member(&self, key: &str) -> Option<(usize, &Member)> {
        match &self.kind {
            Kind::Object(m) => m.iter().enumerate().find(|(_, m)| m.key == key),
            _ => None,
        }
    }

    pub fn is_empty_container(&self) -> bool {
        match &self.kind {
            Kind::Object(m) => m.is_empty(),
            Kind::Array(v) => v.is_empty(),
            Kind::Scalar => false,
        }
    }

    /// 자식 항목의 구간. 멤버는 키부터 값 끝까지다.
    fn child_spans(&self) -> Vec<(usize, usize)> {
        match &self.kind {
            Kind::Object(m) => m.iter().map(|m| (m.key_start, m.value.end)).collect(),
            Kind::Array(v) => v.iter().map(|n| (n.start, n.end)).collect(),
            Kind::Scalar => Vec::new(),
        }
    }
}

pub fn find<'a>(root: &'a Node, path: &[String]) -> Option<&'a Node> {
    let mut node = root;
    for key in path {
        node = &node.member(key)?.1.value;
    }
    Some(node)
}

/// 검증을 통과한 JSON만 받는다. 그래서 아래 파서는 문법 오류를 세세히 가리지 않는다.
pub fn parse(text: &str) -> Result<Node, String> {
    serde_json::from_str::<serde_json::Value>(text).map_err(|e| e.to_string())?;
    let mut p = Parser {
        s: text,
        b: text.as_bytes(),
        i: 0,
    };
    p.value()
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Node, String> {
        self.ws();
        let start = self.i;
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut members = Vec::new();
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b'}') => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        Some(b'"') => {
                            let key_start = self.i;
                            let key = self.string()?;
                            self.ws();
                            if self.b.get(self.i) != Some(&b':') {
                                return Err("':' 없음".into());
                            }
                            self.i += 1;
                            let value = self.value()?;
                            members.push(Member {
                                key,
                                key_start,
                                value,
                            });
                        }
                        _ => return Err("객체 파싱 실패".into()),
                    }
                }
                Ok(Node {
                    start,
                    end: self.i,
                    kind: Kind::Object(members),
                })
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        Some(_) => items.push(self.value()?),
                        None => return Err("배열 파싱 실패".into()),
                    }
                }
                Ok(Node {
                    start,
                    end: self.i,
                    kind: Kind::Array(items),
                })
            }
            Some(b'"') => {
                self.string()?;
                Ok(Node {
                    start,
                    end: self.i,
                    kind: Kind::Scalar,
                })
            }
            Some(_) => {
                while let Some(c) = self.b.get(self.i) {
                    if matches!(c, b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
                Ok(Node {
                    start,
                    end: self.i,
                    kind: Kind::Scalar,
                })
            }
            None => Err("값이 없음".into()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.i;
        self.i += 1;
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            match c {
                b'\\' => self.i += 1,
                b'"' => {
                    return serde_json::from_str(&self.s[start..self.i]).map_err(|e| e.to_string())
                }
                _ => {}
            }
        }
        Err("문자열이 끝나지 않음".into())
    }
}

// --- 구간 편집 ---

fn splice(text: &str, from: usize, to: usize, with: &str) -> String {
    format!("{}{}{}", &text[..from], with, &text[to..])
}

/// `pos`가 있는 줄의 앞 공백.
pub fn line_indent(text: &str, pos: usize) -> String {
    let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    text[line_start..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// 파일의 한 단계 들여쓰기. 처음 들여쓴 줄을 따른다.
pub fn detect_unit(text: &str) -> String {
    for line in text.lines() {
        let ws: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if !ws.is_empty() && line.len() > ws.len() {
            return if ws.starts_with('\t') { "\t".into() } else { ws };
        }
    }
    "  ".into()
}

/// 컨테이너 끝에 자식 하나를 붙인다. `child`는 들여쓰기를 받아 자식 텍스트를 만든다.
///
/// 비어 있지 않으면 마지막 자식 앞 공백을 그대로 복사해 `,` 뒤에 붙인다 — 제거할 때
/// 바로 앞 자식 끝부터 우리 끝까지를 지우면 정확히 원래대로 돌아간다. 비어 있었으면
/// 원래 안쪽 공백을 돌려줘서 호출자가 기록해 두게 한다.
pub fn append_child(
    text: &str,
    container: &Node,
    unit: &str,
    child: impl Fn(&str) -> String,
) -> (String, Option<String>) {
    let spans = container.child_spans();
    if let Some(&(last_start, last_end)) = spans.last() {
        let ws_start = text[..last_start]
            .trim_end_matches([' ', '\t', '\n', '\r'])
            .len();
        let ws = &text[ws_start..last_start];
        let indent = line_indent(text, last_start);
        let ins = format!(",{ws}{}", child(&indent));
        (splice(text, last_end, last_end, &ins), None)
    } else {
        let open = container.start;
        let close = container.end - 1;
        let inner = text[open + 1..close].to_string();
        let outer = line_indent(text, open);
        let child_indent = format!("{outer}{unit}");
        let ins = format!("\n{child_indent}{}\n{outer}", child(&child_indent));
        (splice(text, open + 1, close, &ins), Some(inner))
    }
}

/// 자식 하나를 지운다. 마지막 하나였으면 안쪽을 `restore_inner`로 되돌린다.
pub fn remove_child(text: &str, container: &Node, idx: usize, restore_inner: &str) -> String {
    let spans = container.child_spans();
    if spans.len() == 1 {
        splice(text, container.start + 1, container.end - 1, restore_inner)
    } else if idx > 0 {
        splice(text, spans[idx - 1].1, spans[idx].1, "")
    } else {
        splice(text, spans[0].0, spans[1].0, "")
    }
}

pub fn replace_node(text: &str, node: &Node, with: &str) -> String {
    splice(text, node.start, node.end, with)
}

// --- 설치 기록 ---

/// 파일 하나에 우리가 만든 구조. 제거할 때 이걸 보고 우리가 만든 빈 껍데기까지 걷어낸다.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// 파일이 없어서 우리가 만들었는지. 그러면 다 걷어낸 뒤 파일도 지운다.
    #[serde(default)]
    pub created_file: bool,
    /// 우리가 새로 만든 멤버 경로들.
    #[serde(default)]
    pub created: Vec<Vec<String>>,
    /// 비어 있던 컨테이너에 넣었을 때 원래 안쪽 공백. 키는 경로를 `/`로 이은 것.
    #[serde(default)]
    pub empty_inner: BTreeMap<String, String>,
}

impl Record {
    pub fn note_created(&mut self, path: Vec<String>) {
        if !self.created.contains(&path) {
            self.created.push(path);
        }
    }

    pub fn note_inner(&mut self, container: &[String], inner: Option<String>) {
        if let Some(inner) = inner {
            self.empty_inner
                .entry(container.join("/"))
                .or_insert(inner);
        }
    }

    pub fn inner(&self, container: &[String]) -> &str {
        self.empty_inner
            .get(&container.join("/"))
            .map(String::as_str)
            .unwrap_or("")
    }
}

fn manifest_path(app_dir: &Path) -> PathBuf {
    app_dir.join("hook-installs.json")
}

fn load_manifest(app_dir: &Path) -> BTreeMap<String, Record> {
    let path = manifest_path(app_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => {
            atomicfile::preserve_corrupt(&path);
            BTreeMap::new()
        }
    }
}

pub fn record_for(app_dir: &Path, file: &Path) -> Option<Record> {
    load_manifest(app_dir).remove(&file.to_string_lossy().to_string())
}

fn save_record(app_dir: &Path, file: &Path, record: Option<&Record>) -> Result<(), String> {
    let mut m = load_manifest(app_dir);
    let key = file.to_string_lossy().to_string();
    match record {
        Some(r) => {
            m.insert(key, r.clone());
        }
        None => {
            m.remove(&key);
        }
    }
    let raw = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
    atomicfile::write_atomic(&manifest_path(app_dir), &raw).map_err(|e| e.to_string())
}

// --- 읽기 / 계획 / 적용 ---

/// 읽은 시점의 파일 상태. 파일이 없었으면 둘 다 None.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamp {
    mtime: Option<SystemTime>,
    hash: Option<String>,
}

pub struct Loaded {
    pub text: Option<String>,
    pub stamp: Stamp,
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn stamp_of(path: &Path) -> Result<(Option<Vec<u8>>, Stamp), String> {
    match fs::read(path) {
        Ok(bytes) => {
            let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
            let hash = Some(sha256(&bytes));
            Ok((Some(bytes), Stamp { mtime, hash }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((
            None,
            Stamp {
                mtime: None,
                hash: None,
            },
        )),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// 사용자 파일을 읽고 JSON인지 확인한다. 깨져 있으면 사본을 남기고 멈춘다 —
/// 깨진 파일 위에 우리 항목을 얹으면 사용자가 고칠 단서까지 덮어쓰게 된다.
/// 원본은 그 자리에 둔다. 옮기면 에이전트가 설정을 통째로 잃는다.
pub fn load(path: &Path) -> Result<Loaded, String> {
    let (bytes, stamp) = stamp_of(path)?;
    let Some(bytes) = bytes else {
        return Ok(Loaded { text: None, stamp });
    };
    let text = String::from_utf8(bytes).map_err(|_| format!("{}: UTF-8이 아닙니다", path.display()))?;
    if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
        let kept = atomicfile::preserve_corrupt_copy(path);
        let kept = kept.map(|p| p.display().to_string()).unwrap_or_default();
        return Err(format!(
            "{}: JSON이 깨져 있어 건드리지 않았습니다 ({e}). 사본: {kept}",
            path.display()
        ));
    }
    Ok(Loaded {
        text: Some(text),
        stamp,
    })
}

/// 파일 하나를 어떻게 바꿀지. `after`가 None이면 파일을 지운다.
pub struct Plan {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    pub record: Option<Record>,
    pub stamp: Stamp,
}

impl Plan {
    pub fn changes(&self) -> bool {
        self.before != self.after
    }

    pub fn diff(&self) -> String {
        diff(
            self.before.as_deref().unwrap_or(""),
            self.after.as_deref().unwrap_or(""),
        )
    }

    /// 미리보기와 적용 사이에 내용이 같은지 가르는 지문.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.path.to_string_lossy().as_bytes());
        h.update([0]);
        h.update(self.before.as_deref().unwrap_or("\u{0}none").as_bytes());
        h.update([0]);
        h.update(self.after.as_deref().unwrap_or("\u{0}none").as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// 계획대로 쓴다. 읽은 뒤 파일이 바뀌었으면 쓰지 않는다. 쓰기 전에 `.bak-<시각>`을 남긴다.
pub fn apply(plan: &Plan, app_dir: &Path) -> Result<(), String> {
    if plan.changes() {
        let (_, now) = stamp_of(&plan.path)?;
        if now != plan.stamp {
            return Err(format!(
                "{}: 읽은 뒤 파일이 바뀌어 쓰지 않았습니다. 다시 시도해 주세요",
                plan.path.display()
            ));
        }
        if plan.before.is_some() {
            backup(&plan.path)?;
        }
        match &plan.after {
            Some(text) => atomicfile::write_atomic_preserving(&plan.path, text)
                .map_err(|e| format!("{}: {e}", plan.path.display()))?,
            None => fs::remove_file(&plan.path)
                .map_err(|e| format!("{}: {e}", plan.path.display()))?,
        }
    }
    save_record(app_dir, &plan.path, plan.record.as_ref())
}

/// 같은 초에 두 번 쓰면(설치 직후 제거) 첫 백업이 덮여 원래 파일을 잃는다. 번호를 붙여 피한다.
fn backup(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut dst = path.with_file_name(format!("{name}.bak-{ts}"));
    let mut n = 1;
    while dst.exists() {
        dst = path.with_file_name(format!("{name}.bak-{ts}-{n}"));
        n += 1;
    }
    fs::copy(path, &dst).map_err(|e| format!("백업 실패 {}: {e}", dst.display()))?;
    Ok(dst)
}

/// 줄 단위 diff. `+`/`-`/` ` 접두와 앞뒤 2줄 문맥만 남긴다.
pub fn diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    // 설정 파일은 길어야 수백 줄이다. 그보다 크면 표를 만들지 않고 통째로 보인다.
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        let mut out: Vec<String> = a.iter().map(|l| format!("- {l}")).collect();
        out.extend(b.iter().map(|l| format!("+ {l}")));
        return out.join("\n");
    }
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
            ops.push(('+', b[j]));
            j += 1;
        } else {
            ops.push(('-', a[i]));
            i += 1;
        }
    }
    const CONTEXT: usize = 2;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (c, _))| *c != ' ')
        .map(|(k, _)| k)
        .collect();
    let mut out = Vec::new();
    let mut last_shown: Option<usize> = None;
    for (k, (c, line)) in ops.iter().enumerate() {
        let near = changed
            .iter()
            .any(|&x| k + CONTEXT >= x && k <= x + CONTEXT);
        if !near {
            continue;
        }
        if last_shown.is_some_and(|l| k > l + 1) {
            out.push("…".to_string());
        }
        out.push(format!("{c} {line}"));
        last_shown = Some(k);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_remove_round_trips() {
        let cases = [
            "{\n  \"a\": [\n    1,\n    2\n  ]\n}\n",
            "{\"a\":[1,2]}",
            "{\n  \"a\": []\n}",
            "{\n  \"a\": [ ]\n}",
        ];
        for original in cases {
            let root = parse(original).unwrap();
            let arr = find(&root, &["a".into()]).unwrap();
            let (added, inner) = append_child(original, arr, "  ", |_| "3".to_string());
            assert!(added.contains('3'));
            let root = parse(&added).unwrap();
            let arr = find(&root, &["a".into()]).unwrap();
            let Kind::Array(items) = &arr.kind else { panic!() };
            let back = remove_child(&added, arr, items.len() - 1, inner.as_deref().unwrap_or(""));
            assert_eq!(back, original, "원문: {original:?}");
        }
    }

    #[test]
    fn appended_member_follows_existing_indent() {
        let original = "{\n\t\"a\": 1\n}";
        let root = parse(original).unwrap();
        let unit = detect_unit(original);
        let (added, _) = append_child(original, &root, &unit, |ind| {
            format!("\"b\": {}", J::Obj(vec![("c", J::Num(2))]).render(ind, &unit))
        });
        assert_eq!(added, "{\n\t\"a\": 1,\n\t\"b\": {\n\t\t\"c\": 2\n\t}\n}");
    }

    #[test]
    fn diff_marks_only_changed_lines_with_context() {
        let before = "a\nb\nc\nd\ne\nf\ng";
        let after = "a\nb\nc\nX\nd\ne\nf\ng";
        assert_eq!(diff(before, after), "  b\n  c\n+ X\n  d\n  e");
    }

    #[test]
    fn keys_with_escapes_are_decoded() {
        let root = parse(r#"{"a\"b": 1, "c": {"d": [true, null, "x,]"]}}"#).unwrap();
        assert!(root.member("a\"b").is_some());
        let d = find(&root, &["c".into(), "d".into()]).unwrap();
        let Kind::Array(items) = &d.kind else { panic!() };
        assert_eq!(items.len(), 3);
    }
}
