#!/usr/bin/env bash
# `trw` CLI를 빌드해 Tauri externalBin이 찾는 이름으로 놓는다.
#
# externalBin은 `<이름>-<타깃 트리플>` 파일을 요구하고, 번들할 때 접미사를 떼서
# `Contents/MacOS/trw`로 넣는다. dev 실행에서도 이 파일이 있어야 해서
# beforeDevCommand와 beforeBuildCommand 양쪽에 걸어둔다.
#
# 인자 없으면 release, `--debug`면 debug 빌드를 쓴다 — dev마다 release를
# 다시 만들면 기동이 그만큼 늦어진다.
set -euo pipefail

cd "$(dirname "$0")/../src-tauri"

PROFILE_DIR="release"
BUILD_FLAG="--release"
if [ "${1:-}" = "--debug" ]; then
  PROFILE_DIR="debug"
  BUILD_FLAG=""
fi

TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$TRIPLE" ]; then
  echo "타깃 트리플을 알 수 없습니다" >&2
  exit 1
fi

# shellcheck disable=SC2086
cargo build $BUILD_FLAG -p trw
mkdir -p binaries
cp "target/${PROFILE_DIR}/trw" "binaries/trw-${TRIPLE}"
chmod +x "binaries/trw-${TRIPLE}"
echo "binaries/trw-${TRIPLE} (${PROFILE_DIR}) 준비됨"
