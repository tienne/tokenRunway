#!/usr/bin/env bash
# `trw` CLI를 빌드해 Tauri externalBin이 찾는 이름으로 놓는다.
#
# externalBin은 `<이름>-<타깃 트리플>` 파일을 요구하고, 번들할 때 접미사를 떼서
# `Contents/MacOS/trw`로 넣는다. dev 실행에서도 이 파일이 있어야 해서
# beforeDevCommand와 beforeBuildCommand 양쪽에 걸어둔다.
#
# 릴리스 CI는 `--target universal-apple-darwin`으로 빌드하는데, 그때 요구하는 건
# 아키텍처별 파일이 아니라 `lipo`로 합친 `trw-universal-apple-darwin` 하나다.
# 그래서 설치된 타깃을 각각 빌드하고, 둘 다 있으면 universal도 만든다 —
# 로컬처럼 한쪽만 설치된 환경에서는 그 하나만 만들고 넘어간다.
#
# 인자 없으면 release, `--debug`면 debug 빌드를 쓴다 — dev마다 release를
# 다시 만들면 기동이 그만큼 늦어진다.
set -euo pipefail

cd "$(dirname "$0")/../src-tauri"

HOST_TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$HOST_TRIPLE" ]; then
  echo "타깃 트리플을 알 수 없습니다" >&2
  exit 1
fi

mkdir -p binaries

if [ "${1:-}" = "--debug" ]; then
  # dev는 지금 이 머신에서만 돌므로 host 하나면 된다.
  cargo build -p trw
  cp "target/debug/trw" "binaries/trw-${HOST_TRIPLE}"
  chmod +x "binaries/trw-${HOST_TRIPLE}"
  echo "binaries/trw-${HOST_TRIPLE} (debug) 준비됨"
  exit 0
fi

INSTALLED=$(rustup target list --installed 2>/dev/null || echo "$HOST_TRIPLE")
BUILT=()
for TRIPLE in aarch64-apple-darwin x86_64-apple-darwin; do
  if ! echo "$INSTALLED" | grep -qx "$TRIPLE"; then
    continue
  fi
  cargo build --release --target "$TRIPLE" -p trw
  cp "target/${TRIPLE}/release/trw" "binaries/trw-${TRIPLE}"
  chmod +x "binaries/trw-${TRIPLE}"
  echo "binaries/trw-${TRIPLE} (release) 준비됨"
  BUILT+=("target/${TRIPLE}/release/trw")
done

if [ "${#BUILT[@]}" -eq 0 ]; then
  echo "설치된 apple-darwin 타깃이 없습니다 (rustup target list --installed)" >&2
  exit 1
fi

if [ "${#BUILT[@]}" -ge 2 ]; then
  lipo -create -output "binaries/trw-universal-apple-darwin" "${BUILT[@]}"
  chmod +x "binaries/trw-universal-apple-darwin"
  echo "binaries/trw-universal-apple-darwin (lipo) 준비됨"
fi
