#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-dev}"
OUT_DIR="${2:-dist}"

BIN_PATH="target/release/softmgr"

if [[ ! -x "$BIN_PATH" ]]; then
  echo "未找到可执行文件：$BIN_PATH" >&2
  echo "请先执行：cargo build --release" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

ARCH="$(uname -m)"
VERSION_SAFE="${VERSION//\//_}"
VERSION_SAFE="${VERSION_SAFE// /_}"
PKG_NAME="softmgr-${VERSION_SAFE}-linux-${ARCH}"
STAGE_DIR="${OUT_DIR}/${PKG_NAME}"

if [[ -z "$STAGE_DIR" || "$STAGE_DIR" == "/" ]]; then
  echo "异常输出目录，拒绝清理：STAGE_DIR='$STAGE_DIR'" >&2
  exit 1
fi
if [[ "$STAGE_DIR" != "${OUT_DIR}/"* ]]; then
  echo "异常输出目录，拒绝清理：STAGE_DIR='$STAGE_DIR' OUT_DIR='$OUT_DIR'" >&2
  exit 1
fi

rm -rf -- "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

install -m 755 "$BIN_PATH" "${STAGE_DIR}/softmgr"
install -m 644 data/io.github.softmgr.SoftManagement.desktop \
  "${STAGE_DIR}/io.github.softmgr.SoftManagement.desktop"
install -m 644 data/icons/io.github.softmgr.SoftManagement.svg \
  "${STAGE_DIR}/io.github.softmgr.SoftManagement.svg"
install -m 755 scripts/install-local.sh "${STAGE_DIR}/install-local.sh"
install -m 755 scripts/uninstall-local.sh "${STAGE_DIR}/uninstall-local.sh"
install -m 644 README.md "${STAGE_DIR}/README.md"

TARBALL="${OUT_DIR}/${PKG_NAME}.tar.gz"
tar -C "$OUT_DIR" -czf "$TARBALL" "$PKG_NAME"

SHA_FILE="${TARBALL}.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$OUT_DIR"
    sha256sum "$(basename "$TARBALL")" >"$(basename "$SHA_FILE")"
  )
elif command -v shasum >/dev/null 2>&1; then
  (
    cd "$OUT_DIR"
    shasum -a 256 "$(basename "$TARBALL")" >"$(basename "$SHA_FILE")"
  )
else
  echo "缺少 sha256sum/shasum，跳过生成校验文件：$SHA_FILE" >&2
fi

echo "打包完成：$TARBALL"
