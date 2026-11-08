//! 垃圾回收：清除磁碟上沒有資料庫參照的檔案、放棄的分段上傳，以及過期的 session。
//!
//! 核心不變式：資料庫裡的每一個 storage_key，磁碟上一定有對應的 blob；反過來不保證。
//! 「磁碟有、資料庫沒有」的孤兒由這裡清除，來源包括：
//! - 程式被 kill -9、斷電、panic=abort 時，TempBlob 的 Drop 沒有執行，tmp/ 留下殘檔
//! - 上傳時 persist 之後資料庫寫入失敗，而刪除 blob 也失敗
//! - 刪除項目時，commit 之後刪除 blob 失敗或中途斷線
//! - 分段上傳：宣告後放棄、取消或完成時刪除暫存檔失敗
//!
//! 這是整個專案中唯一會「主動刪除」磁碟檔案的程式，所以規則很嚴格：
//! - 只刪自己認得的東西：名稱、位置、檔案類型全部符合才處理，其他一律跳過
//! - 永遠不跟隨符號連結
//! - 查不到資料庫時，絕不刪除任何 blob 或分段上傳的暫存檔
//! - 只刪超過寬限期的檔案；時間異常（mtime 或 updated_at 在未來）一律保留

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use tokio::time::MissedTickBehavior;

use crate::{
    db::{files as files_db, session, uploads as uploads_db},
    storage,
};

/// 啟動後延遲多久才第一次執行。
/// 樹莓派沒有硬體時鐘，開機時系統時間是上次關機的時間，要等 NTP 同步才正確；
/// 延後執行讓時間先校正。
const INITIAL_DELAY: Duration = Duration::from_secs(5 * 60);

/// 之後每隔多久執行一次
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// 寬限期設定
#[derive(Debug, Clone, Copy)]
pub struct GcPolicy {
    /// blobs/ 中的孤兒：persist 與寫入資料庫之間只有極短的空窗，1 小時非常寬裕。
    /// （persist 會把 mtime 設為當下，所以這個時間是從 persist 起算的。）
    /// 沒有紀錄的分段上傳暫存檔也用這個：宣告時「先檔案、後紀錄」之間同樣只有極短的空窗。
    pub blob_grace: Duration,
    /// tmp/ 中的暫存檔：上傳中每寫入一塊 mtime 就更新，所以這是「多久沒收到資料」。
    /// 24 小時沒有任何資料進來的上傳幾乎可以確定已經中斷；保守的代價只是多佔一天的空間。
    pub tmp_grace: Duration,
    /// 分段上傳的紀錄多久沒有進度就視為放棄。
    /// 從最後一次進度算起：晚上傳到一半闔上筆電，隔天早上還能續傳
    pub upload_grace: Duration,
}

impl Default for GcPolicy {
    fn default() -> Self {
        GcPolicy {
            blob_grace: Duration::from_secs(60 * 60),
            tmp_grace: Duration::from_secs(24 * 60 * 60),
            upload_grace: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// 一次執行的結果
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub deleted_blobs: u64,
    pub deleted_tmp: u64,
    /// 分段上傳的暫存檔（放棄的上傳，以及沒有紀錄的孤兒）
    pub deleted_parts: u64,
    /// 放棄的分段上傳紀錄
    pub expired_uploads: u64,
    pub expired_sessions: u64,
    /// 是孤兒但還在寬限期內，這次保留
    pub kept_recent: u64,
    /// 不認得的項目（名稱不符、位置不符、目錄、符號連結），一律不處理
    pub skipped_unknown: u64,
    pub errors: u64,
}

/// 印到終端機與 journal，所以使用英文
impl fmt::Display for GcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "deleted {} blobs, {} temp files, {} upload part files; \
             expired {} uploads, {} sessions; \
             kept {} recent; skipped {} unknown; {} errors",
            self.deleted_blobs,
            self.deleted_tmp,
            self.deleted_parts,
            self.expired_uploads,
            self.expired_sessions,
            self.kept_recent,
            self.skipped_unknown,
            self.errors
        )
    }
}

// ==================== 純函式：判斷該刪什麼 ====================

/// 磁碟上的一個候選檔案
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub name: String,
    pub modified: SystemTime,
}

/// 篩選結果
#[derive(Debug, Default)]
pub struct Selection {
    pub delete: Vec<PathBuf>,
    pub kept_recent: u64,
}

/// 是否已超過寬限期。
///
/// mtime 在「現在」之後（例如樹莓派開機時時鐘還沒校正）一律視為未過期：寧可少刪，不可誤刪。
pub fn is_expired(modified: SystemTime, now: SystemTime, grace: Duration) -> bool {
    match now.duration_since(modified) {
        Ok(age) => age > grace,
        Err(_) => false,
    }
}

/// 從候選檔案中選出要刪除的孤兒：不在 `known` 中、而且超過寬限期。
///
/// 不碰檔案系統、不碰資料庫、不讀系統時間，所有輸入都由參數提供，方便測試。
/// tmp/ 的暫存檔沒有對應的資料庫紀錄，傳入空的 `known` 即可。
pub fn select_orphans(
    candidates: &[Candidate],
    known: &HashSet<String>,
    now: SystemTime,
    grace: Duration,
) -> Selection {
    let mut selection = Selection::default();
    for c in candidates {
        if known.contains(&c.name) {
            continue;
        }
        if is_expired(c.modified, now, grace) {
            selection.delete.push(c.path.clone());
        } else {
            selection.kept_recent += 1;
        }
    }
    selection
}

/// 時間換成 Unix 秒數。早於 1970 或超出 i64 範圍代表時鐘異常，回傳 None
fn unix_secs(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// 分段上傳暫存檔的檔名：{上傳編號}.part
fn part_file_name(id: &str) -> String {
    format!("{id}.part")
}

/// 是否為分段上傳暫存檔的檔名：編號必須是合法的格式
fn is_part_file_name(name: &str) -> bool {
    name.strip_suffix(".part")
        .is_some_and(storage::is_valid_key)
}

// ==================== 檔案系統操作（阻塞，在 spawn_blocking 中執行） ====================

/// 列出目錄的結果：符合條件的候選檔案，以及不認得的項目數量
#[derive(Debug, Default)]
struct Listing {
    candidates: Vec<Candidate>,
    unknown: u64,
}

/// 列出目錄中「是一般檔案、而且名稱通過 `accept`」的項目。目錄不存在視為空的。
///
/// `DirEntry::file_type` 與 `DirEntry::metadata` 都不跟隨符號連結：
/// 符號連結的類型是「符號連結」而不是「一般檔案」，會被計為不明項目而跳過。
fn list_files(dir: &Path, accept: impl Fn(&str) -> bool) -> io::Result<Listing> {
    let mut listing = Listing::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(listing),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let is_file = entry.file_type()?.is_file();
        match entry.file_name().to_str() {
            Some(name) if is_file && accept(name) => listing.candidates.push(Candidate {
                path: entry.path(),
                name: name.to_string(),
                modified: entry.metadata()?.modified()?,
            }),
            _ => listing.unknown += 1,
        }
    }
    Ok(listing)
}

/// 列出 blobs/ 底下的分層目錄名稱。不是合法分層目錄的項目計為不明。
fn list_shards(blobs_dir: &Path) -> io::Result<(Vec<String>, u64)> {
    let mut shards = Vec::new();
    let mut unknown = 0;
    let entries = match fs::read_dir(blobs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((shards, 0)),
        Err(e) => return Err(e),
    };

    for entry in entries {
        let entry = entry?;
        let is_dir = entry.file_type()?.is_dir();
        match entry.file_name().to_str() {
            Some(name) if is_dir && storage::is_shard_name(name) => shards.push(name.to_string()),
            _ => unknown += 1,
        }
    }
    shards.sort();
    Ok((shards, unknown))
}

/// 逐一刪除檔案，回傳 (成功數, 錯誤數)。已經不存在的不算錯誤，也不算刪除。
fn remove_files(paths: Vec<PathBuf>) -> (u64, u64) {
    let mut deleted = 0;
    let mut errors = 0;
    for path in paths {
        match fs::remove_file(&path) {
            Ok(()) => deleted += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("gc: cannot delete {}: {e}", path.display());
                errors += 1;
            }
        }
    }
    (deleted, errors)
}

/// 在專用執行緒中執行阻塞的檔案系統操作
async fn blocking<T, F>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

/// 在專用執行緒中刪除檔案，回傳 (成功數, 錯誤數)
async fn delete_in_background(paths: Vec<PathBuf>) -> (u64, u64) {
    if paths.is_empty() {
        return (0, 0);
    }
    match blocking(move || Ok(remove_files(paths))).await {
        Ok(counts) => counts,
        Err(e) => {
            eprintln!("gc: the delete task failed: {e}");
            (0, 1)
        }
    }
}

// ==================== 執行一次 ====================

/// 執行一次完整的垃圾回收。「現在」與寬限期由參數傳入，測試可以控制時間。
///
/// 個別步驟的錯誤只記 log 並計入報告，不會中斷其他步驟。
pub async fn run_once(
    pool: &SqlitePool,
    data_dir: &Path,
    now: SystemTime,
    policy: &GcPolicy,
) -> GcReport {
    let mut report = GcReport::default();
    collect_blobs(pool, data_dir, now, policy.blob_grace, &mut report).await;
    collect_tmp(data_dir, now, policy.tmp_grace, &mut report).await;
    collect_uploads(pool, data_dir, now, policy, &mut report).await;
    collect_sessions(pool, now, &mut report).await;
    report
}

/// 清除 blobs/ 中的孤兒：依分層逐一比對，記憶體用量不隨檔案總數成長
async fn collect_blobs(
    pool: &SqlitePool,
    data_dir: &Path,
    now: SystemTime,
    grace: Duration,
    report: &mut GcReport,
) {
    let blobs_dir = storage::blobs_dir(data_dir);

    let shards = {
        let dir = blobs_dir.clone();
        blocking(move || list_shards(&dir)).await
    };
    let (shards, unknown) = match shards {
        Ok(result) => result,
        Err(e) => {
            eprintln!("gc: cannot list {}: {e}", blobs_dir.display());
            report.errors += 1;
            return;
        }
    };
    report.skipped_unknown += unknown;

    for shard in shards {
        // 1. 列出這個分層的檔案：名稱必須是合法 key，而且開頭與分層目錄相符
        let shard_dir = blobs_dir.join(&shard);
        let listing = {
            let dir = shard_dir.clone();
            let prefix = shard.clone();
            blocking(move || {
                list_files(&dir, |name| {
                    storage::is_valid_key(name) && name.starts_with(prefix.as_str())
                })
            })
            .await
        };
        let listing = match listing {
            Ok(listing) => listing,
            Err(e) => {
                eprintln!("gc: cannot list {}: {e}", shard_dir.display());
                report.errors += 1;
                continue;
            }
        };
        report.skipped_unknown += listing.unknown;

        // 2. 查資料庫。查詢失敗就跳過整個分層：沒有正確的 key 清單時絕不刪除任何東西
        let known: HashSet<String> = match files_db::keys_in_shard(pool, &shard).await {
            Ok(keys) => keys.into_iter().collect(),
            Err(e) => {
                eprintln!("gc: cannot query keys for shard {shard}, skipping: {e}");
                report.errors += 1;
                continue;
            }
        };

        // 3. 判斷並刪除
        let selection = select_orphans(&listing.candidates, &known, now, grace);
        report.kept_recent += selection.kept_recent;
        let (deleted, errors) = delete_in_background(selection.delete).await;
        report.deleted_blobs += deleted;
        report.errors += errors;
    }
}

/// 清除 tmp/ 中長時間沒有寫入的暫存檔
async fn collect_tmp(data_dir: &Path, now: SystemTime, grace: Duration, report: &mut GcReport) {
    let tmp_dir = storage::tmp_dir(data_dir);

    let listing = {
        let dir = tmp_dir.clone();
        blocking(move || list_files(&dir, storage::is_temp_file_name)).await
    };
    let listing = match listing {
        Ok(listing) => listing,
        Err(e) => {
            eprintln!("gc: cannot list {}: {e}", tmp_dir.display());
            report.errors += 1;
            return;
        }
    };
    report.skipped_unknown += listing.unknown;

    // 暫存檔沒有對應的資料庫紀錄：known 是空的，只依寬限期判斷
    let selection = select_orphans(&listing.candidates, &HashSet::new(), now, grace);
    report.kept_recent += selection.kept_recent;
    let (deleted, errors) = delete_in_background(selection.delete).await;
    report.deleted_tmp += deleted;
    report.errors += errors;
}

/// 清除分段上傳：放棄的上傳，以及沒有紀錄的暫存檔
async fn collect_uploads(
    pool: &SqlitePool,
    data_dir: &Path,
    now: SystemTime,
    policy: &GcPolicy,
    report: &mut GcReport,
) {
    let uploads_dir = storage::uploads_dir(data_dir);

    // 1. 放棄的上傳：先刪紀錄、再刪暫存檔（與使用者取消相同的順序）。
    //    刪除暫存檔失敗也沒關係：紀錄已經不在，下一步會把它當成孤兒處理。
    //    時鐘異常時跳過；updated_at 在未來的紀錄自然不會被選中
    if let Some(now_secs) = unix_secs(now) {
        let grace = i64::try_from(policy.upload_grace.as_secs()).unwrap_or(i64::MAX);
        match uploads_db::delete_stale(pool, now_secs.saturating_sub(grace)).await {
            Ok(ids) => {
                report.expired_uploads += ids.len() as u64;
                // 編號受資料庫的 CHECK 保護，這裡再驗證一次：路徑由它組成
                let paths = ids
                    .iter()
                    .filter(|id| storage::is_valid_key(id))
                    .map(|id| uploads_dir.join(part_file_name(id)))
                    .collect();
                let (deleted, errors) = delete_in_background(paths).await;
                report.deleted_parts += deleted;
                report.errors += errors;
            }
            Err(e) => {
                eprintln!("gc: cannot delete stale uploads: {e}");
                report.errors += 1;
            }
        }
    }

    // 2. 沒有紀錄的暫存檔。先列檔案、再查資料庫：
    //    列出之後才寫入紀錄的上傳，查詢時已經在清單中；
    //    查詢之後才寫入紀錄的，它的暫存檔一定很新，在寬限期內被保留
    let listing = {
        let dir = uploads_dir.clone();
        blocking(move || list_files(&dir, is_part_file_name)).await
    };
    let listing = match listing {
        Ok(listing) => listing,
        Err(e) => {
            eprintln!("gc: cannot list {}: {e}", uploads_dir.display());
            report.errors += 1;
            return;
        }
    };
    report.skipped_unknown += listing.unknown;

    // 查詢失敗時跳過：沒有正確的清單時絕不刪除任何東西
    let known: HashSet<String> = match uploads_db::all_ids(pool).await {
        Ok(ids) => ids.iter().map(|id| part_file_name(id)).collect(),
        Err(e) => {
            eprintln!("gc: cannot query uploads, skipping their part files: {e}");
            report.errors += 1;
            return;
        }
    };

    let selection = select_orphans(&listing.candidates, &known, now, policy.blob_grace);
    report.kept_recent += selection.kept_recent;
    let (deleted, errors) = delete_in_background(selection.delete).await;
    report.deleted_parts += deleted;
    report.errors += errors;
}

/// 刪除過期的 session
async fn collect_sessions(pool: &SqlitePool, now: SystemTime, report: &mut GcReport) {
    // 時鐘異常，這次不清理
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

/// 在背景定期執行垃圾回收：啟動後延遲一段時間跑第一次，之後每隔固定時間跑一次。
///
/// 每次執行都放在獨立的 task 中：就算某次執行因為 bug 而 panic，
/// 也只影響那一次，排程本身會繼續運作，並把 panic 記錄下來。
pub fn spawn_periodic(pool: SqlitePool, data_dir: Arc<Path>, policy: GcPolicy) {
    tokio::spawn(async move {
        tokio::time::sleep(INITIAL_DELAY).await;

        let mut interval = tokio::time::interval(INTERVAL);
        // 若某次執行超過間隔（或系統休眠），之後的排程順延，而不是連續補跑
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            // 第一次 tick 立即完成
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
    use crate::db::{files as files_db, session, uploads::NewUpload, user};
    use tempfile::TempDir;

    const MINUTE: Duration = Duration::from_secs(60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// 產生指定分層前綴的合法 key
    fn key(prefix: &str, n: u32) -> String {
        format!("{prefix}{n:030x}")
    }

    /// 寫入一個檔案並設定它的 mtime
    fn put(path: &Path, mtime: SystemTime) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn blob(dir: &TempDir, key: &str) -> PathBuf {
        storage::blob_path(dir.path(), key).unwrap()
    }

    fn tmp(dir: &TempDir, name: &str) -> PathBuf {
        storage::tmp_dir(dir.path()).join(name)
    }

    fn part(dir: &TempDir, id: &str) -> PathBuf {
        storage::uploads_dir(dir.path()).join(part_file_name(id))
    }

    fn candidate(name: &str, modified: SystemTime) -> Candidate {
        Candidate {
            path: PathBuf::from(name),
            name: name.to_string(),
            modified,
        }
    }

    /// 建立一筆上傳紀錄，並把「最後有進度」的時間設為 `updated`
    async fn make_upload(pool: &SqlitePool, owner: i64, id: &str, updated: SystemTime) {
        let new = NewUpload {
            id,
            owner_id: owner,
            parent_id: None,
            name: id,
            size: 10,
            file_modified: None,
        };
        uploads_db::create_upload(pool, &new, 100).await.unwrap();
        sqlx::query("UPDATE uploads SET updated_at = ? WHERE id = ?")
            .bind(unix_secs(updated).unwrap())
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn upload_exists(pool: &SqlitePool, id: &str) -> bool {
        uploads_db::all_ids(pool)
            .await
            .unwrap()
            .iter()
            .any(|known| known == id)
    }

    // ==================== 純函式 ====================

    #[test]
    fn expiry_respects_grace() {
        let now = SystemTime::now();
        let grace = Duration::from_secs(3600);

        assert!(is_expired(now - 2 * grace, now, grace));
        assert!(!is_expired(now - MINUTE, now, grace));
        // 剛好等於寬限期不算過期
        assert!(!is_expired(now - grace, now, grace));
    }

    #[test]
    fn future_mtime_is_never_expired() {
        // 樹莓派開機時時鐘可能落後，檔案的 mtime 看起來在「未來」
        let now = SystemTime::now();
        assert!(!is_expired(now + DAY, now, Duration::ZERO));
    }

    #[test]
    fn select_skips_known_and_recent() {
        let now = SystemTime::now();
        let known: HashSet<String> = ["referenced".to_string()].into();
        let candidates = [
            candidate("referenced", now - 2 * DAY),
            candidate("old-orphan", now - 2 * DAY),
            candidate("new-orphan", now - MINUTE),
        ];

        let selection = select_orphans(&candidates, &known, now, Duration::from_secs(3600));

        assert_eq!(selection.delete, vec![PathBuf::from("old-orphan")]);
        assert_eq!(selection.kept_recent, 1);
    }

    #[test]
    fn part_file_names() {
        assert!(is_part_file_name(&part_file_name(&key("ab", 1))));
        for bad in [
            "notes.part",
            "ABCDEF0123456789ABCDEF0123456789.part",
            &key("ab", 1),
        ] {
            assert!(!is_part_file_name(bad), "{bad}");
        }
    }

    #[test]
    fn report_is_ascii() {
        // 印到終端機與 journal：必須是英文
        let report = GcReport {
            deleted_blobs: 1,
            errors: 2,
            ..GcReport::default()
        };
        assert!(report.to_string().is_ascii());
    }

    // ==================== run_once：blobs 與 tmp ====================

    #[sqlx::test]
    async fn deletes_only_old_orphans(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let owner = user::create_user(&pool, "carol", "x").await.unwrap();
        let now = SystemTime::now();

        let referenced = key("00", 1);
        let old_orphan = key("00", 2);
        let new_orphan = key("01", 3);
        let future_orphan = key("02", 4);
        files_db::create_file(&pool, owner, None, "a.txt", 1, None, &referenced)
            .await
            .unwrap();
        put(&blob(&dir, &referenced), now - 2 * DAY);
        put(&blob(&dir, &old_orphan), now - 2 * DAY);
        put(&blob(&dir, &new_orphan), now - MINUTE);
        put(&blob(&dir, &future_orphan), now + DAY);

        let report = run_once(&pool, dir.path(), now, &GcPolicy::default()).await;

        assert!(
            blob(&dir, &referenced).exists(),
            "有資料庫紀錄的 blob 不能刪"
        );
        assert!(!blob(&dir, &old_orphan).exists(), "超過寬限期的孤兒要刪");
        assert!(blob(&dir, &new_orphan).exists(), "寬限期內的孤兒要保留");
        assert!(blob(&dir, &future_orphan).exists(), "mtime 在未來的要保留");
        assert_eq!(report.deleted_blobs, 1);
        assert_eq!(report.kept_recent, 2);
        assert_eq!(report.errors, 0);
    }

    #[sqlx::test]
    async fn skips_unknown_and_misplaced_items(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - 2 * DAY;
        let blobs = storage::blobs_dir(dir.path());

        let readme = blobs.join("00").join("README");
        let notes = blobs.join("notes.txt");
        let bad_shard = blobs.join("zz").join(key("00", 1));
        // 合法 key 放錯分層：開頭是 aa，卻放在 bb/
        let misplaced = blobs.join("bb").join(key("aa", 1));
        put(&readme, old);
        put(&notes, old);
        put(&bad_shard, old);
        put(&misplaced, old);

        let report = run_once(&pool, dir.path(), SystemTime::now(), &GcPolicy::default()).await;

        for path in [&readme, &notes, &bad_shard, &misplaced] {
            assert!(path.exists(), "不認得的項目不能刪：{}", path.display());
        }
        assert_eq!(report.deleted_blobs, 0);
        // README、notes.txt、zz/ 目錄、bb/ 裡放錯的檔案
        assert_eq!(report.skipped_unknown, 4);
    }

    #[sqlx::test]
    #[cfg(unix)]
    async fn never_follows_symlinks(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious.txt");
        put(&target, SystemTime::now() - 2 * DAY);

        // 在分層目錄裡放一個名稱是合法 key 的符號連結，指向外部檔案
        let link = blob(&dir, &key("00", 1));
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let report = run_once(&pool, dir.path(), SystemTime::now(), &GcPolicy::default()).await;

        assert!(target.exists(), "符號連結指向的外部檔案絕不能被刪");
        assert!(link.symlink_metadata().is_ok(), "符號連結本身也不處理");
        assert_eq!(report.deleted_blobs, 0);
        assert_eq!(report.skipped_unknown, 1);
    }

    #[sqlx::test]
    async fn cleans_stale_temp_files(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();

        let stale = tmp(&dir, &format!("{}.part", key("00", 1)));
        let active = tmp(&dir, &format!("{}.part", key("00", 2)));
        let unknown = tmp(&dir, "random.txt");
        put(&stale, now - 2 * DAY);
        put(&active, now - MINUTE);
        put(&unknown, now - 2 * DAY);

        let report = run_once(&pool, dir.path(), now, &GcPolicy::default()).await;

        assert!(!stale.exists(), "超過 24 小時沒寫入的暫存檔要刪");
        assert!(active.exists(), "還在寫入的暫存檔要保留");
        assert!(unknown.exists(), "不是 TempBlob 產生的檔案不能刪");
        assert_eq!(report.deleted_tmp, 1);
        assert_eq!(report.kept_recent, 1);
        assert_eq!(report.skipped_unknown, 1);
    }

    // ==================== run_once：分段上傳 ====================

    #[sqlx::test]
    async fn expires_abandoned_uploads_and_their_part_files(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let owner = user::create_user(&pool, "carol", "x").await.unwrap();
        let now = SystemTime::now();

        let abandoned = key("aa", 1);
        let active = key("aa", 2);
        let future = key("aa", 3);
        make_upload(&pool, owner, &abandoned, now - 2 * DAY).await;
        make_upload(&pool, owner, &active, now - MINUTE).await;
        // 時鐘曾經跳動：updated_at 在未來
        make_upload(&pool, owner, &future, now + DAY).await;
        for id in [&abandoned, &active, &future] {
            put(&part(&dir, id), now - 2 * DAY);
        }

        let report = run_once(&pool, dir.path(), now, &GcPolicy::default()).await;

        assert!(!upload_exists(&pool, &abandoned).await, "放棄的紀錄要刪");
        assert!(!part(&dir, &abandoned).exists(), "放棄的暫存檔要刪");
        // 有紀錄的暫存檔：即使 mtime 很舊（例如很久沒寫入但最近查詢過進度）也不是孤兒
        assert!(upload_exists(&pool, &active).await);
        assert!(part(&dir, &active).exists());
        assert!(
            upload_exists(&pool, &future).await,
            "updated_at 在未來的要保留"
        );
        assert!(part(&dir, &future).exists());
        assert_eq!(report.expired_uploads, 1);
        assert_eq!(report.deleted_parts, 1);
        assert_eq!(report.errors, 0);
    }

    #[sqlx::test]
    async fn deletes_only_old_part_files_without_records(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();

        let old_orphan = part(&dir, &key("bb", 1));
        // 剛建立、紀錄還沒寫入（宣告時「先檔案、後紀錄」之間的空窗）
        let new_orphan = part(&dir, &key("bb", 2));
        let unknown = storage::uploads_dir(dir.path()).join("notes.txt");
        put(&old_orphan, now - 2 * DAY);
        put(&new_orphan, now - MINUTE);
        put(&unknown, now - 2 * DAY);

        let report = run_once(&pool, dir.path(), now, &GcPolicy::default()).await;

        assert!(!old_orphan.exists(), "超過寬限期、沒有紀錄的暫存檔要刪");
        assert!(new_orphan.exists(), "寬限期內的要保留");
        assert!(unknown.exists(), "不認得的檔案不能刪");
        assert_eq!(report.deleted_parts, 1);
        assert_eq!(report.kept_recent, 1);
        assert_eq!(report.skipped_unknown, 1);
    }

    // ==================== run_once：其他 ====================

    #[sqlx::test]
    async fn deletes_expired_sessions(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let owner = user::create_user(&pool, "carol", "x").await.unwrap();
        let now = SystemTime::now();
        let now_secs = unix_secs(now).unwrap();
        session::create_session(&pool, owner, &[1u8; 32], now_secs - 10)
            .await
            .unwrap();
        session::create_session(&pool, owner, &[2u8; 32], now_secs + 3600)
            .await
            .unwrap();

        let report = run_once(&pool, dir.path(), now, &GcPolicy::default()).await;

        assert_eq!(report.expired_sessions, 1);
        assert!(
            session::find_user_by_token_hash(&pool, &[2u8; 32])
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    async fn missing_directories_are_not_errors(pool: SqlitePool) {
        // 全新的資料目錄：blobs/、tmp/、uploads/ 都還不存在
        let dir = tempfile::tempdir().unwrap();

        let report = run_once(&pool, dir.path(), SystemTime::now(), &GcPolicy::default()).await;

        assert_eq!(report, GcReport::default());
    }

    #[sqlx::test]
    async fn database_failure_never_deletes_anything(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - 2 * DAY;
        let orphan_blob = key("00", 1);
        let orphan_part = part(&dir, &key("cc", 1));
        put(&blob(&dir, &orphan_blob), old);
        put(&orphan_part, old);

        // 模擬資料庫故障：關閉連線池後任何查詢都會失敗
        pool.close().await;
        let report = run_once(&pool, dir.path(), SystemTime::now(), &GcPolicy::default()).await;

        // 查不到資料庫時，就算看起來是孤兒也絕不刪除
        assert!(blob(&dir, &orphan_blob).exists());
        assert!(orphan_part.exists());
        assert_eq!(report.deleted_blobs, 0);
        assert_eq!(report.deleted_parts, 0);
        assert!(report.errors >= 1);
    }
}
