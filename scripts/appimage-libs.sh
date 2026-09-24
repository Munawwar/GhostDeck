#!/usr/bin/env bash

is_appimage_system_library() {
    local path="$1"
    local base

    base="$(basename "$path")"

    case "$base" in
        ld-linux*.so*|libanl.so*|libBrokenLocale.so*|libc.so*|libcidn.so*|libdl.so*|libm.so*|libmvec.so*|libnsl.so*|libnss_*.so*|libpthread.so*|libresolv.so*|librt.so*|libthread_db.so*|libutil.so*)
            return 0
            ;;
    esac

    return 1
}

shared_library_dependencies() {
    local path="$1"

    ldd "$path" 2>/dev/null \
        | awk '
            /=> \// { print $3; next }
            /^\// { print $1; next }
        ' \
        | sort -u
}

binary_replace_string() {
    local file="$1"
    local old="$2"
    local new="$3"

    if [ "${#new}" -gt "${#old}" ]; then
        echo "ERROR: replacement string is longer than original while patching ${file}"
        echo "  original:    ${old}"
        echo "  replacement: ${new}"
        exit 1
    fi

    python3 - "$file" "$old" "$new" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
old = sys.argv[2].encode()
new = sys.argv[3].encode()

data = path.read_bytes()
replacement = new + (b"\0" * (len(old) - len(new)))
patched = data.replace(old, replacement)
if patched != data:
    path.write_bytes(patched)
PY
}

copy_appimage_library_closure() {
    local dest_dir="$1"
    shift

    local -a queue=("$@")
    local -A copied=()
    local -A processed=()
    local source
    local dep
    local resolved
    local target

    mkdir -p "$dest_dir"

    while [ "${#queue[@]}" -gt 0 ]; do
        source="${queue[0]}"
        queue=("${queue[@]:1}")

        if [ ! -e "$source" ] || [ -n "${processed[$source]:-}" ]; then
            continue
        fi
        processed["$source"]=1

        while IFS= read -r dep; do
            if [ -z "$dep" ] || [ ! -e "$dep" ] || is_appimage_system_library "$dep"; then
                continue
            fi

            resolved="$(readlink -f "$dep")"
            target="${dest_dir}/$(basename "$dep")"
            if [ -z "${copied[$resolved]:-}" ] && [ ! -e "$target" ]; then
                cp -L "$dep" "$target"
                chmod 755 "$target"
                strip --strip-debug "$target" 2>/dev/null || true
                assert_glibc_compatibility "$target" "AppImage dependency $(basename "$target")"
            fi
            copied["$resolved"]=1

            queue+=("$resolved")
        done < <(shared_library_dependencies "$source")
    done
}
