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

- **Files and folders** in the browser: upload by drag and drop, download, rename, copy, move,
  delete (with a trash you can restore from)
- **Real file names on disk**: the files on the server are ordinary files and folders with their
  real names, so you can back them up or copy them out with any tool
- **Large uploads that survive interruptions**: big files are sent in pieces, retried
  automatically when the connection drops, and can be resumed after reloading the page by
  choosing the same file again
- **Resumable downloads** (HTTP range requests)
- **Search** by name in a folder and its subfolders, or everywhere in your files
- **Upload whole folders**, by dragging them in or with New → Upload folder; the folder
  structure is kept
- **Share links**: share a file or folder with people who don't have an account, optionally
  with a password and an expiry date. Recipients can browse and download, nothing else
- **Storage limits**: an administrator can limit how much space each member uses (their files
  plus their trash)
- **English and Traditional Chinese**: follows the browser's language, or choose one in
  Settings → Appearance
- **Preview in the browser**: pictures, videos, music, PDFs and text files open full screen; use
  ← → to go through the folder. Pictures get thumbnails, made once and kept in
  `.manorcloud/thumbs` on each disk; ones not viewed for 30 days are removed automatically.
  Videos play if the browser supports their format (MP4 with H.264 works everywhere)
- **Several users**, managed by an administrator; each user sees only their own files
- **Several disks**: an administrator can keep each user's files on a different disk, and
  moves existing files there in the background
- **Hidden files** (such as `.DS_Store`) are hidden unless you choose to show them in
  **Settings → Appearance**
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

Verify a download: every release file is signed by GitHub, proving it was built from this
repository by GitHub Actions and not changed afterwards. With the
[GitHub CLI](https://cli.github.com/) installed and logged in:

```sh
gh attestation verify manorcloud-linux-aarch64.tar.gz --repo ADunDing/manorcloud
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

### Storage locations

To use more than one disk, add each one in **Administration → Storage**, then choose where each
user's files are kept with **Change storage location** in **Administration → Users**. Existing
files are moved in the background; until that finishes the user can browse and download, but
not change anything.

A storage location is a folder that already exists and that the service can write to:

```sh
sudo mkdir -p /mnt/disk2/manorcloud
sudo chown manorcloud: /mnt/disk2/manorcloud
sudo systemctl edit manorcloud      # add the two lines below
```

```ini
[Service]
ReadWritePaths=/mnt/disk2/manorcloud
```

Then run `sudo systemctl restart manorcloud`. The database always stays on the local disk.

A NAS share (NFS, SMB) works the same way: mount it on this computer first (for example in
`/etc/fstab`), then add the mounted folder as a storage location. If the disk or share isn't
mounted, ManorCloud notices and refuses to write there instead of filling the empty mount point.

Files are kept with their real names and folders. On every disk (the data directory and each
storage location) it looks like this:

```text
/mnt/disk2/manorcloud/
├── .manorcloud/      ManorCloud's own: unfinished uploads and the trash. Don't change it
├── users/
│   └── alice/        alice's My files
└── Photos/           administrators' files
```

An administrator's **My files** is the top level of their disk; every other disk is listed in
the sidebar under **My files**. Administrators can copy or move files between disks: choose
**Copy** or **Move** and pick the disk at the top of the dialog. It runs in the background with
its progress in the bottom-right corner; a move deletes the originals only after everything has
been copied, and it continues after a restart. The **users** folder holds everyone's own files: administrators
can open and download them there, but not change them. Changes made directly on the disk (for
example over a network share) show up in ManorCloud right away.
`manorcloud uninstall --purge` doesn't touch storage locations; delete them yourself if you no
longer need them.

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
| Uninstall, keeping your files              | `sudo manorcloud uninstall`                                     |
| Uninstall and delete everything            | `sudo manorcloud uninstall --purge`                             |

Your files are never locked inside ManorCloud: they are ordinary files with their real names in
the data directory (`/var/lib/manorcloud/data` by default) and on each storage location.

When you upgrade from a version that stored files under random names, ManorCloud moves them to
their real names the first time it starts. Connect every storage location first: if one isn't
available, ManorCloud stops and tells you which one. Nothing is deleted if something goes wrong.

## License

[GNU Affero General Public License v3.0 or later](LICENSE).

You may use, modify and share ManorCloud freely. If you run a modified version as a service for
others, you must make your modified source code available to them.
