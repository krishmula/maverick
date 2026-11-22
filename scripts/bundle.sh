#!/bin/sh
# Builds Maverick.app, then packs it as a zip and a disk image in target/dist.
set -eu
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' app/Cargo.toml | head -n 1)
arch=$(uname -m)
dist=target/dist
app="$dist/Maverick.app"

cargo build --release --locked -p maverick

rm -rf "$dist"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/maverick "$app/Contents/MacOS/maverick"
cp assets/icon/Maverick.icns "$app/Contents/Resources/Maverick.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Maverick</string>
	<key>CFBundleDisplayName</key><string>Maverick</string>
	<key>CFBundleIdentifier</key><string>dev.maverick.app</string>
	<key>CFBundleExecutable</key><string>maverick</string>
	<key>CFBundleIconFile</key><string>Maverick</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$version</string>
	<key>CFBundleVersion</key><string>$version</string>
	<key>LSMinimumSystemVersion</key><string>11.0</string>
	<key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad hoc signature: Apple Silicon refuses to run unsigned code.
codesign --force --deep --sign - "$app"

ditto -c -k --keepParent "$app" "$dist/Maverick-$version-$arch.zip"
staging=$(mktemp -d)
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
hdiutil create -quiet -volname Maverick -srcfolder "$staging" -format UDZO -ov "$dist/Maverick-$version-$arch.dmg"
rm -rf "$staging"

echo "$dist/Maverick-$version-$arch.dmg"
echo "$dist/Maverick-$version-$arch.zip"
