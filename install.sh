#!/bin/sh
# Install or update Droidloom on Arch Linux / Omarchy (x86_64) or Arch Linux ARM (aarch64).
set -eu

case "${1:-}" in
  --help|-h)
    printf 'Usage: sh install.sh\nAdds the Droidloom pacman repository and installs/updates both packages.\n'
    exit 0 ;;
  '') ;;
  *) printf 'Unknown argument: %s\n' "$1" >&2; exit 1 ;;
esac
[ "$#" -le 1 ] || exit 1
machine=$(uname -m)
case "$machine" in
  x86_64|aarch64) ;;
  *) echo 'Droidloom packages require x86_64 or aarch64.' >&2; exit 1 ;;
esac
command -v pacman >/dev/null || { echo 'This installer requires Arch Linux / Omarchy and pacman.' >&2; exit 1; }

# Keep privileges in this small, reviewable installer. Pacman owns installation,
# dependencies, updates and lifecycle hooks; no runtime setup is performed here.
as_root() {
  if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo "$@"; fi
}

config=/etc/pacman.conf
include=/etc/pacman.d/droidloom.conf
repository=$(mktemp)
trap 'rm -f "$repository"' EXIT HUP INT TERM
cat > "$repository" <<REPO
[droidloom]
# Droidloom currently publishes unsigned packages over HTTPS.
SigLevel = Never
Server = https://denialwm.github.io/droidloom/$machine
CacheServer = https://github.com/denialwm/droidloom/releases/download/packages
REPO

# Refuse conflicting manual configurations instead of creating duplicate repos.
if pacman-conf --repo-list | grep -qx droidloom; then
  if ! [ -f "$include" ] || [ "$(cat "$repository")" != "$(cat "$include")" ] ||
     ! grep -qxF "Include = $include" "$config"; then
    echo 'An existing Droidloom repository differs. Review /etc/pacman.conf and /etc/pacman.d/droidloom.conf first.' >&2
    exit 1
  fi
fi
printf '%s\n' 'This adds the Droidloom repository (unsigned packages over HTTPS).' \
  'Administrator access is needed to write pacman configuration and install packages.' \
  'Pacman will ask before a full system upgrade and installation of Droidloom.'
as_root install -m 644 "$repository" "$include"
if ! grep -qxF "Include = $include" "$config"; then
  as_root cp -p "$config" "$config.droidloom.bak"
  printf '\nInclude = %s\n' "$include" | as_root tee -a "$config" >/dev/null
fi
# Full upgrade avoids Arch partial upgrades; keep pacman's confirmation prompt.
as_root pacman -Syu --needed droidloom-runtime droidloom-image
printf '\nDroidloom is installed. Run droidloomctl start as your ordinary desktop user.\n'
