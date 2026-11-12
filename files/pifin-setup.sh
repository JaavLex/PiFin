#!/bin/bash
# PiFin first-boot setup. Run by pifin-setup.service at every boot until
# /etc/pifin/setup-done exists, so it resumes after a power loss. Idempotent.
#
# Result: Raspberry Pi 4 that boots straight into the PiFin app (a native
# Jellyfin browser under cage; playback in mpv with hardware decoding), on your
# Tailscale network if configured, TV remote via HDMI-CEC, phone remote on
# port 8080, Wi-Fi setup hotspot when no known network is around.
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
PIFIN_RELEASE=${PIFIN_RELEASE:-https://github.com/JaavLex/PiFin/releases/latest/download/pifin-aarch64}
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
# cage: kiosk Wayland compositor. wlr-randr + jq: fit the screen. mpv: playback
# with the Pi's hardware decoders. evdev + cec-utils: TV remote bridge.
# pipewire: HDMI audio. iw: check Wi-Fi power save from the field.
retry apt-get install -y cage wlr-randr jq iw mpv fonts-dejavu-core \
  python3-evdev cec-utils pipewire pipewire-pulse wireplumber \
  unattended-upgrades curl
if ! dpkg -s comitup >/dev/null 2>&1 && ! retry apt-get install -y comitup; then
  echo "comitup not in default repos, adding upstream repo"
  curl -fsSL -o /tmp/comitup-src.deb https://davesteele.github.io/comitup/deb/davesteele-comitup-apt-source_1.2_all.deb
  dpkg -i /tmp/comitup-src.deb && apt-get update && retry apt-get install -y comitup
fi

step "PiFin app (prebuilt aarch64 binary from the GitHub release)"
if [ ! -x /usr/local/bin/pifin ]; then
  retry curl -fsSL -o /tmp/pifin "$PIFIN_RELEASE" && install -m 0755 /tmp/pifin /usr/local/bin/pifin
fi
/usr/local/bin/pifin --version 2>/dev/null || ls -la /usr/local/bin/pifin
mkdir -p "$USER_HOME/.config/pifin"
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

step "App, TV remote (CEC) and phone remote services"
usermod -aG video,render,input,audio,tty,netdev "$USER_NAME"
systemctl daemon-reload
systemctl enable pifin-app.service pifin-cec-remote.service pifin-remote.service pifin-wifi-watchdog.service
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
systemctl daemon-reload

step "Migrate home Wi-Fi to a native NetworkManager profile"
if [ -n "${WIFI_SSID:-}" ] && ! nmcli -t -f NAME con show | grep -qx "$WIFI_SSID"; then
  nmcli con add type wifi ifname wlan0 con-name "$WIFI_SSID" ssid "$WIFI_SSID" \
    wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$WIFI_PSK" connection.autoconnect yes || true
  rm -f /etc/netplan/50-cloud-init.yaml
fi

step "Security: firewall, SSH keys only, services without root"
# The Pi joins networks you don't control and is a member of your tailnet.
# pifin-firewall: see the script; the tailnet ACL in the README is the real fence.
systemctl enable pifin-firewall.service
udevadm control --reload || true   # uinput for group "input" (phone/TV remote as $USER_NAME)
# SSH: password login off as soon as there is a key to log in with
if [ -s "$USER_HOME/.ssh/authorized_keys" ]; then
  grep -q '^PasswordAuthentication' /etc/ssh/sshd_config.d/01-pifin.conf ||
    echo 'PasswordAuthentication no' >> /etc/ssh/sshd_config.d/01-pifin.conf
else
  echo "no SSH key installed (SSH_PUBKEY): password login stays on"
fi
# never route into the server's home LAN, even if it advertises one
command -v tailscale >/dev/null && tailscale set --accept-routes=false || true

step "Faster boot (from the next boot on)"
# Measured on a Pi 4: ~7 s less from power-on to the first frame.
# - swap: compressed RAM only; the default zram+file re-created a 2 GB swap
#   file on the SD card at every boot (~1.4 s before /tmp could mount)
mkdir -p /etc/rpi/swap.conf.d
printf '[Main]\nMechanism=zram\n' > /etc/rpi/swap.conf.d/90-pifin.conf
# - firmware: no camera/DSI display probing (~0.8 s); no initramfs: the firmware
#   reads the SD card at ~5 MB/s, so its 12 MB cost 2.3 s, plus ~1 s in the
#   kernel. Raspberry Pi kernels have SD and ext4 built in. Done last, once
#   first-boot setup no longer needs it.
sed -i -e 's/^camera_auto_detect=1/camera_auto_detect=0/' \
       -e 's/^display_auto_detect=1/display_auto_detect=0/' \
       -e 's/^auto_initramfs=1/auto_initramfs=0/' /boot/firmware/config.txt

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
