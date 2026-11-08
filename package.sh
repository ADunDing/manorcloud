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

# Version from the first "version" in Cargo.toml (the [package] section)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$VERSION" ] || { echo "error: cannot read the version from Cargo.toml" >&2; exit 1; }

case "$TARGET" in
    aarch64-*) ARCH=aarch64 ;;
    x86_64-*) ARCH=x86_64 ;;
    *) echo "error: unsupported target $TARGET" >&2; exit 1 ;;
esac
NAME="manorcloud-$VERSION-$ARCH"

# Package contents
FILES="install.sh export-files.sh manorcloud.service manorcloud.env"

# 1. Frontend: must be built before Rust, because rust-embed bundles it at compile time
npm --prefix frontend ci
npm --prefix frontend run build

# 2. Cross-compile in a Docker container. Node is not needed there: the frontend is built
cross build --release --target "$TARGET"

# 3. Assemble the package
rm -rf "dist/$NAME"
mkdir -p "dist/$NAME"
cp "target/$TARGET/release/manorcloud" "dist/$NAME/"
for file in $FILES; do
    cp "deploy/$file" "dist/$NAME/"
done
tar -C dist -czf "dist/$NAME.tar.gz" "$NAME"

# 4. Check that the package is complete: a missing file is much worse to discover on the
#    target machine. grep -x matches whole lines, so "manorcloud" never matches manorcloud.env
for file in manorcloud $FILES; do
    tar -tzf "dist/$NAME.tar.gz" | grep -qx "$NAME/$file" ||
        { echo "error: the package is missing $file" >&2; exit 1; }
done

echo
echo "done: dist/$NAME.tar.gz"
ls -lh "dist/$NAME.tar.gz"
