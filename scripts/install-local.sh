#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-"$HOME/.local"}"
BIN_DIR="${PREFIX}/bin"
APP_DIR="${PREFIX}/share/applications"
ICON_DIR="${PREFIX}/share/icons/hicolor/scalable/apps"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

install -m 755 "${SCRIPT_DIR}/softmgr" "${BIN_DIR}/softmgr"
install -m 644 "${SCRIPT_DIR}/io.github.softmgr.SoftManagement.desktop" \
  "${APP_DIR}/io.github.softmgr.SoftManagement.desktop"
install -m 644 "${SCRIPT_DIR}/io.github.softmgr.SoftManagement.svg" \
  "${ICON_DIR}/io.github.softmgr.SoftManagement.svg"

cat <<EOF
安装完成：
- 可执行文件：${BIN_DIR}/softmgr
- 桌面入口：${APP_DIR}/io.github.softmgr.SoftManagement.desktop
- 图标：${ICON_DIR}/io.github.softmgr.SoftManagement.svg

提示：
- 请确保 ${BIN_DIR} 在 PATH 中（例如在 ~/.profile 里加入：export PATH="\$HOME/.local/bin:\$PATH"）
- 如未能启动，请先安装系统依赖（GTK4 / libadwaita）
EOF

