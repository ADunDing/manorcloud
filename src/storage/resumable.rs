//! 分段上傳的暫存檔：{data_dir}/uploads/{id}.part。
//!
//! ## 不變式：檔案長度永遠不少於資料庫記錄的「已確認長度」（uploads.received）
//!
//! - 寫入時：先寫檔案並 fsync，呼叫端才更新資料庫。斷電在兩步之間時，
//!   檔案多出一段沒被承認的資料，無害
//! - 每次續寫前：截斷到已確認的長度，從那裡接著寫，多出來的部分直接丟掉
//! - 檔案比已確認的還短：不可能是正常流程造成的，回報 Damaged，絕不在缺了一段的檔案後面繼續接
//!
//! 建立時同樣先檔案、後資料庫：create 讓「檔案存在」這件事落盤之後，呼叫端才寫入紀錄。
//!
//! ## 先拿鎖，再讀已確認長度
//!
//! 兩個請求同時寫同一個暫存檔，內容會被交錯寫壞；資料庫的樂觀鎖只能保護進度數字，保護不了檔案內容。
//! 所以寫入前用 flock 取得獨佔鎖，拿不到就回報 Busy。
//!
//! 更重要的是順序：**拿到鎖之後**，呼叫端才讀取資料庫的已確認長度，再交給 `resume` 截斷。
//! 反過來的話，讀到的值在拿到鎖時可能已經過時（另一個請求剛推進了它），
//! 截斷會把別人剛確認的資料切掉，違反上面的不變式。
//! 因此開啟分成兩步：`PartLock::acquire` 只拿鎖，`PartLock::resume` 才截斷；
//! 不先拿到 PartLock，就拿不到可以寫入的 PartFile，不可能用錯順序。
//!
//! 鎖跟著開啟的檔案走：被丟棄時（包括請求中途斷線、程式當掉）作業系統自動釋放。

use std::fmt;
use std::io::{self, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::{UPLOADS_DIR, blob_path, is_valid_key};

#[derive(Debug)]
pub enum PartError {
    /// 上傳編號的格式不正確（與 storage_key 相同：32 個小寫 hex 字元）
    InvalidId,
    /// 暫存檔不存在（或已經被搬走）
    NotFound,
    /// 另一個請求正在寫入同一個上傳
    Busy,
    /// 暫存檔比已確認的長度還短：內容缺了一段，不能續寫
    Damaged {
        expected: u64,
        actual: u64,
    },
    /// 寫入後會超過宣告的總大小
    TooLarge {
        limit: u64,
    },
    /// 完成時長度與宣告的總大小不符
    Incomplete {
        expected: u64,
        actual: u64,
    },
    Io(io::Error),
}

impl From<io::Error> for PartError {
    fn from(e: io::Error) -> Self {
        PartError::Io(e)
    }
}

/// 只會出現在日誌中，所以使用英文；對使用者的訊息由 HTTP 層決定
impl fmt::Display for PartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PartError::InvalidId => f.write_str("invalid upload id"),
            PartError::NotFound => f.write_str("upload part file not found"),
            PartError::Busy => f.write_str("upload is being written by another request"),
            PartError::Damaged { expected, actual } => write!(
                f,
                "upload part file is shorter than confirmed ({actual} < {expected} bytes)"
            ),
            PartError::TooLarge { limit } => {
                write!(f, "upload would exceed its declared size of {limit} bytes")
            }
            PartError::Incomplete { expected, actual } => {
                write!(f, "upload is incomplete ({actual} of {expected} bytes)")
            }
            PartError::Io(e) => write!(f, "upload part file error: {e}"),
        }
    }
}

impl std::error::Error for PartError {}

/// 暫存檔的路徑。上傳編號必須是 32 個小寫 hex 字元，路徑不可能被拼出 ../
fn part_path(data_dir: &Path, id: &str) -> Option<PathBuf> {
    is_valid_key(id).then(|| data_dir.join(UPLOADS_DIR).join(format!("{id}.part")))
}

/// 宣告上傳時建立空的暫存檔。
///
/// create_new：檔案已存在就失敗，而且不跟隨符號連結。
/// 之後同步目錄，讓「檔案存在」這件事落盤：呼叫端接著才寫入資料庫紀錄，
/// 斷電後不會出現「有紀錄、沒有檔案」。
pub async fn create_part(data_dir: &Path, id: &str) -> Result<(), PartError> {
    let path = part_path(data_dir, id).ok_or(PartError::InvalidId)?;
    let dir = data_dir.join(UPLOADS_DIR);
    fs::create_dir_all(&dir).await?;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await?;
    sync_dir(&dir).await?;
    Ok(())
}

/// 取消上傳時刪除暫存檔。本來就不存在也算成功（目標是「不存在」）
pub async fn remove_part(data_dir: &Path, id: &str) -> Result<(), PartError> {
    let path = part_path(data_dir, id).ok_or(PartError::InvalidId)?;
    match fs::remove_file(&path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

// ==================== 第一步：取得鎖 ====================

/// 持有獨佔鎖、但還沒有截斷的暫存檔。
///
/// 拿到它之後，呼叫端才讀取資料庫的已確認長度，再呼叫 `resume`。
pub struct PartLock {
    file: File,
    path: PathBuf,
    data_dir: PathBuf,
}

impl PartLock {
    /// 開啟暫存檔並取得獨佔鎖；拿不到時立刻回報 Busy，不等待
    pub async fn acquire(data_dir: &Path, id: &str) -> Result<PartLock, PartError> {
        let path = part_path(data_dir, id).ok_or(PartError::InvalidId)?;

        let file = match OpenOptions::new().write(true).open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(PartError::NotFound),
            Err(e) => return Err(e.into()),
        };
        let file = lock_current(file, &path).await?;

        Ok(PartLock {
            file,
            path,
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// 截斷到已確認的長度並移到結尾，準備續寫或完成。
    ///
    /// `confirmed` 必須是**拿到鎖之後**才從資料庫讀出的值；`limit` 是宣告的總大小。
    pub async fn resume(self, confirmed: u64, limit: u64) -> Result<PartFile, PartError> {
        let mut file = self.file;

        let actual = file.metadata().await?.len();
        if actual < confirmed {
            return Err(PartError::Damaged {
                expected: confirmed,
                actual,
            });
        }
        // 多出來的是斷電前寫入、但還沒被資料庫承認的資料：丟掉，從已確認的位置接著寫
        if actual > confirmed {
            file.set_len(confirmed).await?;
        }
        file.seek(SeekFrom::Start(confirmed)).await?;

        Ok(PartFile {
            file,
            path: self.path,
            data_dir: self.data_dir,
            len: confirmed,
            limit,
        })
    }
}

/// 對已開啟的檔案取得獨佔鎖，並確認它仍然是路徑現在指向的那個檔案。
///
/// 另一個請求可能在 finish 改名之前開啟了檔案、改名之後才拿到鎖：
/// 那時它握著的是已經搬進 blobs/ 的完成檔案。比對裝置與 inode 編號，不同就當作暫存檔已經不在。
async fn lock_current(file: File, path: &Path) -> Result<File, PartError> {
    lock_exclusive(&file)?;

    let opened = file.metadata().await?;
    match fs::symlink_metadata(path).await {
        Ok(current) if current.dev() == opened.dev() && current.ino() == opened.ino() => Ok(file),
        Ok(_) => Err(PartError::NotFound),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(PartError::NotFound),
        Err(e) => Err(e.into()),
    }
}

/// 取得獨佔的檔案鎖，拿不到時立刻回報 Busy，不等待
fn lock_exclusive(file: &File) -> Result<(), PartError> {
    // SAFETY: fd 來自仍然開啟中的 File，在這次呼叫期間一定有效
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Err(PartError::Busy)
    } else {
        Err(e.into())
    }
}

// ==================== 第二步：續寫與完成 ====================

/// 持有獨佔鎖、已經截斷到已確認長度、可以寫入的暫存檔
pub struct PartFile {
    file: File,
    path: PathBuf,
    data_dir: PathBuf,
    /// 目前的長度（開始時等於已確認的長度，之後隨寫入增加）
    len: u64,
    /// 宣告的總大小：不能寫超過它
    limit: u64,
}

impl PartFile {
    /// 目前的長度
    pub fn len(&self) -> u64 {
        self.len
    }

    /// 追加一塊資料。寫入前先檢查大小：超過宣告的總大小時一個位元組都不寫
    pub async fn write_chunk(&mut self, data: &[u8]) -> Result<(), PartError> {
        let new_len = self.len.saturating_add(data.len() as u64);
        if new_len > self.limit {
            return Err(PartError::TooLarge { limit: self.limit });
        }
        self.file.write_all(data).await?;
        self.len = new_len;
        Ok(())
    }

    /// 讓寫入的資料落盤，回傳目前的長度。之後呼叫端才可以更新資料庫的已確認長度。
    ///
    /// sync_data（fdatasync）就足夠：它會一併寫出讀回資料所需的中繼資料，包括檔案大小。
    pub async fn sync(&mut self) -> Result<u64, PartError> {
        self.file.flush().await?;
        self.file.sync_data().await?;
        Ok(self.len)
    }

    /// 完成上傳：確認長度等於宣告的總大小，搬進 blobs/，與一般上傳的 blob 放在同一個地方。
    ///
    /// 與 TempBlob::persist 相同的落盤順序：檔案內容 → 改名 → 分層目錄 → blobs/。
    /// 呼叫端接著才寫入檔案紀錄：任何失敗都只會留下沒人參照的 blob，由垃圾回收清除。
    pub async fn finish(mut self, key: &str) -> Result<(), PartError> {
        if self.len != self.limit {
            return Err(PartError::Incomplete {
                expected: self.limit,
                actual: self.len,
            });
        }
        let dest = blob_path(&self.data_dir, key).ok_or(PartError::InvalidId)?;
        let shard_dir = dest
            .parent()
            .expect("blob_path always has a parent directory")
            .to_path_buf();

        self.file.flush().await?;
        self.file.sync_all().await?;

        fs::create_dir_all(&shard_dir).await?;
        fs::rename(&self.path, &dest).await?;

        sync_dir(&shard_dir).await?;
        if let Some(blobs_dir) = shard_dir.parent() {
            sync_dir(blobs_dir).await?;
        }
        Ok(())
    }
}

/// 同步目錄：讓目錄中的新增、改名紀錄落盤
async fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir).await?.sync_all().await
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "fedcba9876543210fedcba9876543210";

    async fn read(path: &Path) -> Vec<u8> {
        fs::read(path).await.unwrap()
    }

    fn part(dir: &Path) -> PathBuf {
        part_path(dir, ID).unwrap()
    }

    /// 取得鎖並截斷：正式流程中兩步之間呼叫端會讀資料庫，測試直接給定已確認長度
    async fn open(dir: &Path, confirmed: u64, limit: u64) -> Result<PartFile, PartError> {
        PartLock::acquire(dir, ID)
            .await?
            .resume(confirmed, limit)
            .await
    }

    // ==================== 建立 ====================

    #[tokio::test]
    async fn create_makes_an_empty_part_file() {
        let dir = tempfile::tempdir().unwrap();

        create_part(dir.path(), ID).await.unwrap();

        assert_eq!(read(&part(dir.path())).await, b"");
    }

    #[tokio::test]
    async fn create_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        assert!(create_part(dir.path(), ID).await.is_err());
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();

        for bad in ["../../etc/passwd", "ABCDEF0123456789ABCDEF0123456789", "x"] {
            assert!(matches!(
                create_part(dir.path(), bad).await,
                Err(PartError::InvalidId)
            ));
            assert!(matches!(
                PartLock::acquire(dir.path(), bad).await,
                Err(PartError::InvalidId)
            ));
        }
    }

    // ==================== 續寫 ====================

    #[tokio::test]
    async fn chunks_are_appended_and_synced() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        let mut file = open(dir.path(), 0, 100).await.unwrap();
        file.write_chunk(b"hello ").await.unwrap();
        assert_eq!(file.sync().await.unwrap(), 6);
        drop(file);

        // 下一個請求從已確認的 6 接著寫
        let mut file = open(dir.path(), 6, 100).await.unwrap();
        file.write_chunk(b"world").await.unwrap();
        assert_eq!(file.sync().await.unwrap(), 11);
        drop(file);

        assert_eq!(read(&part(dir.path())).await, b"hello world");
    }

    #[tokio::test]
    async fn unconfirmed_data_is_discarded_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        // 寫了 11 個位元組並落盤，但資料庫只確認了前 6 個（斷電在兩步之間）
        let mut file = open(dir.path(), 0, 100).await.unwrap();
        file.write_chunk(b"hello world").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        // 從已確認的位置重新送那一段
        let mut file = open(dir.path(), 6, 100).await.unwrap();
        assert_eq!(file.len(), 6);
        file.write_chunk(b"there").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        assert_eq!(read(&part(dir.path())).await, b"hello there");
    }

    #[tokio::test]
    async fn shorter_than_confirmed_is_damaged() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        // 資料庫說已經收到 10 個位元組，檔案卻是空的
        let result = open(dir.path(), 10, 100).await;

        assert!(matches!(
            result,
            Err(PartError::Damaged {
                expected: 10,
                actual: 0
            })
        ));
    }

    #[tokio::test]
    async fn missing_part_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            PartLock::acquire(dir.path(), ID).await,
            Err(PartError::NotFound)
        ));
    }

    #[tokio::test]
    async fn cannot_write_past_the_declared_size() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        let mut file = open(dir.path(), 0, 5).await.unwrap();
        file.write_chunk(b"hello").await.unwrap();
        let result = file.write_chunk(b"!").await;

        assert!(matches!(result, Err(PartError::TooLarge { limit: 5 })));
        // 超過的那一塊一個位元組都沒寫
        assert_eq!(file.sync().await.unwrap(), 5);
        drop(file);
        assert_eq!(read(&part(dir.path())).await, b"hello");
    }

    // ==================== 獨佔 ====================

    #[tokio::test]
    async fn only_one_holder_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        let first = PartLock::acquire(dir.path(), ID).await.unwrap();

        assert!(matches!(
            PartLock::acquire(dir.path(), ID).await,
            Err(PartError::Busy)
        ));

        // 第一個被丟棄（例如請求中途斷線）時，鎖自動釋放
        drop(first);
        assert!(PartLock::acquire(dir.path(), ID).await.is_ok());
    }

    #[tokio::test]
    async fn a_handle_opened_before_finish_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();
        let mut file = open(dir.path(), 0, 5).await.unwrap();
        file.write_chunk(b"hello").await.unwrap();
        file.sync().await.unwrap();

        // 另一個請求在改名之前開啟了暫存檔，但還沒拿到鎖
        let early = OpenOptions::new()
            .write(true)
            .open(part(dir.path()))
            .await
            .unwrap();

        // 完成：暫存檔被搬進 blobs/，鎖隨之釋放
        file.finish(KEY).await.unwrap();

        // 它現在拿得到鎖，但握著的是 blobs/ 裡完成的檔案：必須當作暫存檔已經不在
        let result = lock_current(early, &part(dir.path())).await;
        assert!(matches!(result, Err(PartError::NotFound)));
        // 完成的檔案沒有被動到
        assert_eq!(read(&blob_path(dir.path(), KEY).unwrap()).await, b"hello");
    }

    // ==================== 完成與取消 ====================

    #[tokio::test]
    async fn finish_moves_the_part_into_blobs() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();
        let mut file = open(dir.path(), 0, 5).await.unwrap();
        file.write_chunk(b"hello").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        let file = open(dir.path(), 5, 5).await.unwrap();
        file.finish(KEY).await.unwrap();

        assert_eq!(read(&blob_path(dir.path(), KEY).unwrap()).await, b"hello");
        assert!(!part(dir.path()).exists());
    }

    #[tokio::test]
    async fn incomplete_upload_cannot_finish() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        let file = open(dir.path(), 0, 5).await.unwrap();
        let result = file.finish(KEY).await;

        assert!(matches!(
            result,
            Err(PartError::Incomplete {
                expected: 5,
                actual: 0
            })
        ));
        assert!(!blob_path(dir.path(), KEY).unwrap().exists());
        assert!(part(dir.path()).exists());
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID).await.unwrap();

        remove_part(dir.path(), ID).await.unwrap();
        assert!(!part(dir.path()).exists());
        // 已經不存在：仍然成功
        remove_part(dir.path(), ID).await.unwrap();
    }
}
