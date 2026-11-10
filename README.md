# ManorCloud

Lightweight self-hosted cloud storage for Raspberry Pi and other low-power devices.

[![CI](https://github.com/ADunDing/manorcloud/actions/workflows/ci.yml/badge.svg)](https://github.com/ADunDing/manorcloud/actions/workflows/ci.yml)
[![License: AGPL-3.0-or-later](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue.svg)](LICENSE)

ManorCloud is a private file storage service for a household: everyone gets their own space,
reachable from any browser on your home network. It is a single program with no runtime
dependencies, and it runs comfortably on a Raspberry Pi.

> **Status: early development.** ManorCloud works and is tested, but it is young.
> Keep a separate backup of anything important, and expect changes between releases.

## Features

- **Files and folders** in the browser: upload by drag and drop, download, rename, move, delete
- **Large uploads that survive interruptions**: big files are sent in pieces, retried
  automatically when the connection drops, and can be resumed after reloading the page by
  choosing the same file again
- **Resumable downloads** (HTTP range requests)
- **Several users**, managed by an administrator; each user sees only their own files
- **Same name, no surprises**: uploading a file whose name is taken keeps both
  (`photo.jpg`, `photo (1).jpg`)
- **Light**: about 3 MB to download, a few MB of memory when idle
- **Careful with your data**: everything is written to disk before it is reported as saved,
  and an interrupted upload never leaves a damaged file behind
- **A single binary**: the web interface and the database migrations are built in

## Requirements

- 64-bit Linux on x86_64 or ARM64
- systemd
- glibc 2.34 or later: Ubuntu 22.04, Debian 12, Raspberry Pi OS bookworm (64-bit) or newer

32-bit systems are not supported. On a Raspberry Pi, use **Raspberry Pi OS (64-bit)**.

## Install

```sh
curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | sh
```

Run it as a normal user: the installer downloads the right build for your machine, verifies its
checksum, and only then asks for administrator rights to install it. When it finishes, it prints
the address to open, for example `http://192.168.1.20:3000`.

<details>
<summary>Install a specific version, or install offline</summary>

A specific release:

```sh
curl -fsSL https://github.com/ADunDing/manorcloud/releases/latest/download/install.sh | MANORCLOUD_VERSION=v0.1.0 sh
```

Offline: download `manorcloud-linux-<arch>.tar.gz` from the
[releases page](https://github.com/ADunDing/manorcloud/releases), copy it to the machine, then:

```sh
tar -xzf manorcloud-linux-aarch64.tar.gz
cd manorcloud-*-aarch64
MANORCLOUD_SKIP_DOWNLOAD=true ./install.sh
```

</details>

### First login

On the first start, ManorCloud creates an administrator account named `admin` with a random
temporary password and prints it in the installer's output. You must choose a new username and
password when you first log in.

Missed the password? Restart the service to get a new one, as long as nobody has logged in yet:

```sh
sudo systemctl restart manorcloud
journalctl -u manorcloud -n 20 --no-pager
```

## Configuration

Settings are in `/etc/manorcloud/manorcloud.env`, which explains each option. After changing it:

```sh
sudo systemctl restart manorcloud
```

| Setting                          | Default                                      | Meaning                                       |
|----------------------------------|----------------------------------------------|-----------------------------------------------|
| `MANORCLOUD_BIND`                | `0.0.0.0:3000`                               | Address and port to listen on                 |
| `MANORCLOUD_DATA_DIR`            | `/var/lib/manorcloud/data`                   | Where uploaded files are stored               |
| `MANORCLOUD_DATABASE_URL`        | `sqlite:///var/lib/manorcloud/manorcloud.db` | The database; must be on a local disk         |
| `MANORCLOUD_MAX_UPLOAD_BYTES`    | 10 GiB                                       | Largest single upload                         |
| `MANORCLOUD_MAX_PENDING_UPLOADS` | `5`                                          | Unfinished uploads each user may have at once |
| `MANORCLOUD_SECURE_COOKIES`      | `false`                                      | Set to `true` only when serving over HTTPS    |

To store files on an external disk, change `MANORCLOUD_DATA_DIR` and allow the service to write
there; the comments in the configuration file show how.

### Security

ManorCloud serves plain HTTP and is meant for your home network. **Do not expose it to the
internet** without HTTPS in front of it (for example a reverse proxy such as Caddy). Once HTTPS
is set up, set `MANORCLOUD_SECURE_COOKIES=true`.

## Everyday tasks

| Task                                       | Command                                                         |
|--------------------------------------------|-----------------------------------------------------------------|
| Upgrade                                    | Run the install command again. Your settings and files are kept |
| Status                                     | `systemctl status manorcloud`                                   |
| Logs                                       | `journalctl -u manorcloud -f`                                   |
| Export all files with their original names | `sudo manorcloud export <output-dir>`                           |
| Uninstall, keeping your files              | `sudo manorcloud uninstall`                                     |
| Uninstall and delete everything            | `sudo manorcloud uninstall --purge`                             |

`export` writes every user's files to `<output-dir>/<username>/` with their original names,
folders and modification times. It needs no network and no other software, so your files are
never locked inside ManorCloud.

## License

[GNU Affero General Public License v3.0 or later](LICENSE).

You may use, modify and share ManorCloud freely. If you run a modified version as a service for
others, you must make your modified source code available to them.
