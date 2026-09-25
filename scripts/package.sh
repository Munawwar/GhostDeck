#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"

# Read version from workspace Cargo.toml (single source of truth)
VERSION="${1:-$(grep '^version' "$ROOT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')}"
ARCH="$(uname -m)"
DEB_ARCH="amd64"
[ "$ARCH" = "aarch64" ] && DEB_ARCH="arm64"
RPM_ARCH="x86_64"
[ "$ARCH" = "aarch64" ] && RPM_ARCH="aarch64"

PKG_BASE="ghostdeck-${VERSION}-linux-${ARCH}"
STAGE="/tmp/ghostdeck-staging"
GHOSTTY_INSTALL_ROOT="/tmp/ghostdeck-ghostty-install"
GHOSTTY_SO="${ROOT_DIR}/ghostty/zig-out/lib/libghostty.so"
MAX_GLIBC_VERSION="${GHOSTDECK_MAX_GLIBC:-2.39}"
GHOSTTY_SHARE_DIR=""
GHOSTTY_TERMINFO_DIR=""
ICONS_DIR="${ROOT_DIR}/rust/ghostdeck-host-linux/icons"
APP_ICONS_DIR="${ROOT_DIR}/rust/ghostdeck-host-linux/icons/app"
SKILLS_DIR="${ROOT_DIR}/skills"
DESKTOP_FILE="${ROOT_DIR}/rust/ghostdeck-host-linux/dev.ghostdeck.linux.desktop"
METADATA_FILE="${ROOT_DIR}/rust/ghostdeck-host-linux/dev.ghostdeck.linux.metainfo.xml"
OUT_DIR="${ROOT_DIR}/dist"
GHOSTTY_ZIG_ARGS=(-Doptimize=ReleaseFast -Dcpu=baseline)
CLI_ENTRYPOINT_NAME="ghostdeck"
HOST_ENTRYPOINT_NAME="ghostdeck-host"

remove_tree() {
    local path="$1"

    if [ ! -e "$path" ]; then
        return 0
    fi

    find "$path" -depth -mindepth 1 ! -type d -exec rm -f {} +
    find "$path" -depth -mindepth 1 -type d -exec rmdir {} + 2>/dev/null || true
    rmdir "$path" 2>/dev/null || true
}

version_gt() {
    local left="$1"
    local right="$2"
    [ "$left" != "$right" ] && [ "$(printf '%s\n%s\n' "$left" "$right" | sort -V | tail -n1)" = "$left" ]
}

glibc_requirement_for() {
    local path="$1"

    if ! command -v objdump >/dev/null 2>&1; then
        return 0
    fi

    objdump -T "$path" 2>/dev/null \
        | grep -oE 'GLIBC_[0-9]+\.[0-9]+' \
        | sed 's/^GLIBC_//' \
        | sort -Vu \
        | tail -n1
}

assert_glibc_compatibility() {
    local path="$1"
    local label="$2"
    local required_glibc

    required_glibc="$(glibc_requirement_for "$path" || true)"
    if [ -z "$required_glibc" ]; then
        echo "WARNING: unable to determine GLIBC requirement for ${label}"
        return 0
    fi

    if version_gt "$required_glibc" "$MAX_GLIBC_VERSION"; then
        echo "ERROR: ${label} requires GLIBC_${required_glibc}, which exceeds the supported release baseline GLIBC_${MAX_GLIBC_VERSION}."
        echo "Build release artifacts inside an environment pinned to GLIBC_${MAX_GLIBC_VERSION}."
        echo "Override the baseline intentionally with GHOSTDECK_MAX_GLIBC=<version> if you are targeting a newer distro on purpose."
        exit 1
    fi

    echo "Verified ${label} GLIBC requirement: GLIBC_${required_glibc} (target max GLIBC_${MAX_GLIBC_VERSION})"
}

assert_cli_entrypoint() {
    local path="$1"
    local label="$2"

    if ! "$path" --help 2>&1 | grep -q "ghostdeck CLI"; then
        echo "ERROR: ${label} is not the ghostdeck CLI entrypoint: ${path}"
        exit 1
    fi
}

assert_no_legacy_host_entrypoint() {
    local path="$1"
    local label="$2"

    if [ -e "$path" ]; then
        echo "ERROR: ${label} contains legacy host entrypoint at ${path}"
        echo "Only the CLI may be named 'ghostdeck'; the GTK host must be 'ghostdeck-host'."
        exit 1
    fi
}

install_desktop_file() {
    local src="$1"
    local dest="$2"
    local exec_path="$3"

    sed \
        -e "s|^Exec=.*|Exec=${exec_path}|" \
        -e "s|^TryExec=.*|TryExec=${exec_path}|" \
        "$src" > "$dest"
    chmod 644 "$dest"
}

resolve_ghostty_share_dir() {
    local candidate

    for candidate in \
        "${GHOSTTY_INSTALL_ROOT}/usr/share/ghostty" \
        "${ROOT_DIR}/ghostty/zig-out/share/ghostty" \
        "/usr/local/share/ghostty" \
        "/usr/share/ghostty"
    do
        if [ -d "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

resolve_ghostty_terminfo_dir() {
    local candidate
    local parent

    parent="$(dirname "$GHOSTTY_SHARE_DIR")"

    for candidate in \
        "${GHOSTTY_INSTALL_ROOT}/usr/share/terminfo" \
        "${parent}/terminfo" \
        "/usr/local/share/terminfo" \
        "/usr/share/terminfo"
    do
        if [ -f "${candidate}/g/ghostty" ] || [ -f "${candidate}/x/xterm-ghostty" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done

    return 1
}

copy_ghostty_terminfo_entries() {
    local source_dir="$1"
    local dest_dir="$2"

    mkdir -p "${dest_dir}/g" "${dest_dir}/x"

    if [ -f "${source_dir}/g/ghostty" ]; then
        cp "${source_dir}/g/ghostty" "${dest_dir}/g/ghostty"
    fi

    if [ -f "${source_dir}/x/xterm-ghostty" ]; then
        cp "${source_dir}/x/xterm-ghostty" "${dest_dir}/x/xterm-ghostty"
    fi
}

. "${ROOT_DIR}/scripts/appimage-libs.sh"

configure_ghostty_build_args() {
    if ! command -v pkg-config >/dev/null 2>&1 || ! pkg-config --exists gtk4-layer-shell-0; then
        echo "gtk4-layer-shell not available via pkg-config; building Ghostty with bundled gtk4-layer-shell."
        GHOSTTY_ZIG_ARGS+=(-fno-sys=gtk4-layer-shell)
    fi
}

build_ghostty_resources() {
    echo "Staging Ghostty resources..."
    remove_tree "$GHOSTTY_INSTALL_ROOT"
    mkdir -p "$GHOSTTY_INSTALL_ROOT"

    DESTDIR="$GHOSTTY_INSTALL_ROOT" \
        "$ROOT_DIR/scripts/build-ghostty.sh" \
        --prefix /usr \
        "${GHOSTTY_ZIG_ARGS[@]}" \
        -Demit-docs=false
}

echo "=== GhostDeck Packager ==="
echo "Version: ${VERSION}"
echo "Arch:    ${ARCH}"
echo "GLIBC:   <= ${MAX_GLIBC_VERSION}"

if ! command -v zig >/dev/null 2>&1; then
    echo "ERROR: zig not found in PATH."
    echo "Install Zig, then rerun ./scripts/package.sh"
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "ERROR: python3 not found in PATH."
    echo "Install Python 3, then rerun ./scripts/package.sh"
    exit 1
fi

if [ ! -f "${ROOT_DIR}/ghostty/build.zig" ]; then
    echo "ERROR: Ghostty submodule is missing or uninitialized at ${ROOT_DIR}/ghostty"
    echo "Run: git submodule update --init --recursive"
    exit 1
fi

# Always build libghostty with ReleaseFast to guarantee optimized output.
# Pinning cpu=baseline keeps the shipped library portable across x86_64 CPUs
# that do not expose the builder's ISA extensions, such as AVX-512.
configure_ghostty_build_args
echo "Building libghostty (ReleaseFast, cpu=baseline)..."
"$ROOT_DIR/scripts/build-ghostty.sh" -Dapp-runtime=none "${GHOSTTY_ZIG_ARGS[@]}"
build_ghostty_resources

if [ ! -f "$GHOSTTY_SO" ]; then
    echo "ERROR: libghostty.so not found at ${GHOSTTY_SO} after build"
    exit 1
fi

if ! GHOSTTY_SHARE_DIR="$(resolve_ghostty_share_dir)"; then
    echo "ERROR: Ghostty resources directory not found."
    echo "Looked for:"
    echo "  ${ROOT_DIR}/ghostty/zig-out/share/ghostty"
    echo "  /usr/local/share/ghostty"
    echo "  /usr/share/ghostty"
    exit 1
fi

if ! GHOSTTY_TERMINFO_DIR="$(resolve_ghostty_terminfo_dir)"; then
    echo "ERROR: Ghostty terminfo directory not found."
    echo "Looked for:"
    echo "  $(dirname "$GHOSTTY_SHARE_DIR")/terminfo"
    echo "  /usr/local/share/terminfo"
    echo "  /usr/share/terminfo"
    exit 1
fi

# Build release binary
echo "Building release binary..."
cargo build --release --manifest-path "${ROOT_DIR}/Cargo.toml"

CLI_BINARY="${ROOT_DIR}/target/release/ghostdeck-cli"
HOST_BINARY="${ROOT_DIR}/target/release/ghostdeck"
if [ ! -f "$CLI_BINARY" ]; then
    echo "ERROR: CLI binary not found at ${CLI_BINARY}"
    exit 1
fi
if [ ! -f "$HOST_BINARY" ]; then
    echo "ERROR: Host binary not found at ${HOST_BINARY}"
    exit 1
fi

assert_glibc_compatibility "$GHOSTTY_SO" "libghostty.so"
assert_glibc_compatibility "$CLI_BINARY" "ghostdeck CLI"
assert_glibc_compatibility "$HOST_BINARY" "ghostdeck host"
assert_cli_entrypoint "$CLI_BINARY" "target/release/ghostdeck-cli"

# Clean staging and output
remove_tree "$STAGE"
remove_tree "$OUT_DIR"
mkdir -p "$OUT_DIR"

# =========================================================================
# Helper: populate a prefix tree at a given root
# =========================================================================
populate_tree() {
    local dest="$1"
    local prefix="${2:-/usr/local}"
    local strip_files="${3:-true}"
    local bindir="$dest${prefix}/bin"
    local libexecdir="$dest${prefix}/libexec/ghostdeck"
    local libdir="$dest${prefix}/lib/ghostdeck"
    local ghostty_datadir="$dest${prefix}/share/ghostdeck"
    local ghostty_resdir="$ghostty_datadir/ghostty"
    local appdir="$dest${prefix}/share/applications"
    local metadatadir="$dest${prefix}/share/metainfo"
    local icondir="$dest${prefix}/share/icons/hicolor"

    mkdir -p "$bindir" "$libexecdir" "$libdir" "$ghostty_resdir" "$appdir" "$metadatadir" "$icondir/scalable/actions"

    # Public CLI and private GTK host binary.
    cp "$CLI_BINARY" "$bindir/$CLI_ENTRYPOINT_NAME"
    cp "$HOST_BINARY" "$libexecdir/$HOST_ENTRYPOINT_NAME"
    rm -f "$libexecdir/ghostdeck"
    if [ "$strip_files" = "true" ]; then
        strip "$bindir/$CLI_ENTRYPOINT_NAME"
        strip "$libexecdir/$HOST_ENTRYPOINT_NAME"
    fi
    chmod 755 "$bindir/$CLI_ENTRYPOINT_NAME" "$libexecdir/$HOST_ENTRYPOINT_NAME"
    assert_cli_entrypoint "$bindir/$CLI_ENTRYPOINT_NAME" "packaged $prefix/bin/$CLI_ENTRYPOINT_NAME"
    assert_no_legacy_host_entrypoint "$libexecdir/ghostdeck" "packaged $prefix libexec tree"

    # Shared library
    cp "$GHOSTTY_SO" "$libdir/libghostty.so"
    if [ "$strip_files" = "true" ]; then
        strip --strip-debug "$libdir/libghostty.so"
    fi

    # Ghostty resources required for named themes and shell integration
    cp -r "$GHOSTTY_SHARE_DIR"/. "$ghostty_resdir"
    copy_ghostty_terminfo_entries "$GHOSTTY_TERMINFO_DIR" "$ghostty_datadir/terminfo"
    cp -r "$SKILLS_DIR" "$ghostty_datadir/skills"

    # Desktop file. Use the absolute CLI path so desktop launchers do not
    # accidentally resolve an older GTK host binary named `ghostdeck` from PATH.
    install_desktop_file "$DESKTOP_FILE" "$appdir/dev.ghostdeck.linux.desktop" "$prefix/bin/$CLI_ENTRYPOINT_NAME"
    cp "$METADATA_FILE" "$metadatadir/dev.ghostdeck.linux.metainfo.xml"

    # Action icons
    if [ -d "$ICONS_DIR/hicolor" ]; then
        cp -r "$ICONS_DIR/hicolor/scalable" "$icondir/" 2>/dev/null || true
    fi
    for svg in "$ICONS_DIR"/*.svg; do
        [ -f "$svg" ] && cp "$svg" "$icondir/scalable/actions/"
    done

    # App launcher icons
    if [ -d "$APP_ICONS_DIR" ]; then
        for size in 16 32 128 256 512; do
            src="${APP_ICONS_DIR}/${size}.png"
            if [ -f "$src" ]; then
                mkdir -p "$icondir/${size}x${size}/apps"
                cp "$src" "$icondir/${size}x${size}/apps/ghostdeck.png"
            fi
        done
    fi
}

build_rpm_source_tree() {
    local dest="$1"

    remove_tree "$dest"
    mkdir -p "$dest"
    populate_tree "$dest" "/usr" "false"

    mkdir -p "$dest/etc/ld.so.conf.d"
    echo "/usr/lib/ghostdeck" > "$dest/etc/ld.so.conf.d/ghostdeck.conf"
}

build_rpm_package() {
    local rpm_src_dir="/tmp/ghostdeck-$VERSION"
    local rpm_tarball="/tmp/ghostdeck-$VERSION.tar.gz"
    local rpmbuild_dir="/tmp/rpmbuild-$VERSION"
    local rpm_output="$rpmbuild_dir/RPMS/${RPM_ARCH}/ghostdeck-${VERSION}-1.${RPM_ARCH}.rpm"

    if ! command -v rpmbuild >/dev/null 2>&1; then
        echo "  WARNING: rpmbuild not found, skipping RPM"
        return 0
    fi

    build_rpm_source_tree "$rpm_src_dir"
    tar -czf "$rpm_tarball" -C /tmp "ghostdeck-$VERSION"
    remove_tree "$rpm_src_dir"

    remove_tree "$rpmbuild_dir"
    mkdir -p "$rpmbuild_dir"/{BUILD,RPMS,SOURCES,SPECS}
    cp "$rpm_tarball" "$rpmbuild_dir/SOURCES/"
    cp "$ROOT_DIR/scripts/ghostdeck.spec" "$rpmbuild_dir/SPECS/"

    rpmbuild -bb \
        --define "_topdir $rpmbuild_dir" \
        --define "version $VERSION" \
        --target "$RPM_ARCH" \
        "$rpmbuild_dir/SPECS/ghostdeck.spec" 2>&1

    if [ -f "$rpm_output" ]; then
        cp "$rpm_output" "$OUT_DIR/"
        echo "  -> dist/ghostdeck-${VERSION}-1.${RPM_ARCH}.rpm"
    else
        echo "  WARNING: rpmbuild did not produce expected RPM file"
    fi

    remove_tree "$rpmbuild_dir"
}

# =========================================================================
# 1. Tarball
# =========================================================================
echo ""
echo "--- Building tarball ---"
TARBALL_STAGE="/tmp/${PKG_BASE}"
remove_tree "$TARBALL_STAGE"
mkdir -p "$TARBALL_STAGE"/{lib,libexec/ghostdeck,share/ghostdeck/ghostty,share/ghostdeck/terminfo,share/applications,share/icons/hicolor/scalable/actions}
mkdir -p "$TARBALL_STAGE/share/metainfo"

cp "$CLI_BINARY" "$TARBALL_STAGE/ghostdeck"
cp "$HOST_BINARY" "$TARBALL_STAGE/libexec/ghostdeck/ghostdeck-host"
strip "$TARBALL_STAGE/ghostdeck"
strip "$TARBALL_STAGE/libexec/ghostdeck/ghostdeck-host"
chmod 755 "$TARBALL_STAGE/ghostdeck" "$TARBALL_STAGE/libexec/ghostdeck/ghostdeck-host"
assert_cli_entrypoint "$TARBALL_STAGE/ghostdeck" "tarball ghostdeck"
cp "$GHOSTTY_SO" "$TARBALL_STAGE/lib/libghostty.so"
strip --strip-debug "$TARBALL_STAGE/lib/libghostty.so"
cp -r "$GHOSTTY_SHARE_DIR"/. "$TARBALL_STAGE/share/ghostdeck/ghostty"
copy_ghostty_terminfo_entries "$GHOSTTY_TERMINFO_DIR" "$TARBALL_STAGE/share/ghostdeck/terminfo"
cp -r "$SKILLS_DIR" "$TARBALL_STAGE/share/ghostdeck/skills"
cp "$DESKTOP_FILE" "$TARBALL_STAGE/share/applications/dev.ghostdeck.linux.desktop"
cp "$METADATA_FILE" "$TARBALL_STAGE/share/metainfo/dev.ghostdeck.linux.metainfo.xml"

if [ -d "$ICONS_DIR/hicolor" ]; then
    cp -r "$ICONS_DIR/hicolor/scalable" "$TARBALL_STAGE/share/icons/hicolor/" 2>/dev/null || true
fi
for svg in "$ICONS_DIR"/*.svg; do
    [ -f "$svg" ] && cp "$svg" "$TARBALL_STAGE/share/icons/hicolor/scalable/actions/"
done
if [ -d "$APP_ICONS_DIR" ]; then
    for size in 16 32 128 256 512; do
        src="${APP_ICONS_DIR}/${size}.png"
        if [ -f "$src" ]; then
            mkdir -p "$TARBALL_STAGE/share/icons/hicolor/${size}x${size}/apps"
            cp "$src" "$TARBALL_STAGE/share/icons/hicolor/${size}x${size}/apps/ghostdeck.png"
        fi
    done
fi

# Generate install.sh
cat > "$TARBALL_STAGE/install.sh" << 'INSTALL_EOF'
#!/usr/bin/env bash
set -euo pipefail

PREFIX="/usr/local"
UNINSTALL=false

for arg in "$@"; do
    case "$arg" in
        --prefix=*) PREFIX="${arg#*=}" ;;
        --uninstall) UNINSTALL=true ;;
        -h|--help)
            echo "Usage: install.sh [--prefix=/usr/local] [--uninstall]"
            exit 0
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

need_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "This operation requires root. Re-running with sudo..."
        exec sudo "$0" "$@"
    fi
}

install_desktop_file() {
    local src="$1"
    local dest="$2"
    local exec_path="$3"

    sed \
        -e "s|^Exec=.*|Exec=${exec_path}|" \
        -e "s|^TryExec=.*|TryExec=${exec_path}|" \
        "$src" > "$dest"
    chmod 644 "$dest"
}

legacy_ghostdeck_paths() {
    local sudo_home=""

    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        sudo_home="$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6 || true)"
    fi

    printf '%s\n' \
        "$PREFIX/libexec/ghostdeck/ghostdeck" \
        /usr/local/libexec/ghostdeck/ghostdeck \
        /usr/libexec/ghostdeck/ghostdeck \
        /usr/local/bin/ghostdeck \
        /usr/bin/ghostdeck

    if [ -n "$sudo_home" ]; then
        printf '%s\n' \
            "$sudo_home/.local/libexec/ghostdeck/ghostdeck" \
            "$sudo_home/.local/bin/ghostdeck"
    fi
}

is_legacy_ghostdeck_host() {
    local path="$1"
    local help

    [ -x "$path" ] || return 1
    help="$("$path" --help 2>&1 || true)"
    printf '%s\n' "$help" | grep -q "ghostdeck CLI" && return 1
    printf '%s\n' "$help" | grep -q "GApplication" && return 0
    "$path" --json identify >/tmp/ghostdeck-installer-probe.log 2>&1 && return 1
    grep -q "Unknown option --json" /tmp/ghostdeck-installer-probe.log
}

clean_legacy_ghostdeck_entrypoints() {
    local path

    while IFS= read -r path; do
        [ -n "$path" ] || continue
        [ "$path" = "$PREFIX/bin/ghostdeck" ] && continue
        if [ "${path%/bin/ghostdeck}" != "$path" ]; then
            if is_legacy_ghostdeck_host "$path"; then
                rm -f "$path"
                echo "Removed legacy GhostDeck host entrypoint: $path"
            fi
        elif [ -e "$path" ]; then
            rm -f "$path"
            echo "Removed legacy GhostDeck host entrypoint: $path"
        fi
    done <<EOF_PATHS
$(legacy_ghostdeck_paths)
EOF_PATHS
}

warn_if_ghostdeck_is_shadowed() {
    local expected="$PREFIX/bin/ghostdeck"
    local first

    first="$(PATH="$PREFIX/bin:$PATH" command -v ghostdeck 2>/dev/null || true)"
    if [ "$first" != "$expected" ]; then
        echo "WARNING: the first ghostdeck on PATH is '$first', expected '$expected'."
        echo "         Agent/CLI commands require the GhostDeck CLI entrypoint."
    fi

    if ! "$expected" --help 2>&1 | grep -q "ghostdeck CLI"; then
        echo "ERROR: installed ghostdeck entrypoint is not the CLI: $expected" >&2
        exit 1
    fi
}

remove_tree() {
    local path="$1"

    if [ ! -e "$path" ]; then
        return 0
    fi

    find "$path" -depth -mindepth 1 ! -type d -exec rm -f {} +
    find "$path" -depth -mindepth 1 -type d -exec rmdir {} + 2>/dev/null || true
    rmdir "$path" 2>/dev/null || true
}

if $UNINSTALL; then
    need_root "$@"
    echo "Uninstalling GhostDeck..."
    rm -f "$PREFIX/bin/ghostdeck"
    remove_tree "$PREFIX/libexec/ghostdeck"
    remove_tree "$PREFIX/lib/ghostdeck"
    remove_tree "$PREFIX/share/ghostdeck"
    rm -f /etc/ld.so.conf.d/ghostdeck.conf
    ldconfig 2>/dev/null || true
    rm -f "$PREFIX/share/applications/ghostdeck.desktop"
    rm -f "$PREFIX/share/applications/dev.ghostdeck.linux.desktop"
    rm -f "$PREFIX/share/metainfo/dev.ghostdeck.linux.metainfo.xml"
    for size in 16 32 128 256 512; do
        rm -f "$PREFIX/share/icons/hicolor/${size}x${size}/apps/ghostdeck.png"
    done
    rm -f "$PREFIX/share/icons/hicolor/scalable/apps/ghostdeck.svg"
    rm -f "$PREFIX/share/icons/hicolor/scalable/actions/ghostdeck-globe-symbolic.svg"
    rm -f "$PREFIX/share/icons/hicolor/scalable/actions/ghostdeck-split-horizontal-symbolic.svg"
    rm -f "$PREFIX/share/icons/hicolor/scalable/actions/ghostdeck-split-vertical-symbolic.svg"
    gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
    appstreamcli refresh-cache --force 2>/dev/null || true
    echo "GhostDeck uninstalled."
    exit 0
fi

need_root "$@"
echo "Installing GhostDeck to ${PREFIX}..."

install -Dm755 "$SCRIPT_DIR/ghostdeck" "$PREFIX/bin/ghostdeck"
clean_legacy_ghostdeck_entrypoints
install -Dm755 "$SCRIPT_DIR/libexec/ghostdeck/ghostdeck-host" "$PREFIX/libexec/ghostdeck/ghostdeck-host"
install -Dm644 "$SCRIPT_DIR/lib/libghostty.so" "$PREFIX/lib/ghostdeck/libghostty.so"
if [ -d "$SCRIPT_DIR/share/ghostdeck" ]; then
    cp -r "$SCRIPT_DIR/share/ghostdeck" "$PREFIX/share/"
fi
echo "$PREFIX/lib/ghostdeck" > /etc/ld.so.conf.d/ghostdeck.conf
ldconfig 2>/dev/null || true
rm -f "$PREFIX/share/applications/ghostdeck.desktop"
mkdir -p "$PREFIX/share/applications"
install_desktop_file "$SCRIPT_DIR/share/applications/dev.ghostdeck.linux.desktop" "$PREFIX/share/applications/dev.ghostdeck.linux.desktop" "$PREFIX/bin/ghostdeck"
install -Dm644 "$SCRIPT_DIR/share/metainfo/dev.ghostdeck.linux.metainfo.xml" "$PREFIX/share/metainfo/dev.ghostdeck.linux.metainfo.xml"
if [ -d "$SCRIPT_DIR/share/icons" ]; then
    cp -r "$SCRIPT_DIR/share/icons/hicolor" "$PREFIX/share/icons/"
fi
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true
appstreamcli refresh-cache --force 2>/dev/null || true
warn_if_ghostdeck_is_shadowed

echo ""
echo "GhostDeck installed successfully!"
echo "  CLI:     $PREFIX/bin/ghostdeck"
echo "  Host:    $PREFIX/libexec/ghostdeck/ghostdeck-host"
echo "  Library: $PREFIX/lib/ghostdeck/libghostty.so"
echo "  App:     ghostdeck"
echo ""
echo "System dependencies (install if missing):"
echo "  sudo apt install libgtk-4-1 libadwaita-1-0"
INSTALL_EOF

chmod 755 "$TARBALL_STAGE/install.sh"
tar -czf "$OUT_DIR/${PKG_BASE}.tar.gz" -C /tmp "${PKG_BASE}"
remove_tree "$TARBALL_STAGE"
echo "  -> dist/${PKG_BASE}.tar.gz"

# =========================================================================
# 2. Debian package
# =========================================================================
echo ""
echo "--- Building .deb ---"
DEB_ROOT="$STAGE/deb"
remove_tree "$DEB_ROOT"
populate_tree "$DEB_ROOT" "/usr"

# ldconfig trigger
mkdir -p "$DEB_ROOT/etc/ld.so.conf.d"
echo "/usr/lib/ghostdeck" > "$DEB_ROOT/etc/ld.so.conf.d/ghostdeck.conf"

# Control file
INSTALLED_SIZE=$(du -sk "$DEB_ROOT" | cut -f1)
mkdir -p "$DEB_ROOT/DEBIAN"
cat > "$DEB_ROOT/DEBIAN/control" << EOF
Package: ghostdeck
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${DEB_ARCH}
Installed-Size: ${INSTALLED_SIZE}
Depends: libgtk-4-1, libadwaita-1-0
Maintainer: Will R <will@limux.dev>
Description: GPU-accelerated terminal workspace manager for Linux
 GhostDeck is a terminal workspace manager powered by Ghostty's
 GPU-rendered terminal engine, with split surfaces and tabbed workspaces.
Homepage: https://github.com/Munawwar/GhostDeck
EOF

# Post-install: run ldconfig and update caches
cat > "$DEB_ROOT/DEBIAN/postinst" << 'EOF'
#!/bin/bash
set -e

is_legacy_ghostdeck_host() {
    path="$1"
    [ -x "$path" ] || return 1
    help="$("$path" --help 2>&1 || true)"
    echo "$help" | grep -q "ghostdeck CLI" && return 1
    echo "$help" | grep -q "GApplication" && return 0
    "$path" --json identify >/tmp/ghostdeck-postinst-probe.log 2>&1 && return 1
    grep -q "Unknown option --json" /tmp/ghostdeck-postinst-probe.log
}

ldconfig 2>/dev/null || true
rm -f /usr/libexec/ghostdeck/ghostdeck
rm -f /usr/local/libexec/ghostdeck/ghostdeck
if is_legacy_ghostdeck_host /usr/local/bin/ghostdeck; then
    rm -f /usr/local/bin/ghostdeck
fi
rm -f /usr/share/applications/ghostdeck.desktop
rm -f /usr/local/share/applications/ghostdeck.desktop
gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database /usr/share/applications 2>/dev/null || true
appstreamcli refresh-cache --force 2>/dev/null || true
EOF
chmod 755 "$DEB_ROOT/DEBIAN/postinst"

# Post-remove: clean up
cat > "$DEB_ROOT/DEBIAN/postrm" << 'EOF'
#!/bin/bash
ldconfig 2>/dev/null || true
gtk-update-icon-cache -f -t /usr/share/icons/hicolor 2>/dev/null || true
update-desktop-database /usr/share/applications 2>/dev/null || true
appstreamcli refresh-cache --force 2>/dev/null || true
EOF
chmod 755 "$DEB_ROOT/DEBIAN/postrm"

DEB_FILE="$OUT_DIR/ghostdeck_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --build --root-owner-group "$DEB_ROOT" "$DEB_FILE"
echo "  -> dist/ghostdeck_${VERSION}_${DEB_ARCH}.deb"

# =========================================================================
# 3. RPM package
# =========================================================================
echo ""
echo "--- Building .rpm ---"
build_rpm_package

# =========================================================================
# 4. AppImage
# =========================================================================
echo ""
echo "--- Building AppImage ---"
APPDIR="$STAGE/GhostDeck.AppDir"
remove_tree "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/libexec/ghostdeck" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/metainfo" \
         "$APPDIR/usr/share/icons/hicolor/scalable/actions" \
         "$APPDIR/usr/share/ghostdeck"

# Public CLI and private GTK host binary.
cp "$CLI_BINARY" "$APPDIR/usr/bin/ghostdeck"
cp "$HOST_BINARY" "$APPDIR/usr/libexec/ghostdeck/ghostdeck-host"
strip "$APPDIR/usr/bin/ghostdeck"
strip "$APPDIR/usr/libexec/ghostdeck/ghostdeck-host"
chmod 755 "$APPDIR/usr/bin/ghostdeck" "$APPDIR/usr/libexec/ghostdeck/ghostdeck-host"
assert_cli_entrypoint "$APPDIR/usr/bin/ghostdeck" "AppImage usr/bin/ghostdeck"

# Shared library
cp "$GHOSTTY_SO" "$APPDIR/usr/lib/libghostty.so"
strip --strip-debug "$APPDIR/usr/lib/libghostty.so"

# Bundle non-glibc library dependencies for the CLI, host, and Ghostty.
copy_appimage_library_closure "$APPDIR/usr/lib" "$CLI_BINARY" "$HOST_BINARY" "$GHOSTTY_SO"

# Ghostty resources required for named themes and shell integration
cp -r "$GHOSTTY_SHARE_DIR" "$APPDIR/usr/share/ghostdeck/ghostty"
cp -r "$SKILLS_DIR" "$APPDIR/usr/share/ghostdeck/skills"

# Desktop file (at AppDir root and in usr/share)
cp "$DESKTOP_FILE" "$APPDIR/dev.ghostdeck.linux.desktop"
cp "$DESKTOP_FILE" "$APPDIR/usr/share/applications/dev.ghostdeck.linux.desktop"
cp "$METADATA_FILE" "$APPDIR/usr/share/metainfo/dev.ghostdeck.linux.metainfo.xml"

# Icons
if [ -d "$ICONS_DIR/hicolor" ]; then
    cp -r "$ICONS_DIR/hicolor/scalable" "$APPDIR/usr/share/icons/hicolor/" 2>/dev/null || true
fi
for svg in "$ICONS_DIR"/*.svg; do
    [ -f "$svg" ] && cp "$svg" "$APPDIR/usr/share/icons/hicolor/scalable/actions/"
done
if [ -d "$APP_ICONS_DIR" ]; then
    for size in 16 32 128 256 512; do
        src="${APP_ICONS_DIR}/${size}.png"
        if [ -f "$src" ]; then
            mkdir -p "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps"
            cp "$src" "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/ghostdeck.png"
        fi
    done
fi

# AppImage icon (must be at root as .DirIcon and ghostdeck.png)
if [ -f "$APP_ICONS_DIR/256.png" ]; then
    cp "$APP_ICONS_DIR/256.png" "$APPDIR/ghostdeck.png"
    cp "$APP_ICONS_DIR/256.png" "$APPDIR/.DirIcon"
fi

# AppRun entry point — sets up library path and launches the binary
cat > "$APPDIR/AppRun" << 'APPRUN_EOF'
#!/bin/bash
HERE="$(dirname "$(readlink -f "$0")")"
cd "$HERE"
export LD_LIBRARY_PATH="${HERE}/usr/lib:${LD_LIBRARY_PATH:-}"
export XDG_DATA_DIRS="${HERE}/usr/share:${XDG_DATA_DIRS:-/usr/share}"
exec "${HERE}/usr/bin/ghostdeck" "$@"
APPRUN_EOF
chmod 755 "$APPDIR/AppRun"

# Build AppImage
APPIMAGE_FILE="$OUT_DIR/GhostDeck-${VERSION}-${ARCH}.AppImage"
if command -v appimagetool &>/dev/null; then
    APPIMAGETOOL="appimagetool"
elif [ -x /tmp/appimagetool ]; then
    APPIMAGETOOL="/tmp/appimagetool"
else
    echo "WARNING: appimagetool not found, skipping AppImage"
    APPIMAGETOOL=""
fi

if [ -n "$APPIMAGETOOL" ]; then
    ARCH="$ARCH" "$APPIMAGETOOL" "$APPDIR" "$APPIMAGE_FILE" 2>&1 | tail -3
    echo "  -> dist/GhostDeck-${VERSION}-${ARCH}.AppImage"
fi

# =========================================================================
# Summary
# =========================================================================
echo ""
echo "=== Packages created in dist/ ==="
ls -lh "$OUT_DIR"/ 2>/dev/null
echo ""
echo "Install options:"
echo "  Tarball:   tar xzf dist/${PKG_BASE}.tar.gz && cd ${PKG_BASE} && sudo ./install.sh"
echo "  Deb:       sudo dpkg -i ./dist/ghostdeck_${VERSION}_${DEB_ARCH}.deb"
echo "  RPM:       sudo rpm -i ./dist/ghostdeck-${VERSION}-1.${RPM_ARCH}.rpm"
echo "  AppImage:  chmod +x dist/GhostDeck-${VERSION}-${ARCH}.AppImage && ./dist/GhostDeck-${VERSION}-${ARCH}.AppImage"
