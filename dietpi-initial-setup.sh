#!/usr/bin/env bash
# Prepare a freshly flashed DietPi SD card (Raspberry Pi Zero 2 W) for headless use:
#   - WiFi enabled (SSID from WIFI_SSID, password prompted), Ethernet disabled
#     The Zero 2 W radio is 2.4 GHz only, so use the 2.4 GHz network.
#   - SSH server (Dropbear) enabled with a public key for root/dietpi, password logins disabled
#
# Run on the Mac with the card's boot partition mounted.
# See https://dietpi.com/docs/usage/#how-to-do-an-automatic-base-installation-at-first-boot-dietpi-automation
#
# Usage: WIFI_SSID=MyNetwork WIFI_COUNTRY=DE ./dietpi-initial-setup.sh [BOOT_VOLUME]
#   BOOT_VOLUME   mounted boot partition (default: "/Volumes/NO NAME")
# Required environment:
#   WIFI_SSID     network name (2.4 GHz)
#   WIFI_COUNTRY  2-letter ISO country code, e.g. DE
# Environment overrides:
#   SSH_PUBKEY    path to public key (default: ~/.ssh/ida.pub)
#   HOSTNAME_NEW  hostname to set (default: cala-radio)
set -euo pipefail

BOOT="${1:-/Volumes/NO NAME}"
WIFI_SSID="${WIFI_SSID:?set WIFI_SSID to the 2.4 GHz network name}"
WIFI_COUNTRY="${WIFI_COUNTRY:?set WIFI_COUNTRY to a 2-letter ISO country code, e.g. DE}"
SSH_PUBKEY="${SSH_PUBKEY:-$HOME/.ssh/ida.pub}"
HOSTNAME_NEW="${HOSTNAME_NEW:-cala-radio}"

DIETPI_TXT="$BOOT/dietpi.txt"
WIFI_TXT="$BOOT/dietpi-wifi.txt"

die() { echo "error: $*" >&2; exit 1; }

[[ -f "$DIETPI_TXT" ]] || die "$DIETPI_TXT not found (is the boot partition mounted?)"
[[ -f "$WIFI_TXT" ]] || die "$WIFI_TXT not found"
[[ -f "$SSH_PUBKEY" ]] || die "public key $SSH_PUBKEY not found"

PUBKEY="$(head -n1 "$SSH_PUBKEY" | tr -d '\r')"
[[ "$PUBKEY" =~ ^(ssh-(ed25519|rsa|dss)|ecdsa-sha2-nistp(256|384|521)|sk-[a-z0-9@.-]+)\ [A-Za-z0-9+/=]+ ]] \
  || die "$SSH_PUBKEY does not look like an SSH public key"
[[ "$WIFI_COUNTRY" =~ ^[A-Z]{2}$ ]] || die "WIFI_COUNTRY must be 2 capital letters"
[[ "$HOSTNAME_NEW" =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$ ]] || die "invalid hostname: $HOSTNAME_NEW"

read -r -s -p "WiFi password for \"$WIFI_SSID\": " WIFI_KEY; echo
read -r -s -p "Repeat password: " WIFI_KEY2; echo
[[ "$WIFI_KEY" == "$WIFI_KEY2" ]] || die "passwords do not match"
(( ${#WIFI_KEY} >= 8 && ${#WIFI_KEY} <= 63 )) || die "WPA passphrase must be 8-63 characters"

# Rewrite a file in place through awk (values passed via ENVIRON, so no escaping issues).
# Writes back with cat to keep the original file on the FAT partition.
awk_inplace() {
  local file="$1"; shift
  local tmp; tmp="$(mktemp)"
  awk "$@" "$file" > "$tmp"
  cat "$tmp" > "$file"
  rm -f "$tmp"
}

# Set KEY=value for an existing uncommented key in dietpi.txt.
set_dietpi() {
  grep -q "^$1=" "$DIETPI_TXT" || die "key $1 not found in dietpi.txt"
  K="$1" V="$2" awk_inplace "$DIETPI_TXT" \
    'index($0, ENVIRON["K"] "=") == 1 { print ENVIRON["K"] "=" ENVIRON["V"]; next } { print }'
}

# --- dietpi.txt ---
set_dietpi AUTO_SETUP_NET_WIFI_ENABLED 1
set_dietpi AUTO_SETUP_NET_ETHERNET_ENABLED 0
set_dietpi AUTO_SETUP_SSH_SERVER_INDEX -1   # Dropbear
set_dietpi SOFTWARE_DISABLE_SSH_PASSWORD_LOGINS 1   # pubkey only
set_dietpi AUTO_SETUP_NET_WIFI_COUNTRY_CODE "$WIFI_COUNTRY"
set_dietpi AUTO_SETUP_NET_HOSTNAME "$HOSTNAME_NEW"

# Replace any active AUTO_SETUP_SSH_PUBKEY lines with ours, placed after the commented example.
KEY="$PUBKEY" awk_inplace "$DIETPI_TXT" '
  /^AUTO_SETUP_SSH_PUBKEY=/ { next }
  { print }
  /^#AUTO_SETUP_SSH_PUBKEY=/ && !done { print "AUTO_SETUP_SSH_PUBKEY=" ENVIRON["KEY"]; done = 1 }
  END { if (!done) exit 1 }
' || die "could not find AUTO_SETUP_SSH_PUBKEY template line in dietpi.txt"

# --- dietpi-wifi.txt (entry 0; file is sourced by bash, so single-quote the values) ---
SSID="$WIFI_SSID" PSK="$WIFI_KEY" awk_inplace "$WIFI_TXT" '
  function sq(s,   n, parts, i, out) {
    n = split(s, parts, "\047")
    out = parts[1]
    for (i = 2; i <= n; i++) out = out "\047\\\047\047" parts[i]
    return "\047" out "\047"
  }
  /^aWIFI_SSID\[0\]=/ { print "aWIFI_SSID[0]=" sq(ENVIRON["SSID"]); next }
  /^aWIFI_KEY\[0\]=/  { print "aWIFI_KEY[0]=" sq(ENVIRON["PSK"]); next }
  /^aWIFI_KEYMGR\[0\]=/ { print "aWIFI_KEYMGR[0]=\047WPA-PSK\047"; next }
  { print }
'
unset WIFI_KEY WIFI_KEY2

# Remove macOS ._ AppleDouble files from the FAT partition
dot_clean -m "$BOOT" 2>/dev/null || true

echo "Done. Resulting settings:"
grep -E '^(AUTO_SETUP_NET_(WIFI|ETHERNET)_ENABLED|AUTO_SETUP_NET_WIFI_COUNTRY_CODE|AUTO_SETUP_NET_HOSTNAME|AUTO_SETUP_SSH_SERVER_INDEX|AUTO_SETUP_SSH_PUBKEY|SOFTWARE_DISABLE_SSH_PASSWORD_LOGINS)=' "$DIETPI_TXT" \
  | sed -E 's/^(AUTO_SETUP_SSH_PUBKEY=[^ ]+ .{12}).*( [^ ]*)$/\1...\2/'
grep -E '^aWIFI_(SSID|KEYMGR)\[0\]=' "$WIFI_TXT"
echo "aWIFI_KEY[0]=<hidden>"
echo
echo "Eject with: diskutil eject \"$BOOT\""
