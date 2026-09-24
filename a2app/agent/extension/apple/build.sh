#!/bin/bash
# Build and embed Robrix's ExtensionFoundation app extension.
#
#   build.sh <host-bundle-id> <app-bundle> <octos-dylib>
#
# Produces, inside the app bundle:
#   Contents/Frameworks/libRobrixExtensionHost.dylib   (dlopen'ed by a2app-agent)
#   Contents/Extensions/RobrixAgent.appex              (sandboxed Swift shell)
#   Contents/Extensions/Robrix.appexpt                 (extension point metadata)
#
# Everything is ad-hoc signed; no Apple Developer account is required. Built
# only for macOS 26+ (ExtensionFoundation's AppExtensionPoint era), which is
# also why the host loader treats a failed dlopen as "no extension installed".
set -euo pipefail

HOST_BUNDLE_ID="${1:?usage: build.sh <host-bundle-id> <app-bundle> <octos-dylib>}"
APP_BUNDLE="${2:?usage: build.sh <host-bundle-id> <app-bundle> <octos-dylib>}"
OCTOS_DYLIB="${3:?usage: build.sh <host-bundle-id> <app-bundle> <octos-dylib>}"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SDK=$(xcrun --sdk macosx --show-sdk-path)
TARGET=arm64-apple-macos26.0

EXT_DIR="$APP_BUNDLE/Contents/Extensions"
FW_DIR="$APP_BUNDLE/Contents/Frameworks"
APPEX="$EXT_DIR/RobrixAgent.appex"
APPEX_ID="$HOST_BUNDLE_ID.agent"

mkdir -p "$FW_DIR" "$APPEX/Contents/MacOS" "$APPEX/Contents/Frameworks"

# 1. Host bridge dylib (loaded by Rust at runtime).
swiftc -O -parse-as-library -sdk "$SDK" -target "$TARGET" \
    -framework ExtensionFoundation \
    -emit-library -o "$FW_DIR/libRobrixExtensionHost.dylib" \
    "$HERE/HostBridge.swift"

# 2. Appex executable, with the host bundle id substituted into the @Bind.
sed "s/__HOST_BUNDLE_ID__/$HOST_BUNDLE_ID/g" "$HERE/AgentExtension.swift" \
    > "$APPEX/Contents/MacOS/AgentExtension.generated.swift"
swiftc -O -parse-as-library -sdk "$SDK" -target "$TARGET" \
    -framework ExtensionFoundation \
    -emit-executable -o "$APPEX/Contents/MacOS/RobrixAgent" \
    -Xlinker -e -Xlinker _NSExtensionMain \
    "$APPEX/Contents/MacOS/AgentExtension.generated.swift"
rm -f "$APPEX/Contents/MacOS/AgentExtension.generated.swift"

# 3. The Octos host-managed core, embedded in the appex and dlopen'ed by it.
cp "$OCTOS_DYLIB" "$APPEX/Contents/Frameworks/liboctos_ffi.dylib"
install_name_tool -id @rpath/liboctos_ffi.dylib "$APPEX/Contents/Frameworks/liboctos_ffi.dylib"

# 4. Appex Info.plist: the binding key is what matches it to the host.
cat > "$APPEX/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
	<key>CFBundleExecutable</key><string>RobrixAgent</string>
	<key>CFBundleIdentifier</key><string>$APPEX_ID</string>
	<key>CFBundleName</key><string>RobrixAgent</string>
	<key>CFBundleDisplayName</key><string>Robrix Agent</string>
	<key>CFBundlePackageType</key><string>XPC!</string>
	<key>CFBundleShortVersionString</key><string>1.0</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>CFBundleSupportedPlatforms</key><array><string>MacOSX</string></array>
	<key>LSMinimumSystemVersion</key><string>26.0</string>
	<key>EXAppExtensionAttributes</key>
	<dict>
		<key>EXExtensionPointIdentifier</key><string>$HOST_BUNDLE_ID.agent-host</string>
	</dict>
</dict></plist>
PLIST
printf 'XPC!' > "$APPEX/Contents/PkgInfo"

# 5. Extension point metadata for the host. This is the file Xcode would
# generate with EX_ENABLE_EXTENSION_POINT_GENERATION=YES; the format is small
# and stable, so we write it directly.
cat > "$EXT_DIR/Robrix.appexpt" <<APPEXT
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
	<key>EXVersion</key><integer>2</integer>
	<key>$HOST_BUNDLE_ID.agent-host</key>
	<dict>
		<key>_EXScopeRestriction</key><string>application</string>
		<key>EXExtensionPointName</key><string>agent-host</string>
	</dict>
</dict></plist>
APPEXT

# 6. Sign nested code first, then the appex, then the host bridge.
codesign --force --sign - "$APPEX/Contents/Frameworks/liboctos_ffi.dylib" >/dev/null 2>&1
codesign --force --sign - --entitlements "$HERE/Extension.entitlements" "$APPEX" >/dev/null 2>&1
codesign --force --sign - "$FW_DIR/libRobrixExtensionHost.dylib" >/dev/null 2>&1

echo "robrix-extension: embedded $APPEX (point $HOST_BUNDLE_ID.agent-host)"
