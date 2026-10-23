#!/usr/bin/env bash
# Packages a release owlet-gpui binary as Owlet.app and Owlet.dmg.
#
# Usage: bundle-macos.sh <owlet-gpui binary> <version> <out dir>
#
# The bundle identifier matches the former Tauri app (dev.owlet.ui) so the
# new app replaces it in /Applications. It is ad-hoc signed without Hardened
# Runtime, so wasmtime's JIT needs no entitlements (see docs/plugins.md).
set -euo pipefail

if [ "$#" -ne 3 ]; then
  echo "usage: $0 <owlet-gpui binary> <version> <out dir>" >&2
  exit 2
fi
bin=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)

app="${out}/Owlet.app"
rm -rf "${app}" "${out}/Owlet.dmg"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
cp "${bin}" "${app}/Contents/MacOS/owlet-gpui"
cp "${here}/assets/icons/icon.icns" "${app}/Contents/Resources/icon.icns"

cat > "${app}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>Owlet</string>
	<key>CFBundleExecutable</key>
	<string>owlet-gpui</string>
	<key>CFBundleIconFile</key>
	<string>icon</string>
	<key>CFBundleIdentifier</key>
	<string>dev.owlet.ui</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>Owlet</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>${version}</string>
	<key>CFBundleVersion</key>
	<string>${version}</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

codesign --force --sign - "${app}"
codesign --verify --strict "${app}"

# The disk image holds the app plus an /Applications link for drag-install.
stage=$(mktemp -d)
trap 'rm -rf "${stage}"' EXIT
cp -R "${app}" "${stage}/"
ln -s /Applications "${stage}/Applications"
hdiutil create -volname Owlet -srcfolder "${stage}" -ov -format UDZO "${out}/Owlet.dmg"
echo "built ${out}/Owlet.app and ${out}/Owlet.dmg"
