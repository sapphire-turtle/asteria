#!/bin/bash
# Pre-build Run Script: builds the Rust staticlib for the SDK Xcode is
# targeting and copies it to $BUILT_PRODUCTS_DIR/rust/. Runs from $SRCROOT.
set -euo pipefail

# Xcode's script environment does not include rustup's shims.
export PATH="$HOME/.cargo/bin:$PATH"

case "${PLATFORM_NAME:?PLATFORM_NAME is not set}" in
  iphoneos) target=aarch64-apple-ios ;;
  iphonesimulator) target=aarch64-apple-ios-sim ;;
  *) echo "error: unsupported PLATFORM_NAME '$PLATFORM_NAME'" >&2; exit 1 ;;
esac

profile_dir=debug
release_flag=()
if [ "${CONFIGURATION:-}" = "Release" ]; then
  profile_dir=release
  release_flag=(--release)
fi

cargo build --manifest-path ../Cargo.toml -p asteria-ios-spike --lib \
  --target "$target" ${release_flag[@]+"${release_flag[@]}"}

mkdir -p "$BUILT_PRODUCTS_DIR/rust"
cp "../target/$target/$profile_dir/libasteria_ios_spike.a" "$BUILT_PRODUCTS_DIR/rust/"
