#!/usr/bin/env bash
set -euo pipefail

BIN_PATH="${1:-target/release/app-ui}"
OUT_DIR="${2:-dist}"
APP_BUNDLE="${OUT_DIR}/Branvio.app"

mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APP_BUNDLE}/Contents/Resources"
cp "${BIN_PATH}" "${APP_BUNDLE}/Contents/MacOS/branvio"
chmod +x "${APP_BUNDLE}/Contents/MacOS/branvio"
cp packaging/macos/Info.plist "${APP_BUNDLE}/Contents/Info.plist"
# App icon referenced by CFBundleIconFile (regenerate with scripts/build_app_icons.py).
cp packaging/macos/branvio.icns "${APP_BUNDLE}/Contents/Resources/branvio.icns"
# License texts + third-party attributions must ship with every binary distribution.
cp LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.md "${APP_BUNDLE}/Contents/Resources/"

if command -v codesign >/dev/null 2>&1; then
  codesign --force --deep --sign - "${APP_BUNDLE}" || true
fi

if command -v hdiutil >/dev/null 2>&1; then
  hdiutil create -volname "Branvio" -srcfolder "${APP_BUNDLE}" -ov -format UDZO "${OUT_DIR}/Branvio-macOS.dmg"
fi
