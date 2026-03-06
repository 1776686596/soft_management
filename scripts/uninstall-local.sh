#!/usr/bin/env bash
set -euo pipefail

PREFIX="${PREFIX:-"$HOME/.local"}"
BIN_DIR="${PREFIX}/bin"
APP_DIR="${PREFIX}/share/applications"
ICON_DIR="${PREFIX}/share/icons/hicolor/scalable/apps"

rm -f "${BIN_DIR}/softmgr"
rm -f "${APP_DIR}/io.github.softmgr.SoftManagement.desktop"
rm -f "${ICON_DIR}/io.github.softmgr.SoftManagement.svg"

echo "已卸载：${PREFIX}"

