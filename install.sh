#!/bin/sh
# ManorCloud installer and upgrader
#
# Install, or upgrade to, the latest release:
#   curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | sh
#
# A specific release:
#   curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | MANORCLOUD_VERSION=v0.1.0 sh
#
# Offline, from an extracted package (installs the manorcloud binary next to this script):
#   MANORCLOUD_SKIP_DOWNLOAD=true ./install.sh
#
# Run it as a normal user: downloading and verifying need no privileges, and only the
# installation itself uses sudo. Safe to run again: a second run is an upgrade. It replaces the
# binary and restarts the service, and keeps the configuration and all data.
#
# Everything is inside main(), which is called on the very last line. "curl | sh" runs the
# script while it is still downloading: if the download is cut off, sh never receives that
# last line and runs nothing, instead of half a script.

set -eu

REPO=ADunDing/manorcloud

# The oldest glibc supported. Must match MAX_GLIBC in package.sh
MIN_GLIBC=2.34

# Installed locations. Must match src/layout.rs, which "manorcloud uninstall" uses
BIN_PATH=/usr/local/bin/manorcloud
CONFIG_DIR=/etc/manorcloud
ENV_FILE=$CONFIG_DIR/manorcloud.env
UNIT_FILE=/etc/systemd/system/manorcloud.service

# The program's listen address when the configuration does not set one.
# Must match DEFAULT_BIND in src/config.rs
DEFAULT_BIND=127.0.0.1:3000

# ==================== Helpers ====================

say() {
    printf '==> %s\n' "$*"
}

fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

has() {
    command -v "$1" >/dev/null 2>&1
}

# Run a command as root: directly when already root, otherwise through sudo
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

# True when version $1 is $2 or newer. sort -V compares version numbers: 2.9 < 2.34
version_at_least() {
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" = "$2" ]
}

# Download $1 to $2 over HTTPS only, also for redirects
fetch() {
    if has curl; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
    elif has wget; then
        wget -q --https-only -O "$2" "$1"
    else
        fail "curl or wget is needed to download ManorCloud"
    fi
}

# ==================== 1. Checks ====================

check_system() {
    os=$(uname -s)
    machine=$(uname -m)

    case "$os" in
        Linux) ;;
        Darwin)
            case "$machine" in
                arm64) fail "macOS is not supported yet" ;;
                *) fail "Intel Macs are not supported" ;;
            esac
            ;;
        *) fail "$os is not supported: ManorCloud runs on Linux" ;;
    esac

    case "$machine" in
        x86_64 | amd64) ARCH=x86_64 ;;
        aarch64 | arm64) ARCH=aarch64 ;;
        armv6l | armv7l | armv8l | armhf | arm)
            fail "32-bit ARM is not supported. Install a 64-bit operating system (on a Raspberry Pi: Raspberry Pi OS 64-bit)"
            ;;
        *) fail "unsupported CPU architecture: $machine" ;;
    esac

    # A 64-bit kernel can run a 32-bit operating system (the default on some Raspberry Pis):
    # uname -m then says aarch64, but there are no 64-bit libraries to run ManorCloud
    if [ "$(getconf LONG_BIT 2>/dev/null)" != 64 ]; then
        fail "this is a 32-bit operating system on a 64-bit kernel. Install a 64-bit operating system (on a Raspberry Pi: Raspberry Pi OS 64-bit)"
    fi

    if [ -f /.dockerenv ] || [ -f /run/.containerenv ]; then
        fail "this looks like a container. Install ManorCloud on the host, which needs systemd"
    fi
    [ -d /run/systemd/system ] || fail "this system does not use systemd"

    # "glibc 2.36" -> "2.36". Empty on systems with another C library, such as Alpine
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{ print $2 }')
    [ -n "$glibc" ] ||
        fail "ManorCloud needs glibc; systems with another C library (such as Alpine Linux) are not supported"
    version_at_least "$glibc" "$MIN_GLIBC" ||
        fail "ManorCloud needs glibc $MIN_GLIBC or later (Ubuntu 22.04, Debian 12, Raspberry Pi OS bookworm or newer). This system has glibc $glibc"

    if [ "$(id -u)" -ne 0 ] && ! has sudo; then
        fail "run as root, or install sudo"
    fi
    for tool in tar od head mktemp; do
        has "$tool" || fail "$tool is needed but not installed"
    done
}

# ==================== 2. The binary ====================

# Download the package for this machine, verify it against SHA256SUMS and extract it
download_package() {
    if [ -n "${MANORCLOUD_VERSION:-}" ]; then
        case "$MANORCLOUD_VERSION" in
            v[0-9]*) ;;
            *) fail "MANORCLOUD_VERSION must look like v0.1.0, got: $MANORCLOUD_VERSION" ;;
        esac
        base="https://github.com/$REPO/releases/download/$MANORCLOUD_VERSION"
        label=$MANORCLOUD_VERSION
    else
        # GitHub redirects this to the newest release that is not a pre-release
        base="https://github.com/$REPO/releases/latest/download"
        label="latest release"
    fi
    asset="manorcloud-linux-$ARCH.tar.gz"

    has sha256sum || fail "sha256sum is needed to verify the download"

    say "Downloading ManorCloud ($label) for $ARCH"
    fetch "$base/$asset" "$WORK/$asset" || fail "cannot download $base/$asset"
    fetch "$base/SHA256SUMS" "$WORK/SHA256SUMS" || fail "cannot download $base/SHA256SUMS"

    # Nothing from the download is used before this check passes.
    #
    # What it protects against: a download that is incomplete or damaged on the way.
    # What it does not: SHA256SUMS comes from the same release as the package, so someone able
    # to replace the package there could replace SHA256SUMS too. That needs an independent
    # signature, such as GitHub's build provenance attestations.
    say "Verifying the checksum"
    grep -E "^[0-9a-f]{64}  $asset\$" "$WORK/SHA256SUMS" >"$WORK/expected" ||
        fail "$asset is not listed in SHA256SUMS"
    (cd "$WORK" && sha256sum -c --quiet expected) ||
        fail "checksum mismatch: the download is damaged. Try again"

    tar -xzf "$WORK/$asset" -C "$WORK"
    BINARY=
    for candidate in "$WORK"/manorcloud-*/manorcloud; do
        BINARY=$candidate
    done
    [ -f "$BINARY" ] || fail "the package does not contain the manorcloud binary"
}

# Offline: use the binary next to this script
use_local_binary() {
    script_dir=$(cd "$(dirname "$0")" && pwd)
    BINARY=$script_dir/manorcloud
    [ -f "$BINARY" ] ||
        fail "MANORCLOUD_SKIP_DOWNLOAD=true installs the manorcloud binary next to install.sh, but $script_dir has none"
}

# Read the ELF header: the first 4 bytes are \177ELF, and the 2 bytes at offset 18 are the
# machine type. This does not need the "file" command, which minimal systems may not have.
check_binary() {
    magic=$(head -c 4 "$BINARY" | od -An -c | tr -d ' ')
    [ "$magic" = "177ELF" ] || fail "$BINARY is not a Linux program"

    case "$ARCH" in
        x86_64) expected=62 ;;
        aarch64) expected=183 ;;
    esac
    actual=$(od -An -t u2 -j 18 -N 2 "$BINARY" | tr -d ' ')
    [ "$actual" = "$expected" ] ||
        fail "$BINARY was built for a different CPU architecture than this machine ($ARCH)"
}

# ==================== 3. Installation ====================

install_files() {
    say "Installing (this needs administrator rights)"

    # The service runs as a dedicated user that cannot log in and has no home directory
    if ! id manorcloud >/dev/null 2>&1; then
        as_root useradd --system --user-group --no-create-home \
            --home-dir /var/lib/manorcloud --shell /usr/sbin/nologin manorcloud
        echo "Created the system user manorcloud."
    fi

    # Install to a temporary name and then rename: a rename is atomic, so replacing the binary
    # while the service is running never fails with "text file busy" or leaves a partial file
    as_root install -m 755 "$BINARY" "$BIN_PATH.new"
    as_root mv -f "$BIN_PATH.new" "$BIN_PATH"

    # Never overwrite an existing configuration: upgrades keep the administrator's changes.
    # Mode 600, owned by root: systemd (as root) reads EnvironmentFile before switching users,
    # and the program itself never reads it, so the manorcloud user needs no access.
    as_root install -d -m 755 "$CONFIG_DIR"
    if [ -f "$ENV_FILE" ]; then
        echo "Keeping the existing $ENV_FILE."
    else
        env_template >"$WORK/manorcloud.env"
        as_root install -m 600 "$WORK/manorcloud.env" "$ENV_FILE"
        echo "Created $ENV_FILE."
    fi
    # Older versions installed it as 640 root:manorcloud; tighten it on every run
    as_root chown root:root "$ENV_FILE"
    as_root chmod 600 "$ENV_FILE"

    # The unit file is always replaced: local changes belong in "systemctl edit" drop-ins
    unit_file >"$WORK/manorcloud.service"
    as_root install -m 644 "$WORK/manorcloud.service" "$UNIT_FILE"
    as_root systemctl daemon-reload
    as_root systemctl enable manorcloud >/dev/null 2>&1
}

start_service() {
    started_at=$(date '+%Y-%m-%d %H:%M:%S')

    # Type=notify: systemctl returns only after the program reports that it is listening,
    # and fails if startup fails. No need to guess how long to wait.
    say "Starting the service"
    if ! as_root systemctl restart manorcloud; then
        as_root journalctl -u manorcloud --since "$started_at" --no-pager -o cat
        fail "the service did not start; see the messages above"
    fi

    echo
    echo "---------- startup log ----------"
    as_root journalctl -u manorcloud --since "$started_at" --no-pager -o cat
    echo "---------------------------------"
}

# The address to open in a browser, from MANORCLOUD_BIND in the configuration.
#
# The file is readable by root only; sudo was just used, so it does not ask again.
# As in systemd, the last setting of a name wins, and the quotes it removes are removed here.
# Without a setting, the program's default applies.
web_address() {
    bind=$(as_root sed -n 's/^[[:space:]]*MANORCLOUD_BIND=//p' "$ENV_FILE" |
        tail -n 1 | tr -d "\"' \t")
    [ -n "$bind" ] || bind=$DEFAULT_BIND

    # The port is after the last colon; the host is everything before it ("[::]" keeps its
    # brackets, which is also how an IPv6 address is written in a URL)
    port=${bind##*:}
    host=${bind%:*}

    case "$host" in
        0.0.0.0 | "[::]")
            # Every address of this machine: show the first one others on the network can use
            ip=$(hostname -I 2>/dev/null | awk '{ print $1 }')
            case "$ip" in
                "") echo "http://<this machine's address>:$port" ;;
                *:*) echo "http://[$ip]:$port" ;;
                *) echo "http://$ip:$port" ;;
            esac
            ;;
        127.0.0.1 | "[::1]" | localhost)
            echo "http://$host:$port (from this machine only)"
            ;;
        *)
            echo "http://$host:$port"
            ;;
    esac
}

print_summary() {
    echo
    echo "$("$BIN_PATH" --version) is running."
    echo "  Address:  $(web_address)"
    echo "  Status:   systemctl status manorcloud"
    echo "  Logs:     journalctl -u manorcloud -f"
    echo "  Settings: $ENV_FILE (then: sudo systemctl restart manorcloud)"
    echo "  Export:   sudo manorcloud export <output-dir>"
    echo "  Remove:   sudo manorcloud uninstall (add --purge to also delete /var/lib/manorcloud)"
}

# ==================== Installed files ====================

unit_file() {
    cat <<'EOF'
# systemd unit for ManorCloud
#
# Written by install.sh, which replaces it on every upgrade. For local changes, use a drop-in
# that upgrades never touch: sudo systemctl edit manorcloud

[Unit]
Description=ManorCloud home cloud storage
After=network-online.target
Wants=network-online.target

[Service]
# notify: the program tells systemd when it is listening.
# Until then, systemctl start/restart waits; a failure during startup is reported as such.
Type=notify
User=manorcloud
Group=manorcloud

# "run" is a hidden command and is not listed in "manorcloud --help":
# the server should only be started by systemd. When run by hand, the program checks for
# INVOCATION_ID (set by systemd) and refuses without it; running as root is always refused.
# To debug in the foreground: sudo systemctl stop manorcloud, then run it as the
# manorcloud user with MANORCLOUD_MANUAL_RUN=1 and the variables from the configuration file.
ExecStart=/usr/local/bin/manorcloud run

# Configuration. systemd (as root) reads it before switching users;
# the program itself never reads this file.
EnvironmentFile=/etc/manorcloud/manorcloud.env

# systemd creates /var/lib/manorcloud owned by manorcloud, unreadable by other users
StateDirectory=manorcloud
StateDirectoryMode=0750
WorkingDirectory=/var/lib/manorcloud
UMask=0027

# Startup runs migrations and, on first install, one Argon2 hash: allow time on slow CPUs
TimeoutStartSec=120
# Watchdog: every 20 seconds the program checks the database and pings systemd if it is healthy.
# One failed check is tolerated; two in a row (no ping for 60 seconds) means it is hung.
WatchdogSec=60

# Stopping: the program finishes requests in progress for up to 30 seconds, then exits.
# This must be longer than that, or systemd kills it before it can clean up.
TimeoutStopSec=45

# Restart after a crash or a watchdog timeout, but not after "systemctl stop"
Restart=on-failure
RestartSec=5

# ---------- Hardening ----------
# Even if the program had a vulnerability, an attacker could do as little as possible.

NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
# AF_UNIX is also used for sd_notify
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
EOF
}

env_template() {
    cat <<'EOF'
# ManorCloud configuration
#
# After editing, run: sudo systemctl restart manorcloud
# The installer never overwrites this file: your changes are kept across upgrades.
#
# Format: one NAME=value per line. This file is read by systemd, not by a shell:
# no "export", no $VARIABLE expansion, and comments must be on their own line.

# Address and port to listen on. 0.0.0.0 lets other devices on your network connect;
# use 127.0.0.1:3000 to allow connections from this machine only.
MANORCLOUD_BIND=0.0.0.0:3000

# Database location. It MUST be on a local disk, never on NFS or another network share:
# SQLite relies on file locking, which is unreliable on network filesystems and can
# corrupt the database.
MANORCLOUD_DATABASE_URL=sqlite:///var/lib/manorcloud/manorcloud.db

# Where uploaded files are stored.
# If you move this outside /var/lib/manorcloud (for example to an external disk), the service
# cannot write there: its systemd unit makes the rest of the system read-only. Allow the new
# path with a drop-in (which upgrades never overwrite):
#   sudo systemctl edit manorcloud
# and add:
#   [Service]
#   ReadWritePaths=/your/new/path
MANORCLOUD_DATA_DIR=/var/lib/manorcloud/data

# Maximum size of a single upload, in bytes (default: 10 GiB). Uncomment to change.
#MANORCLOUD_MAX_UPLOAD_BYTES=10737418240

# Unfinished uploads each user may have at once (default: 5). Uncomment to change.
# An upload that makes no progress for 24 hours is deleted automatically. Together with
# the upload limit, this bounds the disk space that abandoned uploads can take.
#MANORCLOUD_MAX_PENDING_UPLOADS=5

# Send the session cookie over HTTPS only.
# Set to true ONLY after HTTPS is configured: with plain http:// it prevents logging in.
MANORCLOUD_SECURE_COOKIES=false
EOF
}

# ==================== Main ====================

main() {
    check_system

    # All downloads and generated files go here, removed when the script ends
    WORK=$(mktemp -d)
    trap 'rm -rf "$WORK"' EXIT
    trap 'exit 130' INT TERM

    if [ "${MANORCLOUD_SKIP_DOWNLOAD:-}" = true ]; then
        use_local_binary
    else
        download_package
    fi
    check_binary

    install_files
    start_service
    print_summary
}

main "$@"
