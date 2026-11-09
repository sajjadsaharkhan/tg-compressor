#!/usr/bin/env bash
# Build "TG Compress.app" and install it to /Applications (pass a folder to install elsewhere).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(dirname "$HERE")"
DEST="${1:-/Applications}"
NAME="TG Compress"
BIN="tg-compress"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -1)"

export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --manifest-path "$CRATE/Cargo.toml"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APP="$WORK/$NAME.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$CRATE/target/release/$BIN" "$APP/Contents/MacOS/$BIN"

echo "Rendering icon..."
swift "$HERE/make-icon.swift" "$WORK/icon.png"
ICONSET="$WORK/AppIcon.iconset"
mkdir "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s "$WORK/icon.png" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) "$WORK/icon.png" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>local.tg-compress</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.video</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

# Ad-hoc signature so Gatekeeper accepts the locally built bundle.
codesign --force --deep --sign - "$APP"

rm -rf "${DEST:?}/$NAME.app"
cp -R "$APP" "$DEST/"
# Make Finder, Launchpad and Spotlight pick up the new icon right away.
touch "$DEST/$NAME.app"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$DEST/$NAME.app" || true
echo "Installed: $DEST/$NAME.app"
