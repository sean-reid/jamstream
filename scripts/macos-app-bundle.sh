#!/usr/bin/env bash
# Assembles a minimal, correct JamStream.app bundle around a prebuilt
# jamstream-app binary (release.yml calls this with the lipo'd universal
# binary; locally any single-arch build works).
#
#   usage: macos-app-bundle.sh <jamstream-app-binary> <version> <out-dir> [jamstreamd-binary]
#
# <version> is the release tag with or without the leading "v", e.g.
# v0.1.3-beta.2. Produces <out-dir>/JamStream.app and lints the generated
# Info.plist with plutil. The app icon is the COMMITTED
# crates/client/assets/icon/jamstream.icns (derived from jamstream.svg by
# scripts/render-icon.sh), copied into Contents/Resources and referenced
# by CFBundleIconFile; nothing is rendered here, so CI needs no icon
# tooling.
#
# [jamstreamd-binary], when given, is bundled at Contents/MacOS/jamstreamd
# beside the app executable (a legal home for helper executables under
# Apple's nesting rules; codesign must sign it explicitly before signing
# the bundle). The local provider resolves this app-adjacent helper, so
# hosting on this computer works with nothing else installed.
set -euo pipefail

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
  echo "usage: $0 <jamstream-app-binary> <version> <out-dir> [jamstreamd-binary]" >&2
  exit 2
fi

BINARY=$1
VERSION=${2#v}
OUT_DIR=$3
SERVER_BINARY=${4:-}

if [ ! -f "$BINARY" ]; then
  echo "error: binary not found: $BINARY" >&2
  exit 1
fi

if [ -n "$SERVER_BINARY" ] && [ ! -f "$SERVER_BINARY" ]; then
  echo "error: jamstreamd binary not found: $SERVER_BINARY" >&2
  exit 1
fi

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
ICNS="$REPO_ROOT/crates/client/assets/icon/jamstream.icns"
if [ ! -f "$ICNS" ]; then
  echo "error: app icon not found: $ICNS (it is committed; regenerate with scripts/render-icon.sh)" >&2
  exit 1
fi

# Apple's version fields only accept dotted integers, so the SemVer
# prerelease suffix cannot pass through verbatim. For v0.1.3-beta.2:
#   CFBundleShortVersionString = 0.1.3   (marketing version)
#   CFBundleVersion            = 0.1.3.2 (build number; the trailing .2 is
#                                         the beta number, so each beta of
#                                         the same base version stays
#                                         distinct)
# Stable tags (v0.2.0) use the same value for both.
SHORT_VERSION=${VERSION%%-*}
BUILD_VERSION=$SHORT_VERSION
case "$VERSION" in
  *-*)
    PRERELEASE=${VERSION#*-}
    PRERELEASE_NUM=${PRERELEASE##*.}
    case "$PRERELEASE_NUM" in
      '' | *[!0-9]*) ;; # no trailing number; keep the base build version
      *) BUILD_VERSION="$SHORT_VERSION.$PRERELEASE_NUM" ;;
    esac
    ;;
esac

APP="$OUT_DIR/JamStream.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 0755 "$BINARY" "$APP/Contents/MacOS/jamstream-app"
if [ -n "$SERVER_BINARY" ]; then
  install -m 0755 "$SERVER_BINARY" "$APP/Contents/MacOS/jamstreamd"
fi
install -m 0644 "$ICNS" "$APP/Contents/Resources/JamStream.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>JamStream</string>
	<key>CFBundleExecutable</key>
	<string>jamstream-app</string>
	<key>CFBundleIconFile</key>
	<string>JamStream</string>
	<key>CFBundleIdentifier</key>
	<string>com.seanreid.jamstream</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>JamStream</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$SHORT_VERSION</string>
	<key>CFBundleVersion</key>
	<string>$BUILD_VERSION</string>
	<key>CFBundleURLTypes</key>
	<array>
		<dict>
			<key>CFBundleURLName</key>
			<string>com.seanreid.jamstream</string>
			<key>CFBundleURLSchemes</key>
			<array>
				<string>jamstream</string>
			</array>
		</dict>
	</array>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>JamStream captures your microphone or instrument input to stream audio to your jam session.</string>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist"
if [ -n "$SERVER_BINARY" ]; then
  echo "assembled $APP (version $SHORT_VERSION, build $BUILD_VERSION, bundled jamstreamd)"
else
  echo "assembled $APP (version $SHORT_VERSION, build $BUILD_VERSION)"
fi
