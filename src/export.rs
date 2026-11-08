//! `manorcloud export <輸出目錄>`：不需要啟動伺服器，把所有檔案還原成原本的資料夾結構與檔名。
//!
//! 與 deploy/export-files.sh 做相同的事，但不需要安裝 sqlite3：這個執行檔本身就內建了一份 SQLite。
//! shell 腳本保留作為最後的備案，用在這個執行檔本身無法執行的情況。
//!
//! 只讀取：資料庫以唯讀模式開啟、不執行 migration，blob 只被複製。服務執行中也能安全使用。
//! 所有終端機訊息使用英文；使用者的檔名是資料，照原樣顯示。
//!
//! ## 以 root 執行時的安全性
//!
//! 這個指令通常以 sudo 執行，也就是以 root 身分寫入檔案。若輸出目錄是別人也能寫入的地方，
//! 對方可以在匯出途中放進符號連結（例如 `out/mallory -> /etc/cron.d`），
//! 讓 root 沿著連結在系統目錄中建立檔案。
//!
//! 解法是讓別人根本無法在輸出目錄裡放任何東西：輸出目錄必須不存在，由這裡以 0700 建立，
//! 匯出期間只有執行者能寫入；全部寫完之後才改回執行 sudo 的人。
//! 另外兩層防護：寫入檔案用 create_new（目標已存在、包括連結，就失敗），
//! 變更擁有者用 lchown（對連結本身操作，不跟隨）。

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use sqlx::SqlitePool;

use crate::{config, db, layout, storage};

pub const USAGE: &str = "usage: manorcloud export [--env-file <file>] <output-dir>\n\
                         The output directory must not exist yet; it will be created.";
const MIB: u64 = 1024 * 1024;

/// 從根目錄往下遞迴，每一層把名稱接在上一層的路徑後面（與 export-files.sh 相同）。
/// 深度上限 256：與伺服器端的祖先鏈查詢相同，資料被改壞形成循環時也會停止。
const TREE_QUERY: &str = "
WITH RECURSIVE tree(id, path, depth) AS (
    SELECT id, name, 0 FROM files WHERE parent_id IS NULL
    UNION ALL
    SELECT f.id, tree.path || '/' || f.name, tree.depth + 1
    FROM files f JOIN tree ON f.parent_id = tree.id
    WHERE tree.depth < 256
)
SELECT users.username, tree.path, files.kind, files.storage_key, files.updated_at
FROM tree
JOIN files ON files.id = tree.id
JOIN users ON users.id = files.owner_id
ORDER BY users.username, tree.path
";

pub async fn run(args: &[OsString]) -> Result<(), String> {
    let (env_file, out) = parse_args(args)?;
    let settings = config::load_from_file(&env_file)?;
    let db_path = layout::resolve(config::sqlite_path(&settings.database_url)?);
    let data_dir = layout::resolve(settings.data_dir);

    println!("database: {}", db_path.display());
    println!("data directory: {}", data_dir.display());

    // 先確認資料庫打得開，再建立輸出目錄：失敗時不留下一個空目錄
    let pool = db::open_read_only(&db_path)
        .await
        .map_err(|e| format!("cannot open the database {}: {e}", db_path.display()))?;

    if let Err(e) = create_output_dir(&out) {
        pool.close().await;
        return Err(e);
    }

    let result = export_tree(&pool, &data_dir, &out).await;
    pool.close().await;

    // 不論成功或失敗都改回擁有者：失敗時已經匯出的部分，執行者也要能自己刪除。
    // 輸出目錄一定是這次建立的（create_output_dir 拒絕已存在的目錄），不會改到別人的目錄
    if let Some((uid, gid)) = sudo_owner()
        && let Err(e) = chown_recursive(&out, uid, gid)
    {
        eprintln!("warning: cannot change the owner of {}: {e}", out.display());
    }

    let summary = result?;
    println!(
        "done: {} files and {} folders exported to {}",
        summary.files,
        summary.folders,
        out.display()
    );
    if summary.problems > 0 {
        return Err(format!(
            "{} items could not be exported; see the warnings above",
            summary.problems
        ));
    }
    Ok(())
}

fn parse_args(args: &[OsString]) -> Result<(PathBuf, PathBuf), String> {
    let mut env_file = PathBuf::from(layout::ENV_FILE);
    let mut out = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--env-file") => {
                env_file = iter
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--env-file needs a path".to_string())?;
            }
            Some(flag) if flag.starts_with('-') => {
                return Err(format!("unknown option: {flag}\n{USAGE}"));
            }
            _ => {
                if out.is_some() {
                    return Err(format!("only one output directory is allowed\n{USAGE}"));
                }
                out = Some(PathBuf::from(arg));
            }
        }
    }

    Ok((env_file, out.ok_or_else(|| USAGE.to_string())?))
}

// ==================== 輸出目錄 ====================

/// 建立輸出目錄：必須不存在，權限 0700。
///
/// 這是以 root 執行時的主要防線：目錄是這次新建的，匯出期間只有執行者能寫入，
/// 其他使用者無法在裡面放進符號連結。上層目錄是執行者自己指定的路徑，可以一併建立。
fn create_output_dir(out: &Path) -> Result<(), String> {
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    // create（不是 create_all）：最後一段已存在時一定失敗，包括它是一個符號連結
    fs::DirBuilder::new()
        .mode(0o700)
        .create(out)
        .map_err(|e| match e.kind() {
            io::ErrorKind::AlreadyExists => format!(
                "{} already exists; give a new path and it will be created",
                out.display()
            ),
            _ => format!("cannot create {}: {e}", out.display()),
        })
}

// ==================== 匯出 ====================

#[derive(Debug, Default, PartialEq)]
struct Summary {
    files: u64,
    folders: u64,
    /// 找不到內容、路徑不安全、或目標已存在而略過的項目
    problems: u64,
}

/// 匯出到 `out`。呼叫前 `out` 必須已由 `create_output_dir` 建立。
async fn export_tree(pool: &SqlitePool, data_dir: &Path, out: &Path) -> Result<Summary, String> {
    let rows: Vec<(String, String, String, Option<String>, i64)> = sqlx::query_as(TREE_QUERY)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("cannot read the database: {e}"))?;
    let total: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM files WHERE kind = 'file'")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("cannot read the database: {e}"))?;

    // 這是複製而不是搬移：輸出目錄所在的磁碟需要與所有檔案相同的空間
    let needed = u64::try_from(total).unwrap_or(0);
    let available =
        storage::available_bytes(out).map_err(|e| format!("cannot check the free space: {e}"))?;
    if needed > available {
        return Err(format!(
            "not enough space: about {} MB needed, {} MB available on {}",
            needed / MIB,
            available / MIB,
            out.display()
        ));
    }
    println!("output: {} (about {} MB)", out.display(), needed / MIB);

    let mut summary = Summary::default();
    for (user, path, kind, key, updated_at) in rows {
        // 每一段名稱再檢查一次：資料庫被改壞也不會寫到輸出目錄以外
        let Some(relative) = safe_relative(&user, &path) else {
            eprintln!("warning: skipped an unsafe path: {user}/{path}");
            summary.problems += 1;
            continue;
        };
        let target = out.join(relative);

        if kind == "folder" {
            // 空資料夾也要建立。輸出目錄只有執行者能寫入，這裡的上層路徑不會被換成連結
            fs::create_dir_all(&target)
                .map_err(|e| format!("cannot create {}: {e}", target.display()))?;
            summary.folders += 1;
            continue;
        }

        // blob_path 會驗證 key 的格式，不合法時回傳 None
        let blob = match key.as_deref().and_then(|k| storage::blob_path(data_dir, k)) {
            Some(blob) if blob.is_file() => blob,
            _ => {
                eprintln!("warning: content not found for {user}/{path}");
                summary.problems += 1;
                continue;
            }
        };

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }

        match copy_to_new_file(&blob, &target, updated_at) {
            Ok(()) => summary.files += 1,
            // 目標已存在：絕不覆蓋，記為問題並繼續
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                eprintln!("warning: skipped {user}/{path}: the target already exists");
                summary.problems += 1;
            }
            Err(e) => return Err(format!("cannot write {}: {e}", target.display())),
        }
    }

    Ok(summary)
}

/// 把 blob 複製成一個**新**檔案，並還原修改時間。
///
/// create_new（O_EXCL）：目標已存在就失敗，而且不跟隨符號連結，絕不會覆蓋任何東西。
/// 修改時間直接用寫入時開啟的檔案設定，不必再開一次。
fn copy_to_new_file(blob: &Path, target: &Path, seconds: i64) -> io::Result<()> {
    let mut source = fs::File::open(blob)?;
    let mut dest = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut source, &mut dest)?;

    // 負數的時間不處理
    if let Ok(seconds) = u64::try_from(seconds) {
        dest.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))?;
    }
    Ok(())
}

/// 帳號與路徑的每一段都必須是普通的名稱：不可為空、`.`、`..`，不可含 `/` 或 NUL
fn safe_relative(user: &str, path: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for part in std::iter::once(user).chain(path.split('/')) {
        if part.is_empty() || part == "." || part == ".." || part.contains(['/', '\0']) {
            return None;
        }
        relative.push(part);
    }
    Some(relative)
}

/// 以 sudo 執行時，執行 sudo 的那個人的使用者與群組編號
fn sudo_owner() -> Option<(u32, u32)> {
    let uid = std::env::var("SUDO_UID").ok()?.parse().ok()?;
    let gid = std::env::var("SUDO_GID").ok()?.parse().ok()?;
    Some((uid, gid))
}

/// 遞迴變更擁有者。
/// lchown：對連結本身操作，不跟隨；symlink_metadata：不進入連結指向的目錄。
/// 匯出不會建立任何連結，這兩點都是保險。
fn chown_recursive(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if fs::symlink_metadata(path)?.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_recursive(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{files as files_db, user};
    use std::os::unix::fs::PermissionsExt;

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    fn write_blob(data_dir: &Path, key: &str, content: &[u8]) {
        let path = storage::blob_path(data_dir, key).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    /// 在暫存目錄中建立一個新的輸出目錄（與正式流程相同）
    fn new_output(parent: &tempfile::TempDir) -> PathBuf {
        let out = parent.path().join("out");
        create_output_dir(&out).unwrap();
        out
    }

    // ==================== 純函式 ====================

    #[test]
    fn unsafe_paths_are_rejected() {
        assert_eq!(
            safe_relative("alston", "相片/旅行.jpg"),
            Some(PathBuf::from("alston/相片/旅行.jpg"))
        );
        for (user, path) in [
            ("alston", "../etc/passwd"),
            ("alston", "a/./b"),
            ("alston", "a//b"),
            ("..", "x"),
            ("a/b", "x"),
            ("alston", ""),
        ] {
            assert!(safe_relative(user, path).is_none(), "{user} {path}");
        }
    }

    #[test]
    fn env_file_option_needs_a_path() {
        let error = parse_args(&[OsString::from("--env-file")]).unwrap_err();
        assert_eq!(error, "--env-file needs a path");
    }

    #[test]
    fn usage_is_ascii() {
        assert!(USAGE.is_ascii());
    }

    // ==================== 以 root 執行時的安全性 ====================

    #[test]
    fn output_dir_must_not_exist() {
        let parent = tempfile::tempdir().unwrap();
        let out = parent.path().join("out");
        fs::create_dir(&out).unwrap();

        let error = create_output_dir(&out).unwrap_err();

        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn output_dir_symlink_is_refused() {
        // 有人事先放了一個指向別處的連結，假裝是輸出目錄
        let parent = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let out = parent.path().join("out");
        std::os::unix::fs::symlink(elsewhere.path(), &out).unwrap();

        assert!(create_output_dir(&out).is_err());
    }

    #[test]
    fn output_dir_is_private() {
        let parent = tempfile::tempdir().unwrap();
        let out = new_output(&parent);

        // 匯出期間其他使用者無法在裡面建立任何東西
        let mode = fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn copy_never_follows_a_symlink_at_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("blob");
        let victim = dir.path().join("victim");
        let target = dir.path().join("target");
        fs::write(&blob, b"attacker content").unwrap();
        fs::write(&victim, b"original").unwrap();
        std::os::unix::fs::symlink(&victim, &target).unwrap();

        let error = copy_to_new_file(&blob, &target, 0).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        // 連結指向的檔案完全沒有被動到
        assert_eq!(fs::read(&victim).unwrap(), b"original");
    }

    // ==================== 匯出 ====================

    #[sqlx::test]
    async fn exports_names_structure_and_times(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let out = new_output(&parent);
        let owner = user::create_user(&pool, "alston", "x").await.unwrap();
        let photos = files_db::create_folder(&pool, owner, None, "相片")
            .await
            .unwrap()
            .id;
        files_db::create_folder(&pool, owner, None, "空的")
            .await
            .unwrap();
        write_blob(data.path(), &key(1), b"hello");
        let name = "旅行 \"最終版\" 100%22.txt";
        let file = files_db::create_file(&pool, owner, Some(photos), name, 5, None, &key(1))
            .await
            .unwrap();

        let summary = export_tree(&pool, data.path(), &out).await.unwrap();

        assert_eq!(
            summary,
            Summary {
                files: 1,
                folders: 2,
                problems: 0
            }
        );
        let exported = out.join("alston/相片").join(name);
        assert_eq!(fs::read(&exported).unwrap(), b"hello");
        assert!(out.join("alston/空的").is_dir());
        let modified = fs::metadata(&exported).unwrap().modified().unwrap();
        assert_eq!(
            modified,
            UNIX_EPOCH + Duration::from_secs(u64::try_from(file.updated_at).unwrap())
        );
    }

    #[sqlx::test]
    async fn missing_blob_is_reported_and_others_continue(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let out = new_output(&parent);
        let owner = user::create_user(&pool, "alston", "x").await.unwrap();
        // a.txt 的 blob 不存在；b.txt 正常
        files_db::create_file(&pool, owner, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();
        write_blob(data.path(), &key(2), b"b");
        files_db::create_file(&pool, owner, None, "b.txt", 1, None, &key(2))
            .await
            .unwrap();

        let summary = export_tree(&pool, data.path(), &out).await.unwrap();

        assert_eq!(summary.files, 1);
        assert_eq!(summary.problems, 1);
        assert!(out.join("alston/b.txt").is_file());
    }
}
