#!/bin/sh
# Builds ScanDesk.app (macOS) into target/release/.
set -e
cd "$(dirname "$0")"
cargo build --release
APP=target/release/ScanDesk.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp target/release/scandesk "$APP/Contents/MacOS/ScanDesk"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>ScanDesk</string>
  <key>CFBundleDisplayName</key><string>ScanDesk</string>
  <key>CFBundleIdentifier</key><string>com.scandesk.ScanDesk</string>
  <key>CFBundleExecutable</key><string>ScanDesk</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>ScanDesk looks for network scanners on your local network.</string>
  <key>NSBonjourServices</key><array><string>_uscan._tcp</string></array>
</dict></plist>
PLIST
codesign --force --sign - "$APP" 2>/dev/null || true
echo "Built $APP"
