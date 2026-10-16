#!/usr/bin/env bash
#
# Build shoes as an XCFramework for iOS.
#
# Output: output/ios/Shoes.xcframework
#
# Requirements:
#   - macOS with Xcode and cbindgen installed
#   - Rust with preinstalled iOS targets and cached locked dependencies
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="$ROOT_DIR/output/ios"
XCFRAMEWORK_NAME="Shoes"
LIB_NAME="libshoes.a"

cd "$ROOT_DIR"

# Fail before changing tracked headers or cleaning build output.
command -v cbindgen >/dev/null || {
    echo "Error: preinstall cbindgen before the offline build" >&2
    exit 1
}
command -v xcodebuild >/dev/null || {
    echo "Error: preinstall Xcode before the offline build" >&2
    exit 1
}
echo "==> Generating and checking C header via cbindgen"
# Compare a temporary header so a failed check cannot overwrite local work.
GENERATED_HEADER="$(mktemp)"
trap 'rm -f -- "$GENERATED_HEADER"' EXIT
CARGO_NET_OFFLINE=true cbindgen --config "$ROOT_DIR/cbindgen.toml" --output "$GENERATED_HEADER"
if ! cmp -s "$GENERATED_HEADER" "$ROOT_DIR/include/shoes.h"; then
    echo "Error: include/shoes.h differs from cbindgen output; regenerate and review it separately" >&2
    exit 1
fi
git diff --exit-code HEAD -- include/shoes.h

echo "==> Cleaning output directory"
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/device" "$OUTPUT_DIR/sim"

# aws-lc-sys uses ___chkstk_darwin which requires iOS 13+.
# Set to 16.0 to match a reasonable modern minimum.
export IPHONEOS_DEPLOYMENT_TARGET="16.0"

echo "==> Checking preinstalled iOS Rust targets"
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
    rustup target list --installed | grep -Fx "$target" >/dev/null || {
        echo "Error: preinstall Rust target $target before the offline build"
        exit 1
    }
done

echo "==> Building for aarch64-apple-ios (physical device)"
cargo build --release --locked --offline --target aarch64-apple-ios

echo "==> Building for aarch64-apple-ios-sim (Apple Silicon simulator)"
cargo build --release --locked --offline --target aarch64-apple-ios-sim

echo "==> Copying libraries"
cp "target/aarch64-apple-ios/release/$LIB_NAME"     "$OUTPUT_DIR/device/$LIB_NAME"
cp "target/aarch64-apple-ios-sim/release/$LIB_NAME" "$OUTPUT_DIR/sim/$LIB_NAME"

echo "==> Packaging as XCFramework"
xcodebuild -create-xcframework \
    -library "$OUTPUT_DIR/device/$LIB_NAME" \
    -headers "$ROOT_DIR/include" \
    -library "$OUTPUT_DIR/sim/$LIB_NAME" \
    -headers "$ROOT_DIR/include" \
    -output "$OUTPUT_DIR/$XCFRAMEWORK_NAME.xcframework"

echo ""
echo "Done: $OUTPUT_DIR/$XCFRAMEWORK_NAME.xcframework"
echo ""
echo "Xcode integration:"
echo "  1. Drag $XCFRAMEWORK_NAME.xcframework into your project (check 'Copy if needed')"
echo "  2. Link it under Target > General > Frameworks, Libraries, and Embedded Content"
echo "  3. Add a bridging header that includes: #include \"shoes.h\""
