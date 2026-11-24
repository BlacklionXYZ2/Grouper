#!/usr/bin/env bash
# Build the Rust core as a static library and regenerate the Swift bindings.
#   scripts/build-apple.sh            # release build for this Mac
#   scripts/build-apple.sh debug      # debug build
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-release}"
FLAG=""; [ "$PROFILE" = release ] && FLAG="--release"
PKG=apple/GrouperKit
# Match Package.swift's platform, so C code in dependencies (TLS, SQLite) targets the same macOS.
export MACOSX_DEPLOYMENT_TARGET=14.0

cargo build -p grouper-ffi $FLAG
LIB="target/$PROFILE/libgrouper_ffi"

# Generate Swift + C header + modulemap from the compiled library.
GEN=$(mktemp -d)
cargo run -q -p grouper-ffi --bin uniffi-bindgen -- generate --library "$LIB.dylib" --language swift --out-dir "$GEN"

cp "$GEN/grouper_ffi.swift" "$PKG/Sources/GrouperKit/grouper_ffi.swift"
cp "$GEN/grouper_ffiFFI.h" "$PKG/Sources/grouper_ffiFFI/include/grouper_ffiFFI.h"
cat > "$PKG/Sources/grouper_ffiFFI/include/module.modulemap" <<'MAP'
module grouper_ffiFFI {
    header "grouper_ffiFFI.h"
    export *
}
MAP

# Only the static library goes next to the package, so the linker can't pick the dylib.
mkdir -p "$PKG/lib"
cp "$LIB.a" "$PKG/lib/libgrouper_ffi.a"
rm -rf "$GEN"
echo "GrouperKit updated from $PROFILE build."
