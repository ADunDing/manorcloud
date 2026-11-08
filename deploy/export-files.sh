#!/bin/sh
# Export all files with their original names and folders, WITHOUT ManorCloud.
#
# This is the last-resort fallback: normally use "sudo manorcloud export <output-dir>",
# which needs nothing else installed. Use this script only if the manorcloud binary itself
# cannot run. It needs the sqlite3 command (sudo apt install sqlite3).
#
# Usage:
#   sudo ./export-files.sh <output-dir>
#       read the database and data locations from /etc/manorcloud/manorcloud.env
#
#   ./export-files.sh --db <database> --data <data-dir> <output-dir>
#       give the locations explicitly, e.g. to export from a backup
#
# Output: <output-dir>/<username>/<original path>, with modification times restored.
# Only reads: the database is opened read-only and blobs are copied, never moved.
set -eu

ENV_FILE=/etc/manorcloud/manorcloud.env
# The service's WorkingDirectory: relative paths in the configuration are relative to it
STATE_DIR=/var/lib/manorcloud

fail() {
    echo "error: $*" >&2
    exit 1
}

usage() {
    echo "usage: $0 [--db <database>] [--data <data-dir>] <output-dir>"
    echo "Without --db or --data, the locations are read from $ENV_FILE."
}

# ---------- 1. Arguments ----------

DB=""
DATA=""
OUT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --db)
            [ $# -ge 2 ] || fail "--db needs a path"
            DB=$2
            shift 2
            ;;
        --data)
            [ $# -ge 2 ] || fail "--data needs a path"
            DATA=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        -*)
            fail "unknown option: $1"
            ;;
        *)
            [ -z "$OUT" ] || fail "only one output directory is allowed"
            OUT=$1
            shift
            ;;
    esac
done
[ -n "$OUT" ] || { usage >&2; exit 1; }

# ---------- 2. Locations from the configuration ----------

# Read one value from the configuration file.
# Never "source" or "." the file: it is in systemd format, not a shell script, and this
# script runs as root, so sourcing it would run anything inside as root.
# Like systemd, the last occurrence of a name wins; surrounding double quotes are removed.
env_value() {
    sed -n "s/^[[:space:]]*$1=//p" "$ENV_FILE" | tail -1 | sed 's/^"\(.*\)"$/\1/'
}

# Relative paths are relative to the service's working directory, as when the service runs
resolve() {
    case "$1" in
        /*) printf '%s' "$1" ;;
        *) printf '%s/%s' "$STATE_DIR" "${1#./}" ;;
    esac
}

if [ -z "$DB" ] || [ -z "$DATA" ]; then
    [ -r "$ENV_FILE" ] || fail "cannot read $ENV_FILE (run with sudo, or use --db and --data)"
fi

if [ -z "$DB" ]; then
    url=$(env_value MANORCLOUD_DATABASE_URL)
    # Same default as the program
    url=${url:-sqlite://app.db}
    path=${url#sqlite://}   # sqlite:///var/lib/x.db -> /var/lib/x.db
    path=${path#sqlite:}    # the sqlite:x.db form
    path=${path%%\?*}       # drop parameters such as ?mode=rwc
    DB=$(resolve "$path")
fi

if [ -z "$DATA" ]; then
    dir=$(env_value MANORCLOUD_DATA_DIR)
    DATA=$(resolve "${dir:-./data}")
fi

# ---------- 3. Checks ----------

command -v sqlite3 >/dev/null 2>&1 || fail "sqlite3 is required (sudo apt install sqlite3)"
[ -f "$DB" ] || fail "database not found: $DB"
[ -d "$DATA/blobs" ] || fail "not found: $DATA/blobs"

mkdir -p "$OUT"
# The output directory must be empty: never mix with or silently overwrite existing files
[ -z "$(ls -A "$OUT")" ] || fail "the output directory is not empty: $OUT"

# Space check: this copies rather than moves, so the output disk needs room for every file
need_kb=$(sqlite3 -readonly "$DB" "SELECT COALESCE(SUM(size), 0) / 1024 + 1 FROM files WHERE kind = 'file';")
free_kb=$(df -Pk "$OUT" | awk 'NR == 2 { print $4 }')
if [ "$need_kb" -gt "$free_kb" ]; then
    fail "not enough space: about $((need_kb / 1024)) MB needed, $((free_kb / 1024)) MB available"
fi

echo "database: $DB"
echo "data directory: $DATA"
echo "output: $OUT (about $((need_kb / 1024)) MB)"
echo

# ---------- 4. Build the full paths ----------
# Walk down from the root folder, joining each name to its parent's path.
# Depth limit 256, as on the server: stops even if corrupted data forms a cycle.
# Fields are separated by tabs: file names cannot contain control characters (including
# tab and newline), so they can never be confused with a separator.
# Folders have no storage_key: use "-" rather than an empty field, because "read" treats
# consecutive tabs as one separator, which would shift the following fields.

TAB=$(printf '\t')
QUERY="
WITH RECURSIVE tree(id, path, depth) AS (
    SELECT id, name, 0 FROM files WHERE parent_id IS NULL
    UNION ALL
    SELECT f.id, tree.path || '/' || f.name, tree.depth + 1
    FROM files f JOIN tree ON f.parent_id = tree.id
    WHERE tree.depth < 256
)
SELECT users.username, tree.path, files.kind,
       COALESCE(files.storage_key, '-'), files.updated_at
FROM tree
JOIN files ON files.id = tree.id
JOIN users ON users.id = files.owner_id
ORDER BY users.username, tree.path;
"

# ---------- 5. One entry at a time ----------
# -readonly: guarantees the database is not modified

sqlite3 -readonly -separator "$TAB" "$DB" "$QUERY" |
while IFS="$TAB" read -r user path kind key updated; do
    target="$OUT/$user/$path"

    if [ "$kind" = "folder" ]; then
        # Create empty folders too
        mkdir -p "$target"
        continue
    fi

    # Blob location: blobs/<first two characters>/<full key>
    prefix=$(printf '%s' "$key" | cut -c1-2)
    blob="$DATA/blobs/$prefix/$key"

    if [ ! -f "$blob" ]; then
        echo "warning: content not found for $user/$path ($blob)" >&2
        continue
    fi

    # Make sure the parent folder exists
    mkdir -p "$(dirname "$target")"
    # Copy straight to the original name; -- so names starting with - are not options
    cp -- "$blob" "$target"
    # Restore the modification time (updated_at is in Unix seconds)
    touch -d "@$updated" -- "$target"
done

echo "done: exported to $OUT"
