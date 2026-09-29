#!/bin/bash
# Removes the CapraLink HAL plug-ins. Run: sudo drivers/macos/uninstall.sh
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "Run with sudo: sudo $0" >&2; exit 1; }
rm -rf /Library/Audio/Plug-Ins/HAL/CapraLinkOutput.driver /Library/Audio/Plug-Ins/HAL/CapraLinkInput.driver
killall coreaudiod   # launchd restarts it
echo "Uninstalled."
