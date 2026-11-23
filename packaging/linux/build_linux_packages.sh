#!/usr/bin/env bash
set -euo pipefail

BIN_PATH="${1:-target/release/app-ui}"
OUT_DIR="${2:-dist}"
VERSION="0.1.0"
ARCH="$(uname -m)"

ICON_SRC="packaging/linux/branvio.png"
ICON_REL="usr/share/icons/hicolor/256x256/apps/branvio.png"

# branvio.desktop declares `Icon=branvio`; appimagetool and desktop environments need the
# matching image. No icon asset is committed yet, so fail loudly instead of shipping packages
# without one.
if [[ ! -f "${ICON_SRC}" ]]; then
  echo "error: ${ICON_SRC} is missing." >&2
  echo "       Add the Branvio application icon as a 256x256 PNG at ${ICON_SRC}" >&2
  echo "       (referenced by Icon=branvio in packaging/linux/branvio.desktop)." >&2
  exit 1
fi

mkdir -p "${OUT_DIR}"

# 1. Portable AppDir & tarball / AppImage layout
APPDIR="${OUT_DIR}/Branvio.AppDir"
rm -rf "${APPDIR}"
mkdir -p "${APPDIR}/usr/bin" "${APPDIR}/usr/share/applications" "$(dirname "${APPDIR}/${ICON_REL}")"
cp "${BIN_PATH}" "${APPDIR}/usr/bin/branvio"
chmod +x "${APPDIR}/usr/bin/branvio"
cp packaging/linux/branvio.desktop "${APPDIR}/branvio.desktop"
cp packaging/linux/branvio.desktop "${APPDIR}/usr/share/applications/branvio.desktop"
cp "${ICON_SRC}" "${APPDIR}/branvio.png"
cp "${ICON_SRC}" "${APPDIR}/${ICON_REL}"
# License texts + third-party attributions must ship with every binary distribution.
LICENSE_FILES=(LICENSE-MIT LICENSE-APACHE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.md)
mkdir -p "${APPDIR}/usr/share/doc/branvio"
cp "${LICENSE_FILES[@]}" "${APPDIR}/usr/share/doc/branvio/"
cat > "${APPDIR}/AppRun" <<'EOF'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
exec "${HERE}/usr/bin/branvio" "$@"
EOF
chmod +x "${APPDIR}/AppRun"

if command -v appimagetool >/dev/null 2>&1; then
  # No `|| true`: a failed AppImage build must fail the release job.
  ARCH=x86_64 appimagetool "${APPDIR}" "${OUT_DIR}/Branvio-${VERSION}-x86_64.AppImage"
else
  tar -C "${OUT_DIR}" -czf "${OUT_DIR}/branvio-${VERSION}-linux-${ARCH}.tar.gz" Branvio.AppDir
fi

# 2. Debian .deb package
DEB_ROOT="${OUT_DIR}/deb_pkg"
rm -rf "${DEB_ROOT}"
mkdir -p "${DEB_ROOT}/DEBIAN" "${DEB_ROOT}/usr/bin" "${DEB_ROOT}/usr/share/applications"
cp "${BIN_PATH}" "${DEB_ROOT}/usr/bin/branvio"
chmod 755 "${DEB_ROOT}/usr/bin/branvio"
cp packaging/linux/branvio.desktop "${DEB_ROOT}/usr/share/applications/branvio.desktop"
mkdir -p "$(dirname "${DEB_ROOT}/${ICON_REL}")"
cp "${ICON_SRC}" "${DEB_ROOT}/${ICON_REL}"
mkdir -p "${DEB_ROOT}/usr/share/doc/branvio"
cp "${LICENSE_FILES[@]}" "${DEB_ROOT}/usr/share/doc/branvio/"
cat > "${DEB_ROOT}/DEBIAN/control" <<EOF
Package: branvio
Version: ${VERSION}
Section: vcs
Priority: optional
Architecture: amd64
Maintainer: Branvio Contributors
Description: High-performance native Rust Desktop Git GUI Client
EOF
if command -v dpkg-deb >/dev/null 2>&1; then
  dpkg-deb --build "${DEB_ROOT}" "${OUT_DIR}/branvio_${VERSION}_amd64.deb"
fi

# 3. RPM spec metadata stage
mkdir -p "${OUT_DIR}/rpm"
cp "${BIN_PATH}" "${OUT_DIR}/rpm/branvio"
cp "${LICENSE_FILES[@]}" "${OUT_DIR}/rpm/"
