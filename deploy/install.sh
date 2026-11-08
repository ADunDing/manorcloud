#!/bin/sh
# ManorCloud installer and upgrader
#
# Usage:
#   sudo ./install.sh                  use the manorcloud binary next to this script
#   sudo ./install.sh <binary>         use the given binary (for testing on a development machine)
#
# Safe to run again: the second run is an upgrade. It replaces the binary and restarts the
# service, and keeps the configuration and all data.
set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
BINARY=${1:-"$SCRIPT_DIR/manorcloud"}

fail() {
    echo "error: $*" >&2
    exit 1
}

# ---------- 1. Checks ----------

[ "$(id -u)" -eq 0 ] || fail "run with sudo"
[ -d /run/systemd/system ] || fail "this system does not use systemd"
[ -f "$BINARY" ] || fail "binary not found: $BINARY"
[ -f "$SCRIPT_DIR/manorcloud.service" ] || fail "not found: $SCRIPT_DIR/manorcloud.service"
[ -f "$SCRIPT_DIR/manorcloud.env" ] || fail "not found: $SCRIPT_DIR/manorcloud.env"

# ---------- 2. CPU architecture of the binary ----------
# Read the ELF header: the first 4 bytes are \177ELF, and the 2 bytes at offset 18 are the
# machine type. This does not need the "file" command, which minimal systems may not have.

magic=$(head -c 4 "$BINARY" | od -An -c | tr -d ' ')
[ "$magic" = "177ELF" ] || fail "$BINARY is not a Linux executable"

case "$(uname -m)" in
    x86_64) expected=62 ;;
    aarch64) expected=183 ;;
    *) fail "unsupported CPU architecture: $(uname -m)" ;;
esac
actual=$(od -An -t u2 -j 18 -N 2 "$BINARY" | tr -d ' ')
[ "$actual" = "$expected" ] ||
    fail "the binary was built for a different CPU architecture than this machine ($(uname -m))"

# ---------- 3. System user ----------
# The service runs as a dedicated user that cannot log in and has no home directory.

if ! id manorcloud >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home \
        --home-dir /var/lib/manorcloud --shell /usr/sbin/nologin manorcloud
    echo "Created the system user manorcloud."
fi

# ---------- 4. Binary ----------
# Install to a temporary name and then rename: a rename is atomic, so replacing the binary
# while the service is running never fails with "text file busy" or leaves a partial file.

install -m 755 "$BINARY" /usr/local/bin/manorcloud.new
mv -f /usr/local/bin/manorcloud.new /usr/local/bin/manorcloud

# ---------- 5. Configuration ----------
# Never overwrite an existing file: upgrades keep the administrator's changes.
# Mode 600, owned by root: systemd (as root) reads EnvironmentFile before switching users,
# and the program itself never reads it, so the manorcloud user needs no access.

install -d -m 755 /etc/manorcloud
if [ -f /etc/manorcloud/manorcloud.env ]; then
    echo "Keeping the existing /etc/manorcloud/manorcloud.env."
else
    install -m 600 "$SCRIPT_DIR/manorcloud.env" /etc/manorcloud/manorcloud.env
    echo "Created /etc/manorcloud/manorcloud.env."
fi
# Older versions installed it as 640 root:manorcloud; tighten it on every run
chown root:root /etc/manorcloud/manorcloud.env
chmod 600 /etc/manorcloud/manorcloud.env

# ---------- 6. Service ----------

install -m 644 "$SCRIPT_DIR/manorcloud.service" /etc/systemd/system/manorcloud.service
systemctl daemon-reload
systemctl enable manorcloud >/dev/null 2>&1

started_at=$(date '+%Y-%m-%d %H:%M:%S')

# Type=notify: systemctl returns only after the program reports that it is listening,
# and fails if startup fails. No need to guess how long to wait.
if ! systemctl restart manorcloud; then
    journalctl -u manorcloud --since "$started_at" --no-pager -o cat
    fail "the service did not start; see the messages above"
fi

# ---------- 7. Done ----------

echo
echo "---------- startup log ----------"
journalctl -u manorcloud --since "$started_at" --no-pager -o cat
echo "---------------------------------"
echo
echo "ManorCloud is running."
ip=$(hostname -I 2>/dev/null | awk '{print $1}')
if [ -n "$ip" ]; then
    echo "  Address:  http://$ip:3000 (use your port if you changed MANORCLOUD_BIND)"
fi
echo "  Status:   systemctl status manorcloud"
echo "  Logs:     journalctl -u manorcloud -f"
echo "  Settings: /etc/manorcloud/manorcloud.env (then: sudo systemctl restart manorcloud)"
echo "  Export:   sudo manorcloud export <output-dir>"
echo "  Remove:   sudo manorcloud uninstall (add --purge to also delete /var/lib/manorcloud)"
