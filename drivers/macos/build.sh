#!/bin/bash
# Builds the CapraLink Output / CapraLink Input HAL plug-ins from BlackHole.c.
# Needs only the Xcode Command Line Tools. Output: build/CapraLinkOutput.driver, build/CapraLinkInput.driver
set -euo pipefail
cd "$(dirname "$0")"

VERSION=0.7.1   # upstream BlackHole version (see NOTICE)
# Core Audio HAL plug-in type UUID (fixed). Factory UUIDs below are our own, distinct from BlackHole's.
PLUGIN_TYPE=443ABAB8-E7B3-491A-B985-BEB9187030DB

build() { # $1 bundle name, $2 device name, $3 bundle id, $4 factory UUID
    local out="build/$1.driver"
    rm -rf "$out"
    mkdir -p "$out/Contents/MacOS"
    clang -bundle -O2 -arch arm64 -arch x86_64 -mmacosx-version-min=12.3 -Wno-format-extra-args \
        -DkDriver_Name="\"$2\"" \
        -DkDevice_Name="\"$2\"" \
        -DkPlugIn_BundleID="\"$3\"" \
        -DkManufacturer_Name='"Capra Audio"' \
        -DkHas_Driver_Name_Format=0 \
        -DkNumber_Of_Channels=2 \
        -DkCanBeDefaultSystemDevice=0 \
        -framework CoreAudio -framework CoreFoundation -framework Accelerate \
        -o "$out/Contents/MacOS/$1" BlackHole.c
    cat > "$out/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>English</string>
	<key>CFBundleExecutable</key>
	<string>$1</string>
	<key>CFBundleIdentifier</key>
	<string>$3</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$1</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleSignature</key>
	<string>????</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>CFPlugInFactories</key>
	<dict>
		<key>$4</key>
		<string>BlackHole_Create</string>
	</dict>
	<key>CFPlugInTypes</key>
	<dict>
		<key>$PLUGIN_TYPE</key>
		<array>
			<string>$4</string>
		</array>
	</dict>
</dict>
</plist>
EOF
    codesign --force --sign - "$out"
    echo "built $out"
}

build CapraLinkOutput "CapraLink Output" com.capraaudio.capralink.output 4602CFB0-26BB-4332-A5F7-1C19FC649C7B
build CapraLinkInput  "CapraLink Input"  com.capraaudio.capralink.input  115BE302-6348-4A47-A0FC-62334FBF24C4
