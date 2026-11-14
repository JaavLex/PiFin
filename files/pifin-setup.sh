#!/bin/bash
# PiFin setup, in two phases:
#
#   install    packages, PiFin's files, the app binary, services. No secrets and no
#              per-device config: tools/build-image.sh runs it in a chroot to make the
#              ready-made image (PIFIN_IMAGE_BUILD=1: nothing is started or restarted).
#   configure  per device, at first boot (~1 min, works offline): Wi-Fi country, the
#              setup hotspot, and the home Wi-Fi / Tailscale key when build.py wrote them
#              to /etc/pifin/secrets.env. Everything else is set later from the phone.
#
# Without an argument (pifin-setup.service at every boot until /etc/pifin/setup-done):
# only configure when the image already has PiFin (/etc/pifin/version), else both
# (build.py on a plain Raspberry Pi OS card). Idempotent, resumes after a power loss.
#
# Result: Raspberry Pi 4 that boots straight into the PiFin app (a native Jellyfin
# browser under cage; playback in mpv with hardware decoding), TV remote via HDMI-CEC,
# phone remote on port 8080, Wi-Fi setup hotspot when no known network is around.
#
# /etc/pifin/secrets.env (optional, written by build.py from pifin.env):
#   PIFIN_HOSTNAME, PIFIN_USER, WIFI_COUNTRY, HOTSPOT_SSID, HOTSPOT_PASS,
#   JELLYFIN_URL, and until setup finishes WIFI_SSID/WIFI_PSK/TS_AUTHKEY
set -uo pipefail

PIFIN_RELEASE=${PIFIN_RELEASE:-https://github.com/JaavLex/PiFin/releases/latest/download/pifin-aarch64}
export DEBIAN_FRONTEND=noninteractive

step() { echo; echo ">>> $*"; }
retry() { local n=0; until "$@"; do n=$((n+1)); [ $n -ge 5 ] && return 1; echo "retry $n: $*"; sleep 15; done; }
# false while building the image in a chroot: no running systemd to restart things in
live() { [ -z "${PIFIN_IMAGE_BUILD:-}" ]; }

# install | configure | all, from the argument and whether PiFin is installed
pick_mode() {
  case "${1:-}" in
    install|configure|all) echo "$1" ;;
    "") if [ -s "${PIFIN_ROOT:-}/etc/pifin/version" ]; then echo configure; else echo all; fi ;;
    *) return 1 ;;
  esac
}

# ---------------------------------------------------------------- install ----
# Reads no secrets.env: the same image goes to everyone.
install_phase() {
  USER_NAME=${PIFIN_USER:-pifin}
  USER_HOME=/home/$USER_NAME

  step "User $USER_NAME (kiosk session; no password: set one with build.py or sudo)"
  if ! id "$USER_NAME" >/dev/null 2>&1; then
    # Raspberry Pi OS Lite ships a placeholder "pi" (shell nologin) for its rename wizard
    if id pi >/dev/null 2>&1; then
      usermod -l "$USER_NAME" -d "$USER_HOME" -m pi && groupmod -n "$USER_NAME" pi
    else
      useradd -m "$USER_NAME"
    fi
    passwd -l "$USER_NAME" >/dev/null || true
  fi
  usermod -s /bin/bash "$USER_NAME"
  mkdir -p "$USER_HOME"
  # Raspberry Pi OS asks for a user name on the console at first boot otherwise
  systemctl disable userconfig.service 2>/dev/null || true
  systemctl mask userconfig.service 2>/dev/null || true

  step "Persistent logs (capped)"
  mkdir -p /etc/systemd/journald.conf.d
  cat > /etc/systemd/journald.conf.d/90-pifin-persistent.conf <<'EOF'
# PiFin: keep journalctl -u comitup / -u NetworkManager after a reboot.
[Journal]
Storage=persistent
SystemMaxUse=50M
EOF
  live && systemctl restart systemd-journald   # flushes the runtime journal into /var/log/journal

  if live; then
    step "Waiting for internet"
    for i in $(seq 1 120); do
      curl -fsI --max-time 5 https://pkgs.tailscale.com >/dev/null && break
      sleep 5
    done
  fi

  step "Repair any interrupted package install"
  dpkg --configure -a || true
  apt-get install -f -y || true

  step "apt update"
  retry apt-get update

  step "Packages"
  # cage: kiosk Wayland compositor. wlr-randr + jq: fit the screen. mpv: playback
  # with the Pi's hardware decoders. evdev + cec-utils: TV remote bridge.
  # pipewire: HDMI audio. iw: check Wi-Fi power save from the field.
  retry apt-get install -y cage wlr-randr jq iw mpv fonts-dejavu-core \
    python3-evdev cec-utils pipewire pipewire-pulse wireplumber \
    unattended-upgrades curl xz-utils
  if ! dpkg -s comitup >/dev/null 2>&1 && ! retry apt-get install -y comitup; then
    echo "comitup not in default repos, adding upstream repo"
    curl -fsSL -o /tmp/comitup-src.deb https://davesteele.github.io/comitup/deb/davesteele-comitup-apt-source_1.2_all.deb
    dpkg -i /tmp/comitup-src.deb && apt-get update && retry apt-get install -y comitup
  fi

  step "Tailscale (installed, signed in later from the phone or by build.py's key)"
  if ! command -v tailscale >/dev/null; then
    retry bash -c 'curl -fsSL https://tailscale.com/install.sh | sh'
  fi
  systemctl enable tailscaled 2>/dev/null || true

  step "PiFin app (prebuilt aarch64 binary)"
  if [ -n "${PIFIN_APP_BIN:-}" ]; then
    install -m 0755 "$PIFIN_APP_BIN" /usr/local/bin/pifin
  elif [ ! -x /usr/local/bin/pifin ]; then
    retry curl -fsSL -o /tmp/pifin "$PIFIN_RELEASE" && install -m 0755 /tmp/pifin /usr/local/bin/pifin
  fi
  mkdir -p /etc/pifin "$USER_HOME/.config/pifin"
  chown -R "$USER_NAME:$USER_NAME" "$USER_HOME"

  # comitup runs its own dnsmasq instance
  systemctl disable dnsmasq 2>/dev/null || true
  live && systemctl stop dnsmasq 2>/dev/null
  systemctl mask dnsmasq 2>/dev/null || true
  systemctl enable comitup || true

  step "Wi-Fi power save off (brcmfmac drops and slow scans)"
  mkdir -p /etc/NetworkManager/conf.d
  cat > /etc/NetworkManager/conf.d/90-pifin-wifi.conf <<'EOF'
# PiFin: brcmfmac power save makes scans slow and links drop on the Pi 4.
# 2 = "disable"; NM applies it to every profile that leaves it unspecified,
# which includes the ones comitup creates from the phone's setup page.
[connection]
wifi.powersave = 2
EOF
  if live; then
    systemctl reload NetworkManager || true        # reloads conf.d, does not touch the link
    for w in /sys/class/net/*/wireless; do         # best effort for the current association
      [ -d "$w" ] && { i=${w%/wireless}; iw dev "${i##*/}" set power_save off 2>/dev/null || true; }
    done
  fi

  step "Comitup: retry the known networks 1 min after a failed attempt"
  # comitup's FAIL_TIMEOUT is hard-coded: it is how long comitup waits in HOTSPOT
  # after a failed CONNECTING sweep before it tries the known networks again.
  CS=/usr/share/comitup/comitup/states.py
  if [ -f "$CS" ] && ! grep -q "^FAIL_TIMEOUT = 60" "$CS"; then
    if sed -i "s/^FAIL_TIMEOUT = 360.*/FAIL_TIMEOUT = 60  # PiFin: retry a minute after a failed sweep/" "$CS" \
       && python3 -m py_compile "$CS"; then
      if grep -q "PiFin" "$CS"; then
        rm -rf /usr/share/comitup/comitup/__pycache__
        live && { systemctl try-restart comitup || true; }
      else
        echo "comitup tweak not applied: states.py does not match the expected line, leaving it untouched"
      fi
    else
      echo "comitup's states.py does not match the expected line: leaving it untouched"
    fi
  fi

  step "App, TV remote (CEC) and phone remote services"
  usermod -aG video,render,input,audio,tty,netdev "$USER_NAME"
  live && systemctl daemon-reload
  systemctl enable pifin-app.service pifin-cec-remote.service pifin-remote.service \
    pifin-wifi-watchdog.service pifin-settingsd.service pifin-setup.service
  systemctl set-default multi-user.target
  # desktop helpers the kiosk never uses (~60 MB, and they pulled in Xwayland)
  systemctl --global mask xdg-desktop-portal.service xdg-desktop-portal-gtk.service \
    xdg-document-portal.service xdg-permission-store.service at-spi-dbus-bus.service \
    mpris-proxy.service 2>/dev/null || true

  step "Updates: security only, at night (never while watching)"
  cat > /etc/apt/apt.conf.d/20auto-upgrades <<EOF
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF
  for t in apt-daily apt-daily-upgrade; do
    mkdir -p /etc/systemd/system/$t.timer.d
    printf '[Timer]\nOnCalendar=\nOnCalendar=*-*-* 04:00\nRandomizedDelaySec=30m\nPersistent=false\n' \
      > /etc/systemd/system/$t.timer.d/pifin-night.conf
  done
  live && systemctl daemon-reload

  step "Security: firewall, services without root"
  # The Pi joins networks you don't control and is a member of your tailnet.
  # pifin-firewall: see the script; the tailnet ACL in the README is the real fence.
  systemctl enable pifin-firewall.service
  live && { udevadm control --reload || true; }   # uinput for group "input"

  step "Faster boot (from the next boot on)"
  # Measured on a Pi 4: ~7 s less from power-on to the first frame.
  # - swap: compressed RAM only; the default zram+file re-created a 2 GB swap
  #   file on the SD card at every boot (~1.4 s before /tmp could mount)
  mkdir -p /etc/rpi/swap.conf.d
  printf '[Main]\nMechanism=zram\n' > /etc/rpi/swap.conf.d/90-pifin.conf
  # (the firmware part is in configure_phase: the image's first boot still needs the
  # initramfs, which grows the root partition to the card)

  step "Installed version"
  local v=${PIFIN_VERSION:-}
  if [ -z "$v" ]; then
    v=$(/usr/local/bin/pifin --version 2>/dev/null | sed -n 's/^pifin //p')
  fi
  echo "${v:-0.0.0}" > /etc/pifin/version
  echo "PiFin ${v:-unknown} installed"
}

# -------------------------------------------------------------- configure ----
# Per device. Every value is optional: a fresh image gets them from the phone.
configure_phase() {
  [ -f /etc/pifin/secrets.env ] && source /etc/pifin/secrets.env
  USER_NAME=${PIFIN_USER:-pifin}
  USER_HOME=/home/$USER_NAME

  step "Wi-Fi country + unblock"
  if [ -n "${WIFI_COUNTRY:-}" ]; then
    raspi-config nonint do_wifi_country "$WIFI_COUNTRY" || true
  else
    echo "WIFI_COUNTRY not set: keeping the OS regulator default"
  fi
  rfkill unblock wifi || true

  step "Setup hotspot password"
  if [ -z "${HOTSPOT_PASS:-}" ]; then
    # a fresh image: a random one, shown on the TV in the hotspot's QR code; the phone
    # wizard replaces it. No 0/O/1/l: it is also typed from the TV screen.
    HOTSPOT_PASS=$(tr -dc 'a-km-np-z2-9' < /dev/urandom | head -c 12)
    ( umask 077; printf "HOTSPOT_PASS='%s'\n" "$HOTSPOT_PASS" >> /etc/pifin/secrets.env )
    echo "generated a hotspot password (shown on the TV)"
  fi

  step "Comitup config (Wi-Fi setup hotspot)"
  # comitup reads this file with Python's configparser: values are taken as written (no
  # quotes), and `%` must be doubled
  AP_NAME=${HOTSPOT_SSID:-PiFin-setup}; AP_NAME=${AP_NAME//\%/%%}
  AP_PASS=${HOTSPOT_PASS//\%/%%}
  cat > /etc/comitup.conf <<EOF
ap_name: ${AP_NAME}
ap_password: ${AP_PASS}
web_service:
enable_appliance_mode: false
external_callback: /usr/local/bin/pifin-comitup-callback
EOF

  step "Tailscale"
  if [ -z "${TS_AUTHKEY:-}" ]; then
    echo "no TS_AUTHKEY: Tailscale stays signed out (sign in from the phone; it is optional)"
  else
    systemctl enable --now tailscaled
    if tailscale status >/dev/null 2>&1; then
      echo "already logged in to Tailscale"
    else
      retry tailscale up --authkey="$TS_AUTHKEY" --hostname="${PIFIN_HOSTNAME:-pifin}" \
        --accept-routes=false --timeout=60s
    fi
  fi
  # never route into the server's home LAN, even if it advertises one
  command -v tailscale >/dev/null && tailscale set --accept-routes=false 2>/dev/null || true

  step "Migrate home Wi-Fi to a native NetworkManager profile"
  if [ -n "${WIFI_SSID:-}" ] && ! nmcli -t -f NAME con show | grep -qx "$WIFI_SSID"; then
    nmcli con add type wifi con-name "$WIFI_SSID" ssid "$WIFI_SSID" \
      wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$WIFI_PSK" connection.autoconnect yes || true
    rm -f /etc/netplan/50-cloud-init.yaml
  fi

  step "Wi-Fi: a USB adapter takes over from the built-in radio"
  # pifin-wifi-select (before comitup) picks the radio: a USB Wi-Fi adapter when one
  # is plugged in, else the built-in one. So no network profile may be tied to an
  # interface name (older PiFin bound the home Wi-Fi to wlan0), and all use the
  # standard MTU (some had 1280, which fragments every full-size Tailscale packet).
  for u in $(nmcli -t -f UUID,TYPE con show | sed -n 's/:802-11-wireless$//p'); do
    case "$(nmcli -g 802-11-wireless.mode con show "$u")" in ap|adhoc|mesh) continue ;; esac
    nmcli con modify "$u" connection.interface-name "" 802-11-wireless.mtu 0 || true
  done

  step "SSH: keys only once a key is installed"
  if [ -s "$USER_HOME/.ssh/authorized_keys" ]; then
    grep -q '^PasswordAuthentication' /etc/ssh/sshd_config.d/01-pifin.conf ||
      echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config.d/01-pifin.conf
  else
    echo "no SSH key installed (SSH_PUBKEY): password login stays as it is"
  fi

  step "Faster boot, firmware part (from the next boot on)"
  # no camera/DSI display probing (~0.8 s); no initramfs: the firmware reads the SD card
  # at ~5 MB/s, so its 12 MB cost 2.3 s, plus ~1 s in the kernel. Raspberry Pi kernels
  # have SD and ext4 built in. Only now: this first boot has grown the root partition.
  sed -i -e 's/^camera_auto_detect=1/camera_auto_detect=0/' \
         -e 's/^display_auto_detect=1/display_auto_detect=0/' \
         -e 's/^auto_initramfs=1/auto_initramfs=0/' /boot/firmware/config.txt

  step "Scrub secrets"
  rm -f /var/lib/cloud/instance/user-data.txt* /var/lib/cloud/instances/*/user-data.txt* 2>/dev/null || true
  printf '#cloud-config\n' > /boot/firmware/user-data
  sed -i 's/^\(\s*password:\).*/\1 "<removed>"/' /boot/firmware/network-config 2>/dev/null || true
  sed -i '/^TS_AUTHKEY=/d;/^WIFI_PSK=/d' /etc/pifin/secrets.env
  touch /etc/cloud/cloud-init.disabled
}

main() {
  local mode
  mode=$(pick_mode "${1:-}") || { echo "usage: pifin-setup.sh [install|configure|all]" >&2; exit 2; }
  exec > >(tee -a /var/log/pifin-setup.log) 2>&1
  echo "=== PiFin setup ($mode) start $(date) ==="
  case "$mode" in
    install) install_phase; echo "=== PiFin install done $(date) ==="; return 0 ;;
    all) install_phase ;;
  esac
  configure_phase
  echo "=== PiFin setup done $(date), rebooting ==="
  touch /etc/pifin/setup-done
  systemctl disable pifin-setup.service
  systemctl --no-block reboot
}

# tools/test_setup_phases.py sources the functions without running anything
[ -n "${PIFIN_SETUP_SOURCE_ONLY:-}" ] || main "$@"
