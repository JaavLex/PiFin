#!/bin/bash
# PiFin first-boot setup. Run by pifin-setup.service at every boot until
# /etc/pifin/setup-done exists, so it resumes after a power loss. Idempotent.
#
# Result: Raspberry Pi 4 that boots straight into Jellyfin Desktop (TV layout,
# GPU accelerated, fitted to the connected screen), on your Tailscale network,
# TV remote via HDMI-CEC, Wi-Fi setup hotspot when no known network is around.
#
# Reads /etc/pifin/secrets.env (written by build.py from pifin.env):
#   PIFIN_HOSTNAME, PIFIN_USER, WIFI_COUNTRY, HOTSPOT_SSID, HOTSPOT_PASS,
#   JELLYFIN_URL, and until setup finishes WIFI_SSID/WIFI_PSK/TS_AUTHKEY
set -uo pipefail
exec > >(tee -a /var/log/pifin-setup.log) 2>&1
echo "=== PiFin setup start $(date) ==="

source /etc/pifin/secrets.env
USER_NAME=${PIFIN_USER:-pifin}
USER_HOME=/home/$USER_NAME
JD_APP=org.jellyfin.JellyfinDesktop
export DEBIAN_FRONTEND=noninteractive

step() { echo; echo ">>> $*"; }
retry() { local n=0; until "$@"; do n=$((n+1)); [ $n -ge 5 ] && return 1; echo "retry $n: $*"; sleep 15; done; }

step "Persistent logs (capped)"
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/90-pifin-persistent.conf <<'EOF'
# PiFin: keep journalctl -u comitup / -u NetworkManager after a reboot.
[Journal]
Storage=persistent
SystemMaxUse=50M
EOF
systemctl restart systemd-journald   # flushes the current runtime journal into /var/log/journal

step "Wi-Fi country + unblock"
if [ -n "${WIFI_COUNTRY:-}" ]; then
  raspi-config nonint do_wifi_country "$WIFI_COUNTRY" || true
else
  echo "WIFI_COUNTRY not set: keeping the OS regulator default"
fi
rfkill unblock wifi || true

step "Waiting for internet"
for i in $(seq 1 120); do
  curl -fsI --max-time 5 https://pkgs.tailscale.com >/dev/null && break
  sleep 5
done

step "Repair any interrupted package install"
dpkg --configure -a || true
apt-get install -f -y || true

step "apt update"
retry apt-get update

step "Tailscale"
if [ -z "${TS_AUTHKEY:-}" ]; then
  echo "no TS_AUTHKEY in /etc/pifin/secrets.env: skipping Tailscale (it is optional)"
else
  if ! command -v tailscale >/dev/null; then
    retry bash -c 'curl -fsSL https://tailscale.com/install.sh | sh'
  fi
  systemctl enable --now tailscaled
  if tailscale status >/dev/null 2>&1; then
    echo "already logged in to Tailscale"
  else
    retry tailscale up --authkey="$TS_AUTHKEY" --hostname="${PIFIN_HOSTNAME:-pifin}" --accept-routes=false
  fi
fi

step "Packages"
# cage: kiosk Wayland compositor. xwayland + matchbox: X11 layer (GPU path for
# the Flatpak, see pifin-jellyfin-session). wlr-randr + jq: fit the screen.
# evdev + cec-utils: TV remote bridge. pipewire: HDMI audio for the Flatpak.
# websocket + qrcode: Wi-Fi setup screen. mpv: playback with the Pi's hardware
# decoders (Jellyfin Desktop hands videos over to it, see pifin-remote).
# iw: check Wi-Fi power save from the field.
retry apt-get install -y flatpak cage xwayland matchbox-window-manager wlr-randr jq iw \
  python3-evdev cec-utils pipewire pipewire-pulse wireplumber \
  python3-websocket python3-qrcode unattended-upgrades curl mpv
if ! dpkg -s comitup >/dev/null 2>&1 && ! retry apt-get install -y comitup; then
  echo "comitup not in default repos, adding upstream repo"
  curl -fsSL -o /tmp/comitup-src.deb https://davesteele.github.io/comitup/deb/davesteele-comitup-apt-source_1.2_all.deb
  dpkg -i /tmp/comitup-src.deb && apt-get update && retry apt-get install -y comitup
fi

step "Jellyfin Desktop (Flathub)"
flatpak remote-add --if-not-exists --system flathub https://dl.flathub.org/repo/flathub.flatpakrepo
retry flatpak install -y --system --noninteractive flathub "$JD_APP"

step "Jellyfin Desktop pre-config (server address, TV layout, transcoding)"
# The Flatpak player can't use the Pi's hardware decoders: HEVC, 10-bit, AV1
# and 4K get converted to H.264 by the server, which the Pi decodes fine.
JD_DATA="$USER_HOME/.var/app/$JD_APP/data/jellyfin-desktop"
if [ ! -f "$JD_DATA/profiles.json" ]; then
  PROFILE=$(cat /proc/sys/kernel/random/uuid | tr -d -)
  mkdir -p "$JD_DATA/profiles/$PROFILE"
  printf '{\n    "defaultProfile": "%s"\n}\n' "$PROFILE" > "$JD_DATA/profiles.json"
  printf '{\n    "name": "Default"\n}\n' > "$JD_DATA/profiles/$PROFILE/profile.json"
  cat > "$JD_DATA/profiles/$PROFILE/jellyfin-desktop.conf" <<EOF
{
    "sections": {
        "main": {
            "userWebClient": "${JELLYFIN_URL}",
            "layout": "tv",
            "fullscreen": true,
            "checkForUpdates": false
        },
        "cec": {
            "enable": false
        },
        "video": {
            "force_transcode_hevc": true,
            "force_transcode_hi10p": true,
            "force_transcode_av1": true,
            "force_transcode_4k": true,
            "force_transcode_dovi": true
        }
    },
    "version": 7
}
EOF
fi
chown -R "$USER_NAME:$USER_NAME" "$USER_HOME"

step "Comitup config (Wi-Fi setup hotspot)"
cat > /etc/comitup.conf <<EOF
ap_name: ${HOTSPOT_SSID:-PiFin-setup}
ap_password: '${HOTSPOT_PASS}'
web_service:
enable_appliance_mode: false
external_callback: /usr/local/bin/pifin-comitup-callback
EOF
# comitup runs its own dnsmasq instance
systemctl disable --now dnsmasq 2>/dev/null || true
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
systemctl reload NetworkManager || true        # reloads conf.d, does not touch the link
iw dev wlan0 set power_save off 2>/dev/null || true   # best effort for the current association

step "Comitup: retry the known networks 1 min after a failed attempt"
# comitup's FAIL_TIMEOUT is hard-coded: it is how long comitup waits in HOTSPOT
# after a failed CONNECTING sweep before it tries the known networks again.
CS=/usr/share/comitup/comitup/states.py
if [ -f "$CS" ] && ! grep -q "^FAIL_TIMEOUT = 60" "$CS"; then
  if sed -i "s/^FAIL_TIMEOUT = 360.*/FAIL_TIMEOUT = 60  # PiFin: retry a minute after a failed sweep/" "$CS" \
     && python3 -m py_compile "$CS"; then
    if grep -q "PiFin" "$CS"; then
      rm -rf /usr/share/comitup/comitup/__pycache__
      systemctl try-restart comitup || true
    else
      echo "comitup tweak not applied: states.py does not match the expected line, leaving it untouched"
    fi
  else
    echo "comitup's states.py does not match the expected line: leaving it untouched"
  fi
fi

step "Kiosk, TV remote (CEC) and phone remote services"
usermod -aG video,render,input,audio,tty,netdev "$USER_NAME"
systemctl daemon-reload
systemctl enable jellyfin-kiosk.service pifin-cec-remote.service pifin-remote.service
systemctl set-default multi-user.target

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
systemctl daemon-reload

step "Migrate home Wi-Fi to a native NetworkManager profile"
if [ -n "${WIFI_SSID:-}" ] && ! nmcli -t -f NAME con show | grep -qx "$WIFI_SSID"; then
  nmcli con add type wifi ifname wlan0 con-name "$WIFI_SSID" ssid "$WIFI_SSID" \
    wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$WIFI_PSK" connection.autoconnect yes || true
  rm -f /etc/netplan/50-cloud-init.yaml
fi

step "Scrub secrets"
rm -f /var/lib/cloud/instance/user-data.txt* /var/lib/cloud/instances/*/user-data.txt* 2>/dev/null || true
printf '#cloud-config\n' > /boot/firmware/user-data
sed -i 's/^\(\s*password:\).*/\1 "<removed>"/' /boot/firmware/network-config 2>/dev/null || true
sed -i '/^TS_AUTHKEY=/d;/^WIFI_PSK=/d' /etc/pifin/secrets.env
touch /etc/cloud/cloud-init.disabled

echo "=== PiFin setup done $(date), rebooting ==="
touch /etc/pifin/setup-done
systemctl disable pifin-setup.service
systemctl --no-block reboot
