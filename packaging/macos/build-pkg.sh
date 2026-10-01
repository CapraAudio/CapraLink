#!/bin/bash
# Builds CapraLink-<version>-macOS.pkg: the app into /Applications and the CapraLink Input/Output
# audio drivers into /Library/Audio/Plug-Ins/HAL. Usage: build-pkg.sh <CapraLink.app> <version> <out dir>
set -euo pipefail
app=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
"$here/../../drivers/macos/build.sh"

stage=$(mktemp -d)
# without extended attributes, so the payload carries no ._ metadata files
ditto --norsrc --noextattr --noqtn "$app" "$stage/root/Applications/$(basename "$app")"
for d in "$here/../../drivers/macos/build/"*.driver; do
    ditto --norsrc --noextattr --noqtn "$d" "$stage/root/Library/Audio/Plug-Ins/HAL/$(basename "$d")"
done
# install exactly here, never "upgrade" another copy found elsewhere on disk
pkgbuild --analyze --root "$stage/root" "$stage/components.plist"
i=0
while plutil -extract "$i" xml1 -o /dev/null "$stage/components.plist" 2>/dev/null; do
    plutil -replace "$i.BundleIsRelocatable" -bool NO "$stage/components.plist"
    i=$((i + 1))
done
mkdir -p "$out"
pkgbuild --root "$stage/root" --component-plist "$stage/components.plist" --scripts "$here/scripts" \
    --identifier com.capraaudio.capralink.pkg --version "$version" --install-location / \
    "$out/CapraLink-$version-macOS.pkg"
rm -rf "$stage"
