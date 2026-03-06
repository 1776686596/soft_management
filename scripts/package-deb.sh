#!/usr/bin/env bash
set -euo pipefail

VERSION_RAW="${1:-dev}"
OUT_DIR="${2:-dist}"

BIN_PATH="target/release/softmgr"
DESKTOP_FILE="data/io.github.softmgr.SoftManagement.desktop"
ICON_FILE="data/icons/io.github.softmgr.SoftManagement.svg"

if [[ ! -x "$BIN_PATH" ]]; then
  echo "未找到可执行文件：$BIN_PATH" >&2
  echo "请先执行：cargo build --release" >&2
  exit 1
fi
if [[ ! -f "$DESKTOP_FILE" ]]; then
  echo "未找到 desktop 文件：$DESKTOP_FILE" >&2
  exit 1
fi
if [[ ! -f "$ICON_FILE" ]]; then
  echo "未找到图标文件：$ICON_FILE" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

# Debian 版本号只允许 [0-9A-Za-z.+~:-]；并且最好以数字开头。
VERSION="${VERSION_RAW#v}"
VERSION="$(echo "$VERSION" | sed -E 's/[^0-9A-Za-z.+~:-]+/-/g; s/^-+//; s/-+$//')"
if [[ -z "$VERSION" ]]; then
  VERSION="0.0.0"
fi
if [[ ! "$VERSION" =~ ^[0-9] ]]; then
  VERSION="0.0.0+${VERSION}"
fi

ARCH="$(dpkg --print-architecture 2>/dev/null || true)"
if [[ -z "$ARCH" ]]; then
  case "$(uname -m)" in
    x86_64) ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *) ARCH="$(uname -m)" ;;
  esac
fi

PKG_NAME="softmgr"
STAGE_DIR="${OUT_DIR}/deb_${PKG_NAME}_${VERSION}_${ARCH}"
DEB_PATH="${OUT_DIR}/${PKG_NAME}_${VERSION}_${ARCH}.deb"

if [[ -z "$STAGE_DIR" || "$STAGE_DIR" == "/" ]]; then
  echo "异常输出目录，拒绝清理：STAGE_DIR='$STAGE_DIR'" >&2
  exit 1
fi
if [[ "$STAGE_DIR" != "${OUT_DIR}/"* ]]; then
  echo "异常输出目录，拒绝清理：STAGE_DIR='$STAGE_DIR' OUT_DIR='$OUT_DIR'" >&2
  exit 1
fi

rm -rf -- "$STAGE_DIR"
mkdir -p "$STAGE_DIR/DEBIAN"
mkdir -p "$STAGE_DIR/usr/bin"
mkdir -p "$STAGE_DIR/usr/share/applications"
mkdir -p "$STAGE_DIR/usr/share/icons/hicolor/scalable/apps"

install -m 755 "$BIN_PATH" "$STAGE_DIR/usr/bin/softmgr"
install -m 644 "$DESKTOP_FILE" \
  "$STAGE_DIR/usr/share/applications/io.github.softmgr.SoftManagement.desktop"
install -m 644 "$ICON_FILE" \
  "$STAGE_DIR/usr/share/icons/hicolor/scalable/apps/io.github.softmgr.SoftManagement.svg"

cat >"$STAGE_DIR/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: softmgr contributors <noreply@github.com>
Depends: libgtk-4-1 (>= 4.12), libadwaita-1-0 (>= 1.4), xdg-utils
Description: Linux software and development environment unified management tool
 A native GNOME/GTK4 app that unifies software discovery and dev-environment insights.
EOF

dpkg-deb --build --root-owner-group "$STAGE_DIR" "$DEB_PATH"

SHA_FILE="${DEB_PATH}.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$OUT_DIR"
    sha256sum "$(basename "$DEB_PATH")" >"$(basename "$SHA_FILE")"
  )
elif command -v shasum >/dev/null 2>&1; then
  (
    cd "$OUT_DIR"
    shasum -a 256 "$(basename "$DEB_PATH")" >"$(basename "$SHA_FILE")"
  )
else
  echo "缺少 sha256sum/shasum，跳过生成校验文件：$SHA_FILE" >&2
fi

echo "打包完成：$DEB_PATH"
