#!/bin/bash
# Builds VocalScope.app for macOS.
#
#   apps/macos/build.sh            release build  -> apps/macos/build/VocalScope.app
#   apps/macos/build.sh --debug    debug build of both the core and the app
#
# Steps: build the Rust core as a static library, generate its Swift
# bindings, build the Swift app with SwiftPM, then assemble and ad-hoc sign
# the application bundle. Needs only the Xcode Command Line Tools and Rust.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PROFILE=release
SWIFT_CONFIG=release
CARGO_FLAGS=(--release)
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE=debug
  SWIFT_CONFIG=debug
  CARGO_FLAGS=()
fi
JOBS="${VOCALSCOPE_BUILD_JOBS:-4}"
# Must match the deployment target in Package.swift, or the C parts of the
# core (SQLite) are built for a newer macOS than the app claims to support.
export MACOSX_DEPLOYMENT_TARGET=14.0

# From the macOS 27 SDK on, SwiftUI's property wrappers are compiler macros
# whose plug-in ships with full Xcode but not with the Command Line Tools.
# With only the tools installed, build against the newest earlier SDK they
# include; the app runs on the same systems either way.
if [[ -z "${SDKROOT:-}" ]]; then
  DEVELOPER="$(xcode-select -p 2>/dev/null || true)"
  if [[ "$DEVELOPER" == */CommandLineTools && ! -e "$DEVELOPER/usr/lib/swift/host/plugins/libSwiftUIMacros.dylib" ]]; then
    SDK_MAJOR="$(xcrun --show-sdk-version 2>/dev/null | cut -d. -f1)"
    if [[ "${SDK_MAJOR:-0}" -ge 27 ]]; then
      OLDER_SDK="$(ls -d "$DEVELOPER"/SDKs/MacOSX2[0-6].*.sdk 2>/dev/null | sort -V | tail -1)"
      if [[ -n "$OLDER_SDK" ]]; then
        echo "==> Using $(basename "$OLDER_SDK") (the default SDK needs full Xcode)"
        export SDKROOT="$OLDER_SDK"
      fi
    fi
  fi
fi

cd "$ROOT"
echo "==> Building core ($PROFILE)"
cargo build -p vocalscope-core -j "$JOBS" ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

echo "==> Generating Swift bindings"
GENERATED="$HERE/.generated"
rm -rf "$GENERATED" && mkdir -p "$GENERATED"
cargo run -q -p vocalscope-core -j "$JOBS" --features bindgen --bin uniffi-bindgen ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} -- \
  generate --library "target/$PROFILE/libvocalscope_core.dylib" --language swift --out-dir "$GENERATED"
mkdir -p "$HERE/Sources/VocalScopeCore" "$HERE/Sources/vocalscope_coreFFI/include"
cp "$GENERATED/vocalscope_core.swift" "$HERE/Sources/VocalScopeCore/vocalscope_core.swift"
cp "$GENERATED/vocalscope_coreFFI.h" "$HERE/Sources/vocalscope_coreFFI/include/vocalscope_coreFFI.h"

# A directory holding only the static library, so the linker cannot pick the
# dynamic one that sits next to it in target/.
LIBDIR="$HERE/.corelib"
mkdir -p "$LIBDIR"
cp "target/$PROFILE/libvocalscope_core.a" "$LIBDIR/libvocalscope_core.a"

echo "==> Building app ($SWIFT_CONFIG)"
cd "$HERE"
swift build -c "$SWIFT_CONFIG" -j "$JOBS"
BINARY="$(swift build -c "$SWIFT_CONFIG" --show-bin-path)/VocalScope"

echo "==> Assembling VocalScope.app"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -1)"
APP="$HERE/build/VocalScope.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/VocalScope"
[[ "$PROFILE" == release ]] && strip -x "$APP/Contents/MacOS/VocalScope"
cp "$HERE/Resources/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
sed "s/__VERSION__/$VERSION/g" "$HERE/Resources/Info.plist" > "$APP/Contents/Info.plist"
codesign --force --sign - "$APP" >/dev/null 2>&1

echo "Built $APP ($(du -sh "$APP" | cut -f1), version $VERSION)"
