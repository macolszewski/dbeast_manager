#!/bin/bash
# Build a Finder/Dock application without additional packaging dependencies.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cargo build --release
bundle="target/release/DBeast.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp target/release/dbeast "$bundle/Contents/MacOS/DBeast"
cp assets/branding/DBeast.icns "$bundle/Contents/Resources/DBeast.icns"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
cat > "$bundle/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>DBeast</string>
<key>CFBundleName</key><string>DBeast</string>
<key>CFBundleDisplayName</key><string>DBeast</string>
<key>CFBundleIdentifier</key><string>ai.inprojects.dbeast</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleIconFile</key><string>DBeast</string>
<key>CFBundleShortVersionString</key><string>$version</string>
<key>CFBundleVersion</key><string>$version</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$bundle/Contents/Info.plist"
codesign --force --deep --sign - "$bundle"
echo "Built $bundle"
