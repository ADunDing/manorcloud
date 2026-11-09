#!/bin/sh
# Build an installation package: the frontend, the cross-compiled binary and the deployment
# files, in one archive.
#
# Usage:
#   ./package.sh                               build for ARM64 (Raspberry Pi, RK3528, ...)
#   ./package.sh x86_64-unknown-linux-gnu      build for x86_64
#
# Produces dist/manorcloud-<version>-<arch>.tar.gz. On the target machine, extract it and run
# sudo ./install.sh. No source code, Rust or Node is needed there.
set -eu

# Always work from the project root, wherever this is run from
cd "$(dirname "$0")"

TARGET=${1:-aarch64-unknown-linux-gnu}

# The oldest glibc the binaries may require. Raising it drops support for older systems
# (2.34: Ubuntu 22.04, Debian 12, Raspberry Pi OS bookworm and later), so it is checked below
# and must only change on purpose. install.sh checks the same version on the target machine.
MAX_GLIBC=2.34

# Version from the first "version" in Cargo.toml (the [package] section)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "error: cannot read the version from Cargo.toml" >&2; exit 1; }

case "$TARGET" in
    aarch64-unknown-linux-gnu) ARCH=aarch64 ;;
    x86_64-unknown-linux-gnu) ARCH=x86_64 ;;
    *) echo "error: unsupported target $TARGET (use aarch64- or x86_64-unknown-linux-gnu)" >&2; exit 1 ;;
esac
NAME="manorcloud-$VERSION-$ARCH"

# Package contents
FILES="install.sh export-files.sh manorcloud.service manorcloud.env"

# 1. Frontend: must be built before Rust, because rust-embed bundles it at compile time
npm --prefix frontend ci
npm --prefix frontend run build

# 2. Cross-compile in a Docker container. Node is not needed there: the frontend is built.
#    SQLx checks the queries against .sqlx/ instead of a database: a clean environment such as
#    CI has no app.db. Cross.toml passes this variable into the container and pins the image,
#    which decides the glibc the binary is linked against.
export SQLX_OFFLINE=true
cross build --release --target "$TARGET"

BINARY="target/$TARGET/release/manorcloud"

# 3. The newest glibc version the binary requires is the oldest glibc it can run on.
#    A newer build image or dependency can raise it silently, so stop instead of shipping a
#    binary that fails with "GLIBC_x.y not found" on systems we claim to support.
#
#    Read the version requirements table (readelf -V, "Name: GLIBC_x.y  Flags: ..."): it is
#    what the dynamic loader checks before the program starts. A requirement marked WEAK may be
#    missing; any other must be present. Weak *symbols* do not help: a weak symbol whose version
#    requirement is not weak still stops the program from starting on an older glibc.
required=$(readelf -V "$BINARY" |
    awk '/Name: GLIBC_/ && !/WEAK/ { sub(/.*Name: GLIBC_/, ""); print $1 }' |
    sort -uV | tail -1)
[ -n "$required" ] || { echo "error: cannot read the glibc requirements of $BINARY" >&2; exit 1; }
newest=$(printf '%s\n%s\n' "$required" "$MAX_GLIBC" | sort -V | tail -1)
if [ "$newest" != "$MAX_GLIBC" ]; then
    echo "error: $BINARY requires glibc $required, newer than the supported $MAX_GLIBC" >&2
    echo "the symbols that require it:" >&2
    objdump -T "$BINARY" | grep "GLIBC_$required" >&2
    exit 1
fi
echo "requires glibc $required (supported: $MAX_GLIBC and later)"

# 4. Assemble the package
rm -rf "dist/$NAME"
mkdir -p "dist/$NAME"
cp "$BINARY" "dist/$NAME/"
for file in $FILES; do
    cp "deploy/$file" "dist/$NAME/"
done
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"

# 5. Check that the package is complete: a missing file is much worse to discover on the
#    target machine. grep -x matches whole lines, so "manorcloud" never matches manorcloud.env
for file in manorcloud $FILES; do
    tar -tzf "dist/$NAME.tar.gz" | grep -qx "$NAME/$file" ||
        { echo "error: the package is missing $file" >&2; exit 1; }
done

echo
echo "done: dist/$NAME.tar.gz"
ls -lh "dist/$NAME.tar.gz"
