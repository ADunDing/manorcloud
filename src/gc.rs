//! 垃圾回收：放棄的分段上傳、留下來的暫存檔、超過保留期限的垃圾桶項目、很久沒用到的縮圖，
//! 以及過期的 session。
//!
//! 每顆有掛載的硬碟（資料目錄與儲存位置）各自檢查它的 `.manorcloud/`。
//! **只會動 `.manorcloud/` 裡面的東西**，使用者的檔案永遠不會被這裡刪除。
//!
//! 規則：
//! - 只刪自己認得的東西：名稱、位置、檔案類型全部符合才處理，其他一律跳過
//! - 永遠不跟隨符號連結
//! - 查不到資料庫時，絕不刪除分段上傳的暫存檔或垃圾桶中的項目
//! - 只刪超過寬限期的；時間異常（mtime 在未來）一律保留
//! - 先列出硬碟上的檔案，再查資料庫：紀錄一定比檔案先寫入（見 db::trash、uploads.rs），
//!   所以列出時已經存在的檔案，查詢時一定查得到它的紀錄

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use tokio::time::MissedTickBehavior;

use crate::{
    db::{
        session, storage as storage_db, transfers as transfers_db, trash as trash_db,
        uploads as uploads_db,
    },
    storage, thumbs, tree,
};

/// 啟動後延遲多久才第一次執行。
/// 樹莓派沒有硬體時鐘，開機時系統時間是上次關機的時間，要等 NTP 同步才正確
const INITIAL_DELAY: Duration = Duration::from_secs(5 * 60);

/// 之後每隔多久執行一次
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// 寬限期設定
#[derive(Debug, Clone, Copy)]
pub struct GcPolicy {
    /// 沒有紀錄的分段上傳暫存檔、垃圾桶項目：寫入紀錄與檔案之間只有極短的空窗，1 小時非常寬裕
    pub orphan_grace: Duration,
    /// tmp/ 中的暫存（一般上傳、複製）：多久沒有變動就視為中斷
    pub tmp_grace: Duration,
    /// 分段上傳的紀錄多久沒有進度就視為放棄。
    /// 從最後一次進度算起：晚上傳到一半闔上筆電，隔天早上還能續傳
    pub upload_grace: Duration,
    /// 放進垃圾桶的項目保留多久，之後永久刪除。與 Google Drive 相同的 30 天
    pub trash_retention: Duration,
    /// 縮圖多久沒有用到就刪除（用到時會更新修改時間，見 thumbs.rs）
    pub thumbs_unused: Duration,
}

impl Default for GcPolicy {
    fn default() -> Self {
        GcPolicy {
            orphan_grace: Duration::from_secs(60 * 60),
            tmp_grace: Duration::from_secs(24 * 60 * 60),
            upload_grace: Duration::from_secs(24 * 60 * 60),
            trash_retention: Duration::from_secs(30 * 24 * 60 * 60),
            thumbs_unused: Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

/// 一次執行的結果
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub deleted_tmp: u64,
    /// 分段上傳的暫存檔（放棄的上傳，以及沒有紀錄的孤兒）
    pub deleted_parts: u64,
    /// 放棄的分段上傳紀錄
    pub expired_uploads: u64,
    pub expired_sessions: u64,
    /// 超過保留期限、從垃圾桶永久刪除的項目（資料夾算一個）
    pub purged_trash: u64,
    /// 垃圾桶中沒有紀錄的檔案，以及沒有檔案的紀錄
    pub orphan_trash: u64,
    pub deleted_thumbs: u64,
    pub errors: u64,
}

/// 印到終端機與 journal，所以使用英文
impl fmt::Display for GcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "deleted {} temp items, {} upload part files; expired {} uploads, {} sessions; \
             purged {} trash items, {} orphaned trash entries; {} thumbnails; {} errors",
            self.deleted_tmp,
            self.deleted_parts,
            self.expired_uploads,
            self.expired_sessions,
            self.purged_trash,
            self.orphan_trash,
            self.deleted_thumbs,
            self.errors
        )
    }
}

/// 是否已超過寬限期。mtime 在「現在」之後一律視為未過期：寧可少刪，不可誤刪
pub fn is_expired(modified: SystemTime, now: SystemTime, grace: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age > grace,
        Err(_) => false,
    }
}

fn unix_secs(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// 一顆要檢查的硬碟：儲存位置的編號（None 是資料目錄）與根目錄
struct Root {
    volume: Option<i64>,
    path: PathBuf,
}

/// 執行一次
pub async fn run_once(
    pool: &SqlitePool,
    data_dir: &Path,
    now: SystemTime,
    policy: &GcPolicy,
) -> GcReport {
    let mut report = GcReport::default();
    let roots = roots(pool, data_dir, &mut report).await;

    if let Some(cutoff) =
        unix_secs(now).and_then(|n| n.checked_sub(policy.upload_grace.as_secs() as i64))
    {
        match uploads_db::delete_stale(pool, cutoff).await {
            Ok(ids) => report.expired_uploads += ids.len() as u64,
            Err(e) => {
                eprintln!("gc: cannot expire uploads: {e}");
                report.errors += 1;
            }
        }
    }
    for root in &roots {
        collect_parts(pool, &root.path, now, policy, &mut report).await;
        collect_tmp(pool, &root.path, now, policy, &mut report).await;
        collect_trash(pool, root.volume, &root.path, now, policy, &mut report).await;
        collect_thumbs(&root.path, now, policy, &mut report).await;
    }
    collect_sessions(pool, now, &mut report).await;
    report
}

/// 有掛載的硬碟。沒有掛載的硬碟跳過：沒有東西可以清理
async fn roots(pool: &SqlitePool, data_dir: &Path, report: &mut GcReport) -> Vec<Root> {
    let mut roots = vec![Root {
        volume: None,
        path: data_dir.to_path_buf(),
    }];
    match storage_db::list_volumes(pool).await {
        Ok(volumes) => roots.extend(
            volumes
                .into_iter()
                .map(|v| Root {
                    volume: Some(v.id),
                    path: PathBuf::from(v.path),
                })
                .filter(|root| storage::volume_ready(&root.path)),
        ),
        Err(e) => {
            eprintln!("gc: cannot list the storage locations: {e}");
            report.errors += 1;
        }
    }
    roots
}

/// 資料夾中的一般檔案或資料夾（不含符號連結）：(名稱, 路徑, 修改時間)
fn entries(dir: &Path) -> io::Result<Vec<(String, PathBuf, SystemTime)>> {
    let items = match std::fs::read_dir(dir) {
        Ok(items) => items,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for item in items {
        let item = item?;
        let meta = std::fs::symlink_metadata(item.path())?;
        if !(meta.is_file() || meta.is_dir()) {
            continue;
        }
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        out.push((name, item.path(), meta.modified()?));
    }
    Ok(out)
}

async fn list(dir: PathBuf, report: &mut GcReport) -> Option<Vec<(String, PathBuf, SystemTime)>> {
    let shown = dir.display().to_string();
    match tokio::task::spawn_blocking(move || entries(&dir)).await {
        Ok(Ok(list)) => Some(list),
        Ok(Err(e)) => {
            eprintln!("gc: cannot list {shown}: {e}");
            report.errors += 1;
            None
        }
        Err(e) => {
            eprintln!("gc: cannot list {shown}: {e}");
            report.errors += 1;
            None
        }
    }
}

/// 刪除一個項目，回傳是否成功（本來就不在也算成功）
async fn delete(path: PathBuf, report: &mut GcReport) -> bool {
    let shown = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || match tree::remove_path(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
    .await;
    match result {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            eprintln!("gc: cannot delete {shown}: {e}");
            report.errors += 1;
            false
        }
        Err(e) => {
            eprintln!("gc: cannot delete {shown}: {e}");
            report.errors += 1;
            false
        }
    }
}

/// 沒有紀錄的分段上傳暫存檔
async fn collect_parts(
    pool: &SqlitePool,
    root: &Path,
    now: SystemTime,
    policy: &GcPolicy,
    report: &mut GcReport,
) {
    let Some(files) = list(storage::uploads_dir(root), report).await else {
        return;
    };
    let known: HashSet<String> = match uploads_db::all_ids(pool).await {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            eprintln!("gc: cannot query uploads, skipping their part files: {e}");
            report.errors += 1;
            return;
        }
    };
    for (name, path, modified) in files {
        let Some(id) = name
            .strip_suffix(".part")
            .filter(|id| storage::is_valid_key(id))
        else {
            continue;
        };
        if !known.contains(id)
            && is_expired(modified, now, policy.orphan_grace)
            && delete(path, report).await
        {
            report.deleted_parts += 1;
        }
    }
}

/// tmp/ 中太久沒有變動的暫存。進行中的搬移與傳輸（move-、transfer-{編號}）可能要好幾個小時，跳過
async fn collect_tmp(
    pool: &SqlitePool,
    root: &Path,
    now: SystemTime,
    policy: &GcPolicy,
    report: &mut GcReport,
) {
    let Some(items) = list(storage::tmp_dir(root), report).await else {
        return;
    };
    let (moves, transfers) = match (
        storage_db::running_moves(pool).await,
        transfers_db::running(pool).await,
    ) {
        (Ok(moves), Ok(transfers)) => (
            moves.into_iter().collect::<HashSet<i64>>(),
            transfers.into_iter().collect::<HashSet<i64>>(),
        ),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("gc: cannot query moves and transfers: {e}");
            report.errors += 1;
            return;
        }
    };
    let running = |prefix: &str, ids: &HashSet<i64>, name: &str| {
        name.strip_prefix(prefix)
            .and_then(|id| id.parse().ok())
            .is_some_and(|id| ids.contains(&id))
    };
    for (name, path, modified) in items {
        let moving = running("move-", &moves, &name) || running("transfer-", &transfers, &name);
        if !moving && is_expired(modified, now, policy.tmp_grace) && delete(path, report).await {
            report.deleted_tmp += 1;
        }
    }
}

/// 垃圾桶：超過保留期限的項目、沒有紀錄的檔案、沒有檔案的紀錄
async fn collect_trash(
    pool: &SqlitePool,
    volume: Option<i64>,
    root: &Path,
    now: SystemTime,
    policy: &GcPolicy,
    report: &mut GcReport,
) {
    let Some(files) = list(storage::trash_dir(root), report).await else {
        return;
    };
    let rows = match trash_db::on_volume(pool, volume).await {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("gc: cannot query the trash: {e}");
            report.errors += 1;
            return;
        }
    };
    let Some(now_secs) = unix_secs(now) else {
        return;
    };
    let retention = policy.trash_retention.as_secs() as i64;
    let grace = policy.orphan_grace.as_secs() as i64;
    let on_disk: HashSet<i64> = files
        .iter()
        .filter_map(|(name, _, _)| name.parse().ok())
        .collect();

    for row in &rows {
        let path = storage::trash_path(root, row.id);
        if row.trashed_at + retention < now_secs {
            // 先刪檔案，再刪紀錄
            if delete(path, report).await {
                match trash_db::remove(pool, row.id).await {
                    Ok(_) => report.purged_trash += 1,
                    Err(e) => {
                        eprintln!("gc: cannot delete trash entry {}: {e}", row.id);
                        report.errors += 1;
                    }
                }
            }
        } else if !on_disk.contains(&row.id) && row.trashed_at + grace < now_secs {
            // 放進垃圾桶時中途失敗，或檔案被手動刪除了
            if trash_db::remove(pool, row.id).await.is_ok() {
                report.orphan_trash += 1;
            }
        }
    }

    let known: HashSet<i64> = rows.iter().map(|row| row.id).collect();
    for (name, path, modified) in files {
        let Ok(id) = name.parse::<i64>() else {
            continue;
        };
        // 使用者被刪除之後留下的項目
        if !known.contains(&id)
            && is_expired(modified, now, policy.orphan_grace)
            && delete(path, report).await
        {
            report.orphan_trash += 1;
        }
    }
}

/// 很久沒有用到的縮圖：`thumbs/{兩個字元}/{32 個十六進位字元}.jpg`
async fn collect_thumbs(root: &Path, now: SystemTime, policy: &GcPolicy, report: &mut GcReport) {
    let Some(dirs) = list(thumbs::cache_dir(root), report).await else {
        return;
    };
    let hex = |text: &str, len: usize| {
        text.len() == len
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    for (dir_name, dir, _) in dirs {
        if !hex(&dir_name, 2) {
            continue;
        }
        let Some(files) = list(dir, report).await else {
            continue;
        };
        for (name, path, modified) in files {
            let ours = name
                .strip_suffix(".jpg")
                .is_some_and(|key| hex(key, 32) && key.starts_with(&dir_name));
            if ours && is_expired(modified, now, policy.thumbs_unused) && delete(path, report).await
            {
                report.deleted_thumbs += 1;
            }
        }
    }
}

/// 刪除過期的 session
async fn collect_sessions(pool: &SqlitePool, now: SystemTime, report: &mut GcReport) {
    let Some(now_secs) = unix_secs(now) else {
        return;
    };
    match session::delete_expired(pool, now_secs).await {
        Ok(n) => report.expired_sessions += n,
        Err(e) => {
            eprintln!("gc: cannot delete expired sessions: {e}");
            report.errors += 1;
        }
    }
}

// ==================== 定期執行 ====================

/// 在背景定期執行垃圾回收。每次執行都放在獨立的 task 中：
/// 就算某次執行因為 bug 而 panic，排程本身會繼續運作
pub fn spawn_periodic(pool: SqlitePool, data_dir: Arc<Path>, policy: GcPolicy) {
    tokio::spawn(async move {
        tokio::time::sleep(INITIAL_DELAY).await;

        let mut interval = tokio::time::interval(INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let pool = pool.clone();
            let data_dir = data_dir.clone();
            let run = tokio::spawn(async move {
                run_once(&pool, &data_dir, SystemTime::now(), &policy).await
            });

            match run.await {
                Ok(report) => println!("gc: done: {report}"),
                Err(e) => eprintln!("gc: error, will retry at the next run: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{trash::Place, user};

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn old(path: &Path) {
        tree::set_modified(path, SystemTime::now() - 40 * DAY);
    }

    #[sqlx::test]
    async fn only_systems_leftovers_are_removed(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        storage::init_dirs(root).unwrap();
        // 使用者的檔案：無論多舊都不碰
        std::fs::write(root.join("old.txt"), b"x").unwrap();
        old(&root.join("old.txt"));
        // tmp：舊的刪，新的留
        std::fs::write(storage::tmp_dir(root).join("upload-a"), b"x").unwrap();
        old(&storage::tmp_dir(root).join("upload-a"));
        std::fs::write(storage::tmp_dir(root).join("upload-b"), b"x").unwrap();
        // 沒有紀錄的暫存檔
        let orphan = storage::uploads_dir(root).join(format!("{:032x}.part", 1));
        std::fs::write(&orphan, b"x").unwrap();
        old(&orphan);
        // 縮圖：很久沒用到的刪，最近用過的留
        let thumbs = thumbs::cache_dir(root).join("ab");
        std::fs::create_dir_all(&thumbs).unwrap();
        let unused = thumbs.join(format!("ab{:030x}.jpg", 1));
        let used = thumbs.join(format!("ab{:030x}.jpg", 2));
        std::fs::write(&unused, b"x").unwrap();
        std::fs::write(&used, b"x").unwrap();
        old(&unused);

        let report = run_once(&pool, root, SystemTime::now(), &GcPolicy::default()).await;

        assert_eq!(report.deleted_tmp, 1);
        assert_eq!(report.deleted_parts, 1);
        assert_eq!(report.deleted_thumbs, 1);
        assert!(!unused.exists() && used.exists());
        assert!(root.join("old.txt").exists());
        assert!(storage::tmp_dir(root).join("upload-b").exists());
        assert!(!orphan.exists());
    }

    #[sqlx::test]
    async fn the_trash_is_emptied_after_the_retention_period(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        storage::init_dirs(root).unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let place = Place {
            volume_id: None,
            owner_id: Some(carol),
        };
        let expired = trash_db::add(&pool, place, "a", false, 1).await.unwrap();
        sqlx::query("UPDATE trash SET trashed_at = 0 WHERE id = ?")
            .bind(expired)
            .execute(&pool)
            .await
            .unwrap();
        let recent = trash_db::add(&pool, place, "b", false, 1).await.unwrap();
        for id in [expired, recent] {
            std::fs::write(storage::trash_path(root, id), b"x").unwrap();
        }
        // 沒有紀錄的舊檔案（使用者被刪除之後留下的）
        let left = storage::trash_path(root, 999);
        std::fs::write(&left, b"x").unwrap();
        old(&left);

        let report = run_once(&pool, root, SystemTime::now(), &GcPolicy::default()).await;

        assert_eq!(report.purged_trash, 1);
        assert_eq!(report.orphan_trash, 1);
        assert!(!storage::trash_path(root, expired).exists());
        assert!(storage::trash_path(root, recent).exists());
        assert!(!left.exists());
        assert_eq!(trash_db::list(&pool, place).await.unwrap().len(), 1);
    }
}
