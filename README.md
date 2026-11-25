# ManorCloud

Lightweight self-hosted cloud storage for Raspberry Pi and other low-power devices.

[![CI](https://github.com/ADunDing/manorcloud/actions/workflows/ci.yml/badge.svg)](https://github.com/ADunDing/manorcloud/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)

A private file storage for a household: everyone gets their own space, reachable from any
browser. A single program with no dependencies that runs comfortably on a Raspberry Pi.

> **Status: early development.** Keep a separate backup of anything important.

## Requirements

64-bit Linux (x86_64 or ARM64) with systemd and glibc 2.34 or later: Ubuntu 22.04, Debian 12,
Raspberry Pi OS (64-bit) bookworm or newer. Or anything that runs Docker.

## Install

```sh
curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | sh
```

The installer prints the address to open and the first administrator's password (`admin`,
changed at first login). Missed it? `sudo systemctl restart manorcloud`, then
`journalctl -u manorcloud -n 20`.

<details>
<summary>A specific version, offline install, verifying a download</summary>

```sh
# A specific version
curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | MANORCLOUD_VERSION=v0.1.0 sh

# Offline: download manorcloud-linux-<arch>.tar.gz from the releases page, then
tar -xzf manorcloud-linux-aarch64.tar.gz
cd manorcloud-*-aarch64
MANORCLOUD_SKIP_DOWNLOAD=true ./install.sh

# Every release file is signed by GitHub (needs the GitHub CLI)
gh attestation verify manorcloud-linux-aarch64.tar.gz --repo ADunDing/manorcloud
```

</details>

### Docker

```sh
docker run -d --name manorcloud --restart unless-stopped --stop-timeout 35 \
  -p 3000:3000 -v manorcloud:/var/lib/manorcloud ghcr.io/adunding/manorcloud
docker logs manorcloud
```

Or with Docker Compose, in `compose.yaml`:

```yaml
services:
  manorcloud:
    image: ghcr.io/adunding/manorcloud
    restart: unless-stopped
    stop_grace_period: 35s
    ports:
      - "3000:3000"
    volumes:
      - manorcloud:/var/lib/manorcloud
      # - /mnt/disk2:/mnt/disk2

volumes:
  manorcloud:
```

```sh
docker compose up -d
docker compose logs manorcloud
```

The log shows the first administrator's password. Settings below go in as `-e NAME=value`
(Compose: `environment:`). To upgrade, `docker compose pull && docker compose up -d`, or pull and
recreate the container; your files stay in the `manorcloud` volume. For more disks, mount them
into the container and give them to its user with `sudo chown 65532:65532 /mnt/disk2`.

## Configuration

Almost everything is set in **Administration** in the browser. Only these are in
`/etc/manorcloud/manorcloud.env` (then `sudo systemctl restart manorcloud`):

| Setting                   | Default                                      | Meaning                                    |
|---------------------------|----------------------------------------------|--------------------------------------------|
| `MANORCLOUD_BIND`         | `127.0.0.1:3000`                             | Use `0.0.0.0:3000` for other devices       |
| `MANORCLOUD_DATA_DIR`     | `/var/lib/manorcloud/data`                   | Where files are stored                     |
| `MANORCLOUD_DATABASE_URL` | `sqlite:///var/lib/manorcloud/manorcloud.db` | Must be on a local disk                    |
| `MANORCLOUD_TLS_CERT`     | (none)                                       | PEM certificate, to serve HTTPS directly   |
| `MANORCLOUD_TLS_KEY`      | (none)                                       | Its private key                            |

### More disks

Mount the disk under `/mnt`, `/media` or `/srv`, give it to ManorCloud, then add `/mnt/disk2` in
**Administration → Storage**:

```sh
sudo chown manorcloud: /mnt/disk2
```

Each person's files can then be moved there in **Administration → Users**. If a disk isn't
mounted, ManorCloud refuses to write there instead of filling the empty folder.

### HTTPS

ManorCloud is meant for your home network. Before exposing it to the internet, put a reverse
proxy such as Caddy or nginx in front of it and enter the proxy's address in
**Administration → Reverse proxy**, or set the two TLS settings above.

### WebDAV

Turn it on in **Administration → Storage**, create an app password in **Settings → WebDAV**, and
connect to `https://your-server/dav/` with your username and that password.

- **macOS**: Finder → Go → Connect to Server (⌘K)
- **Windows**: Map network drive. Needs HTTPS, and files are limited to 50 MB unless you raise
  `FileSizeLimitInBytes` in the registry
- **rclone**: storage type `webdav`, vendor `owncloud` (keeps modification times)

## Everyday tasks

| Task                    | Command                                             |
|-------------------------|-----------------------------------------------------|
| Upgrade                 | Run the install command again; nothing is lost      |
| Status / logs           | `systemctl status manorcloud` / `journalctl -u manorcloud -f` |
| Uninstall, keep files   | `sudo manorcloud uninstall`                         |
| Uninstall, delete all   | `sudo manorcloud uninstall --purge`                 |

## License

[AGPL-3.0-or-later](LICENSE): use, modify and share freely. If you run a modified version as a
service for others, make your source code available to them.
