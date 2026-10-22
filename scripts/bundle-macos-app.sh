#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

echo "[*] Building release binary..."
cargo build --release

APP_NAME="LAN Audio.app"
APP_DIR="target/release/${APP_NAME}"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

rm -rf "${APP_DIR}"
mkdir -p "${MACOS_DIR}" "${RESOURCES_DIR}"

cp target/release/lan-audio "${MACOS_DIR}/lan-audio"
chmod +x "${MACOS_DIR}/lan-audio"

cat > "${CONTENTS_DIR}/Info.plist" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>LAN Audio</string>
    <key>CFBundleDisplayName</key>
    <string>LAN Audio</string>
    <key>CFBundleIdentifier</key>
    <string>com.lanaudio.app</string>
    <key>CFBundleVersion</key>
    <string>0.2.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>lan-audio</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>LAN Audio captures audio from BlackHole or your chosen input device to stream over your local network.</string>
</dict>
</plist>
EOF

echo "[+] Created macOS application bundle at: ${APP_DIR}"
echo "    To install to /Applications, run:"
echo "    cp -R \"${APP_DIR}\" /Applications/"
