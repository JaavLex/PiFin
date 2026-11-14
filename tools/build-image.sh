#!/usr/bin/env bash
# Build the ready-made PiFin image: Raspberry Pi OS Lite (Trixie, arm64) with PiFin
# installed (pifin-setup.sh install in a chroot). Its first boot only configures (~1 min,
# offline); the rest is done from the phone. Run by .github/workflows/image.yml.
#
#   sudo tools/build-image.sh --app <pifin-aarch64> --version 0.3.0 [--out DIR] [--work DIR]
#   tools/build-image.sh --dry-run ...      print every step, change nothing, no root needed
#
# Needs root (loop devices, mount, chroot), xz, curl, sfdisk, e2fsprogs; on a non-arm64
# host also qemu-user-static with binfmt (the chroot runs arm64 programs through it).
set -euo pipefail
cd "$(dirname "$0")/.."

# Pinned base image (Raspberry Pi OS Lite, 64-bit). Update both lines together.
RPIOS_URL=https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/2026-10-06-raspios-trixie-arm64-lite.img.xz
RPIOS_SHA256=483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843
GROW_MB=2048          # room for PiFin's packages; shrunk back to the content afterwards
USER_NAME=pifin
HOST_NAME=pifin

DRY=0 APP="" VERSION="" OUT=dist WORK=""
usage() { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1 ;;
    --app) APP=${2:?}; shift ;;
    --version) VERSION=${2:?}; shift ;;
    --out) OUT=${2:?}; shift ;;
    --work) WORK=${2:?}; shift ;;
    -h|--help) usage ;;
    *) echo "unknown option: $1" >&2; usage ;;
  esac
  shift
done
[ -n "$APP" ] && [ -n "$VERSION" ] || usage
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "--version must look like 0.3.0" >&2; exit 2; }
WORK=${WORK:-$OUT/work}
IMG=$WORK/pifin.img
MNT=$WORK/root
NAME=pifin-$VERSION.img

say() { echo; echo "=== $*"; }
# run a command, or only print it with --dry-run
run() { echo "+ $*"; [ "$DRY" = 1 ] || "$@"; }
# the same for a shell snippet (pipes, redirects)
runsh() { echo "+ $1"; [ "$DRY" = 1 ] || bash -c "$1"; }

LOOP="" MOUNTED=()
cleanup() {
  [ "$DRY" = 1 ] && return
  for ((i=${#MOUNTED[@]}-1; i>=0; i--)); do umount -l "${MOUNTED[i]}" 2>/dev/null || true; done
  [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
}
trap cleanup EXIT
mnt() { run mount "$@"; MOUNTED+=("${!#}"); }
# e2fsck exits 1 when it fixed something: fine; 4 and up is an error
fsck() { run e2fsck -fy "$1" || [ $? -le 1 ]; }

say "Checks"
if [ "$DRY" = 0 ]; then
  [ "$(id -u)" = 0 ] || { echo "needs root (loop devices, mount, chroot): run with sudo, or use --dry-run" >&2; exit 1; }
  for t in curl xz sha256sum losetup sfdisk e2fsck resize2fs dumpe2fs chroot python3; do
    command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 1; }
  done
  [ -f "$APP" ] || { echo "no app binary at $APP" >&2; exit 1; }
fi
QEMU=""
if [ "$(uname -m)" != aarch64 ]; then
  QEMU=$(command -v qemu-aarch64-static || true)
  [ -n "$QEMU" ] || [ "$DRY" = 1 ] || { echo "not on arm64: install qemu-user-static (with binfmt)" >&2; exit 1; }
  QEMU=${QEMU:-/usr/bin/qemu-aarch64-static}
  echo "arm64 programs run through $QEMU"
fi
run mkdir -p "$WORK" "$OUT"

say "Base image (pinned, checksum verified)"
BASE=$WORK/$(basename "$RPIOS_URL")
if [ ! -f "$BASE" ] || [ "$DRY" = 1 ]; then
  run curl -fL --retry 3 -o "$BASE" "$RPIOS_URL"
fi
runsh "echo '$RPIOS_SHA256  $BASE' | sha256sum -c -"
runsh "xz -dc '$BASE' > '$IMG'"

say "Grow the root partition by ${GROW_MB} MB"
run truncate -s +"${GROW_MB}M" "$IMG"
runsh "echo ', +' | sfdisk --no-reread -N 2 '$IMG'"
if [ "$DRY" = 1 ]; then LOOP=/dev/loopX; else LOOP=$(losetup -fP --show "$IMG"); fi
echo "loop device: $LOOP"
fsck "${LOOP}p2"
run resize2fs "${LOOP}p2"

say "Mount"
run mkdir -p "$MNT"
mnt "${LOOP}p2" "$MNT"
mnt "${LOOP}p1" "$MNT/boot/firmware"
for d in dev dev/pts proc sys; do mnt --bind "/$d" "$MNT/$d"; done
mnt -t tmpfs tmpfs "$MNT/run"
mnt -t tmpfs tmpfs "$MNT/tmp"
[ -n "$QEMU" ] && run install -m 0755 "$QEMU" "$MNT/usr/bin/qemu-aarch64-static"
# DNS inside the chroot; the image's own resolv.conf is put back afterwards
run cp -a "$MNT/etc/resolv.conf" "$WORK/resolv.conf.orig"
runsh "rm -f '$MNT/etc/resolv.conf' && cp -L /etc/resolv.conf '$MNT/etc/resolv.conf'"
# no service may start inside the chroot (packages' postinst scripts)
runsh "printf '#!/bin/sh\nexit 101\n' > '$MNT/usr/sbin/policy-rc.d' && chmod 0755 '$MNT/usr/sbin/policy-rc.d'"

say "PiFin files (files/install.list) and the app"
while read -r src dst mode; do
  case "$src" in ''|'#'*) continue ;; esac
  run install -D -m "$mode" "files/$src" "$MNT$dst"
  run sed -i -e "s/@@PIFIN_USER@@/$USER_NAME/g" -e "s/@@PIFIN_HOSTNAME@@/$HOST_NAME/g" "$MNT$dst"
done < files/install.list
run install -m 0755 "$APP" "$MNT/tmp/pifin-aarch64"

say "Install phase in the chroot"
run chroot "$MNT" /usr/bin/env PIFIN_IMAGE_BUILD=1 PIFIN_APP_BIN=/tmp/pifin-aarch64 \
  PIFIN_VERSION="$VERSION" PIFIN_USER="$USER_NAME" /usr/local/sbin/pifin-setup.sh install

say "Device defaults: host name, HDMI (as build.py writes them)"
runsh "echo $HOST_NAME > '$MNT/etc/hostname'"
runsh "sed -i 's/\\braspberrypi\\b/$HOST_NAME/g' '$MNT/etc/hosts'"
runsh "sed -i 's/^dtparam=audio=on/dtparam=audio=off/' '$MNT/boot/firmware/config.txt'"
runsh "grep -q '^# --- PiFin ---' '$MNT/boot/firmware/config.txt' || printf '\n# --- PiFin ---\n[all]\nhdmi_force_hotplug=1\n' >> '$MNT/boot/firmware/config.txt'"

say "Clean up (caches, logs, per-machine ids)"
run chroot "$MNT" apt-get clean
runsh "rm -rf '$MNT'/var/lib/apt/lists/* '$MNT'/var/cache/apt/*.bin '$MNT/usr/sbin/policy-rc.d' '$MNT/root/.cache'"
runsh "rm -f '$MNT'/etc/ssh/ssh_host_*"   # made at first boot, unique per box
runsh "[ ! -e '$MNT/etc/machine-id' ] || : > '$MNT/etc/machine-id'"
runsh "find '$MNT/var/log' -type f -exec truncate -s 0 {} +"
runsh "rm -f '$MNT/etc/resolv.conf' && cp -a '$WORK/resolv.conf.orig' '$MNT/etc/resolv.conf'"
[ -n "$QEMU" ] && run rm -f "$MNT/usr/bin/qemu-aarch64-static"

say "Unmount"
if [ "$DRY" = 0 ]; then
  for ((i=${#MOUNTED[@]}-1; i>=0; i--)); do umount "${MOUNTED[i]}"; done
  MOUNTED=()
else
  echo "+ umount (all of the above, in reverse)"
fi

say "Shrink the root partition to its content (+ 256 MB); the first boot grows it to the card"
fsck "${LOOP}p2"
run resize2fs -M "${LOOP}p2"
if [ "$DRY" = 0 ]; then
  BLOCKS=$(dumpe2fs -h "${LOOP}p2" 2>/dev/null | sed -n 's/^Block count: *//p')
  BSIZE=$(dumpe2fs -h "${LOOP}p2" 2>/dev/null | sed -n 's/^Block size: *//p')
  START=$(sfdisk -d "$IMG" | sed -n "s|^$IMG"'2 : start= *\([0-9]*\),.*|\1|p')
  SECTORS=$(( (BLOCKS * BSIZE + 256 * 1024 * 1024) / 512 ))
  losetup -d "$LOOP"; LOOP=""
  echo "$START, $SECTORS" | sfdisk --no-reread -N 2 "$IMG"
  truncate -s $(( (START + SECTORS) * 512 )) "$IMG"
  # the file system may use the 256 MB margin too
  LOOP=$(losetup -fP --show "$IMG"); e2fsck -fy "${LOOP}p2" || true; resize2fs "${LOOP}p2"; losetup -d "$LOOP"; LOOP=""
else
  echo "+ sfdisk -N 2 (start, content + 256 MB); truncate the image after it"
fi

say "Compress"
run mv "$IMG" "$OUT/$NAME"
run xz -T0 -6 -f "$OUT/$NAME"
runsh "cd '$OUT' && sha256sum '$NAME.xz' > '$NAME.xz.sha256'"
echo
echo "done: $OUT/$NAME.xz"
