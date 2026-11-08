#!/usr/bin/env node
// src/i18n.ts 사전을 검사한다. 하나라도 걸리면 종료 코드 1.
//  1. ko와 en의 키 집합이 같다
//  2. 값에 {x} 자리표시자가 있는 키를 params 없이 t()/tr()로 부르지 않는다
//  3. Rust가 note로 내보내는 키(error.* 등)가 사전에 있다
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(import.meta.url), "..", "..");
const errors = [];

function walk(dir, exts) {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (name === "node_modules" || name === "target") return [];
    if (statSync(p).isDirectory()) return walk(p, exts);
    return exts.some((e) => name.endsWith(e)) ? [p] : [];
  });
}

// 사전은 `  ko: {` ~ `  },` 블록 안에 4칸 들여쓴 `key: "값",` 줄로 되어 있다.
// TS를 실행하지 않고 줄 단위로 읽는다 — 이 스크립트가 빌드 도구에 기대지 않게.
// 값이 길면 키 다음 줄로 넘어가므로 그 경우만 두 줄을 붙여 읽는다.
function parseDict(src) {
  const dicts = {};
  let lang = null;
  let pendingKey = null;
  for (const line of src.split("\n")) {
    if (pendingKey) {
      const value = line.match(/^\s+(["'`])(.*)\1,?\s*$/);
      if (value) dicts[lang].set(pendingKey, value[2]);
      pendingKey = null;
      continue;
    }
    const open = line.match(/^ {2}(\w+): \{\s*$/);
    if (open) {
      lang = open[1];
      dicts[lang] = new Map();
      continue;
    }
    if (lang && /^ {2}\},?\s*$/.test(line)) {
      lang = null;
      continue;
    }
    if (!lang) continue;
    const entry = line.match(/^ {4}(?:"([^"]+)"|([A-Za-z_$][\w$]*)):\s*(["'`])(.*)\3,?\s*$/);
    if (entry) {
      dicts[lang].set(entry[1] ?? entry[2], entry[4]);
      continue;
    }
    const keyOnly = line.match(/^ {4}(?:"([^"]+)"|([A-Za-z_$][\w$]*)):\s*$/);
    if (keyOnly) pendingKey = keyOnly[1] ?? keyOnly[2];
  }
  return dicts;
}

const i18nPath = join(root, "src/i18n.ts");
const dicts = parseDict(readFileSync(i18nPath, "utf8"));
const { ko, en } = dicts;
if (!ko || !en || ko.size === 0 || en.size === 0) {
  console.error("src/i18n.ts에서 ko/en 사전을 못 읽었어요 — 파일 구조가 바뀌었는지 확인하세요");
  process.exit(1);
}

// 1. 키 집합
for (const [a, b, an, bn] of [
  [ko, en, "ko", "en"],
  [en, ko, "en", "ko"],
]) {
  for (const k of a.keys()) {
    if (!b.has(k)) errors.push(`${an}에만 있는 키: ${k} (${bn}에 없음)`);
  }
}

// 2. 자리표시자가 있는 키를 params 없이 부르는 곳
const placeholderKeys = new Set(
  [...ko, ...en].filter(([, v]) => /\{\w+\}/.test(v)).map(([k]) => k),
);

// 키 인자를 문자열 리터럴로 줄이는 데 쓴다. 조건식이면 양쪽 리터럴을 다 보고,
// 템플릿 리터럴이면 접두가 같은 키 전부를 본다. 그 밖의 식(백엔드가 준 키 등)은
// 정적으로 알 수 없어 건너뛴다.
function candidateKeys(expr) {
  const tpl = expr.match(/^`([^`$]*)\$\{/);
  if (tpl) return [...ko.keys()].filter((k) => k.startsWith(tpl[1]));
  return [...expr.matchAll(/"([^"]+)"|'([^']+)'/g)].map((m) => m[1] ?? m[2]);
}

// 괄호 짝을 맞춰 인자 목록을 자른다. 정규식만으로는 중첩된 객체 리터럴을 못 다룬다.
function splitCall(src, start) {
  const args = [];
  let depth = 0;
  let cur = "";
  let quote = null;
  for (let i = start; i < src.length; i++) {
    const c = src[i];
    if (quote) {
      cur += c;
      if (c === "\\") {
        cur += src[++i];
      } else if (c === quote) {
        quote = null;
      }
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      quote = c;
      cur += c;
      continue;
    }
    if ("({[".includes(c)) depth++;
    if (")}]".includes(c)) {
      if (depth === 0) {
        args.push(cur.trim());
        return args;
      }
      depth--;
    }
    if (c === "," && depth === 0) {
      args.push(cur.trim());
      cur = "";
      continue;
    }
    cur += c;
  }
  return null;
}

for (const file of walk(join(root, "src"), [".ts", ".tsx"])) {
  if (file === i18nPath) continue;
  const src = readFileSync(file, "utf8");
  for (const m of src.matchAll(/\b(t|tr)\(/g)) {
    const args = splitCall(src, m.index + m[0].length);
    if (!args) continue;
    // t(lang, key, params) / tr(key, params)
    const [keyExpr, params] = m[1] === "t" ? [args[1], args[2]] : [args[0], args[1]];
    if (!keyExpr || (params && params !== "undefined")) continue;
    const line = src.slice(0, m.index).split("\n").length;
    for (const k of candidateKeys(keyExpr)) {
      if (placeholderKeys.has(k)) {
        errors.push(`${relative(root, file)}:${line} — "${k}"에 자리표시자가 있는데 params 없이 불러요`);
      }
    }
  }
}

// 3. Rust가 note로 내보내는 키
const notePrefix = /^"((?:error|note|insight|verdict)\.[A-Za-z0-9_]+)"$/;
const rustKeys = new Map();
for (const file of walk(join(root, "src-tauri/src"), [".rs"])) {
  const src = readFileSync(file, "utf8");
  for (const m of src.matchAll(/"[^"\n]*"/g)) {
    const key = m[0].match(notePrefix)?.[1];
    if (key && !rustKeys.has(key)) rustKeys.set(key, relative(root, file));
  }
}
for (const [k, file] of rustKeys) {
  for (const [lang, d] of [
    ["ko", ko],
    ["en", en],
  ]) {
    if (!d.has(k)) errors.push(`${file}가 내보내는 "${k}"가 src/i18n.ts ${lang}에 없어요`);
  }
}

if (errors.length) {
  console.error(errors.map((e) => `✗ ${e}`).join("\n"));
  console.error(`\ni18n 검사 실패 ${errors.length}건`);
  process.exit(1);
}
console.log(
  `i18n 검사 통과 — ko ${ko.size}키 · en ${en.size}키 · Rust note 키 ${rustKeys.size}개`,
);
