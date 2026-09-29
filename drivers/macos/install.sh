#!/bin/bash
# Installs the CapraLink HAL plug-ins. Run: sudo drivers/macos/install.sh (after build.sh)
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "Run with sudo: sudo $0" >&2; exit 1; }
cd "$(dirname "$0")"
HAL=/Library/Audio/Plug-Ins/HAL
for b in CapraLinkOutput CapraLinkInput; do
    [ -d "build/$b.driver" ] || { echo "build/$b.driver missing; run ./build.sh first (without sudo)" >&2; exit 1; }
    rm -rf "$HAL/$b.driver"
    cp -R "build/$b.driver" "$HAL/"
    chown -R root:wheel "$HAL/$b.driver"
done
killall coreaudiod   # launchd restarts it
echo "Installed. 'CapraLink Output' and 'CapraLink Input' should appear in Audio MIDI Setup."
