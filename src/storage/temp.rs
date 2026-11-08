//! 上傳暫存檔的守衛：保證失敗時一定清掉暫存檔。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::{TMP_DIR, blob_path, generate_storage_key};

/// 單一檔案的預設大小上限：10 GiB。
/// 串流寫入時記憶體用量與檔案大小無關，這個上限保護的是磁碟空間。
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// 儲存層的錯誤
#[derive(Debug)]
pub enum StorageError {
    /// 超過大小上限
    TooLarge { limit: u64 },
    /// storage_key 格式不正確
    InvalidKey,
    /// 檔案系統錯誤（包含磁碟已滿，可用 `io::ErrorKind::StorageFull` 判斷）
    Io(io::Error),
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        StorageError::Io(e)
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::TooLarge { limit } => {
                write!(f, "file exceeds the size limit of {limit} bytes")
            }
            StorageError::InvalidKey => f.write_str("invalid storage key"),
            StorageError::Io(e) => write!(f, "file system error: {e}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// 上傳中的暫存檔。
///
/// 被丟棄時會自動刪除暫存檔，除非已呼叫 `persist`。
/// 因此任何失敗路徑（超過上限、磁碟滿、資料庫錯誤）都不需要手動清理。
///
/// 也包含「取消」：使用者上傳到一半斷線時，axum 會直接丟棄 handler 的 future，
/// `.await` 之後的程式碼完全不會執行，但 Drop 照樣會執行，所以斷線也能清乾淨。
pub struct TempBlob {
    /// 開啟中的檔案；只有在 persist 或 Drop 時才會被取走
    file: Option<File>,
    /// 暫存檔路徑：{data_dir}/tmp/{隨機}.part
    path: PathBuf,
    data_dir: PathBuf,
    size: u64,
    max_bytes: u64,
    /// 暫存檔已不在原位（已搬到正式位置），Drop 時不要刪除
    persisted: bool,
}

impl TempBlob {
    /// 在 `{data_dir}/tmp/` 建立一個隨機名稱的暫存檔
    pub async fn create(data_dir: &Path, max_bytes: u64) -> Result<Self, StorageError> {
        let tmp_dir = data_dir.join(TMP_DIR);
        fs::create_dir_all(&tmp_dir).await?;

        let path = tmp_dir.join(format!("{}.part", generate_storage_key()));

        // create_new：檔案已存在就失敗，且不跟隨符號連結（O_EXCL）
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;

        Ok(TempBlob {
            file: Some(file),
            path,
            data_dir: data_dir.to_path_buf(),
            size: 0,
            max_bytes,
            persisted: false,
        })
    }

    /// 寫入一塊資料。寫入前先檢查大小，超過上限就不寫並回傳錯誤，
    /// 所以暫存檔永遠不會超過上限。
    pub async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), StorageError> {
        let new_size = self.size.saturating_add(chunk.len() as u64);
        if new_size > self.max_bytes {
            return Err(StorageError::TooLarge {
                limit: self.max_bytes,
            });
        }

        let file = self
            .file
            .as_mut()
            .expect("the TempBlob file always exists before persist");
        file.write_all(chunk).await?;
        self.size = new_size;

        Ok(())
    }

    /// 目前已寫入的位元組數
    pub fn size(&self) -> u64 {
        self.size
    }

    /// 完成上傳：確保資料寫進磁碟，再原子地搬到正式位置，回傳檔案大小。
    ///
    /// 回傳成功時，檔案內容與「它位於正式位置」這件事都已經落盤，
    /// 之後寫入資料庫才是安全的：不會出現「資料庫有紀錄、重開機後 blob 卻不見」。
    ///
    /// 取走 `self` 的所有權：呼叫之後原本的變數不能再使用，
    /// 「persist 之後又繼續寫」會在編譯時就被擋下。
    /// 任何一步失敗時，`self` 在函式結束時被丟棄，Drop 會清掉暫存檔。
    pub async fn persist(mut self, key: &str) -> Result<u64, StorageError> {
        let dest = blob_path(&self.data_dir, key).ok_or(StorageError::InvalidKey)?;
        let shard_dir = dest
            .parent()
            .expect("blob_path always has a parent directory")
            .to_path_buf();

        // 1. 先 flush：tokio::fs::File 的 write_all 回傳時資料不一定已寫出，
        //    flush 讓背景中的寫入全部完成，再轉成標準庫的 File。
        let mut file = self
            .file
            .take()
            .expect("the TempBlob file always exists before persist");
        file.flush().await?;
        let file = file.into_std().await;

        // 2. 把 mtime 設成「現在」，再把內容與中繼資料一起落盤（阻塞操作，放到專用執行緒）。
        //    rename 不會改變 mtime；若不重設，blob 的 mtime 是「最後一次寫入」的時間。
        //    上傳內容傳完後若 multipart 的結尾邊界延遲很久才到，persist 時 mtime 可能已超過
        //    垃圾回收的寬限期，GC 若剛好在 persist 與寫入資料庫之間掃到它就會誤刪，
        //    違反「資料庫有的 key，磁碟上一定有 blob」的不變式。
        //    重設後 blob 的 mtime 精確等於 persist 的時間，寬限期不再依賴上傳的節奏。
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            file.set_modified(SystemTime::now())?;
            file.sync_all()
        })
        .await
        .map_err(io::Error::other)??;

        // 3. 建立分層目錄（例如 blobs/ab/）
        fs::create_dir_all(&shard_dir).await?;

        // 4. 同一檔案系統內的 rename 是原子操作：要嘛完全沒發生，要嘛完全完成
        fs::rename(&self.path, &dest).await?;

        // 暫存檔已經不在 tmp/ 了，Drop 不需要再刪。
        // 若下一步失敗，blob 會成為沒人參照的孤兒，由垃圾回收清除；
        // 因為資料庫尚未寫入，不會產生指向不存在檔案的紀錄。
        self.persisted = true;

        // 5. rename 修改的是「目錄」的內容，必須 sync 目錄才能保證斷電後仍然生效。
        //    分層目錄可能是這次才建立的，它在 blobs/ 中的紀錄也要一併落盤。
        sync_dir(&shard_dir).await?;
        if let Some(blobs_dir) = shard_dir.parent() {
            sync_dir(blobs_dir).await?;
        }

        Ok(self.size)
    }
}

/// 對目錄做 fsync，讓目錄內的新增、改名紀錄落盤。
///
/// 只在 Unix 上執行：Linux 與 macOS 可以開啟目錄並 sync；
/// Windows 不支援以這種方式開啟目錄，NTFS 的中繼資料也由日誌另行保護。
async fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir).await?.sync_all().await?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

impl Drop for TempBlob {
    fn drop(&mut self) {
        if self.persisted {
            return;
        }

        // 先關閉檔案（Windows 無法刪除開啟中的檔案；Unix 則不受影響）
        self.file.take();

        // Drop 不能是 async，所以用同步的 std::fs。刪除單一檔案非常快，可以接受。
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != io::ErrorKind::NotFound
        {
            // 刪除失敗只記錄，不能 panic：Drop 中 panic 可能讓整個程式中止。
            // 殘留的暫存檔之後由垃圾回收清除。
            eprintln!(
                "failed to delete the temporary file {}: {e}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    /// tmp/ 目錄中的檔案數量（目錄不存在視為 0）
    fn tmp_count(data_dir: &Path) -> usize {
        std::fs::read_dir(data_dir.join(TMP_DIR))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn persist_moves_blob_into_place() {
        let dir = tempfile::tempdir().unwrap();

        let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
        blob.write_chunk(b"hello ").await.unwrap();
        blob.write_chunk(b"world").await.unwrap();
        assert_eq!(tmp_count(dir.path()), 1);

        let size = blob.persist(KEY).await.unwrap();

        assert_eq!(size, 11);
        let dest = blob_path(dir.path(), KEY).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello world");
        assert_eq!(tmp_count(dir.path()), 0);
    }

    #[tokio::test]
    async fn dropping_without_persist_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();

        let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
        blob.write_chunk(b"partial").await.unwrap();
        assert_eq!(tmp_count(dir.path()), 1);

        drop(blob);

        assert_eq!(tmp_count(dir.path()), 0);
    }

    #[tokio::test]
    async fn exactly_at_limit_is_allowed() {
        let dir = tempfile::tempdir().unwrap();

        let mut blob = TempBlob::create(dir.path(), 5).await.unwrap();
        blob.write_chunk(b"hello").await.unwrap();

        assert_eq!(blob.persist(KEY).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn exceeding_limit_fails_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();

        let mut blob = TempBlob::create(dir.path(), 5).await.unwrap();
        blob.write_chunk(b"hello").await.unwrap();
        let result = blob.write_chunk(b"!").await;

        assert!(matches!(result, Err(StorageError::TooLarge { limit: 5 })));
        // 超限的那一塊沒有被寫入
        assert_eq!(blob.size(), 5);

        drop(blob);
        assert_eq!(tmp_count(dir.path()), 0);
    }

    #[tokio::test]
    async fn invalid_key_fails_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();

        let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
        blob.write_chunk(b"data").await.unwrap();
        let result = blob.persist("../../etc/passwd").await;

        assert!(matches!(result, Err(StorageError::InvalidKey)));
        // persist 失敗時 self 已在函式內被丟棄，暫存檔應該已經清掉
        assert_eq!(tmp_count(dir.path()), 0);
    }

    #[tokio::test]
    async fn persist_into_existing_shard_directory() {
        // 同一個分層目錄連續放入兩個 blob：第二次 create_dir_all 與目錄 sync 都要正常運作
        let dir = tempfile::tempdir().unwrap();
        let key_a = "ab000000000000000000000000000001";
        let key_b = "ab000000000000000000000000000002";

        for key in [key_a, key_b] {
            let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
            blob.write_chunk(key.as_bytes()).await.unwrap();
            blob.persist(key).await.unwrap();
        }

        assert!(blob_path(dir.path(), key_a).unwrap().exists());
        assert!(blob_path(dir.path(), key_b).unwrap().exists());
    }

    /// rename 只在同一個檔案系統內是原子操作；跨檔案系統會直接失敗（EXDEV）
    #[tokio::test]
    #[cfg(unix)]
    async fn tmp_and_blobs_are_on_same_filesystem() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
        blob.write_chunk(b"x").await.unwrap();
        blob.persist(KEY).await.unwrap();

        let tmp_dev = std::fs::metadata(dir.path().join(TMP_DIR)).unwrap().dev();
        let blob_dev = std::fs::metadata(blob_path(dir.path(), KEY).unwrap())
            .unwrap()
            .dev();

        assert_eq!(tmp_dev, blob_dev);
    }
    #[tokio::test]
    async fn persist_resets_mtime_to_now() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let mut blob = TempBlob::create(dir.path(), 1024).await.unwrap();
        blob.write_chunk(b"data").await.unwrap();
        blob.file.as_mut().unwrap().flush().await.unwrap();

        // 模擬「最後一次寫入是兩小時前」：內容傳完後，結尾邊界延遲很久才到
        let two_hours_ago = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&blob.path)
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();

        blob.persist(KEY).await.unwrap();

        let mtime = std::fs::metadata(blob_path(dir.path(), KEY).unwrap())
            .unwrap()
            .modified()
            .unwrap();
        let age = SystemTime::now().duration_since(mtime).unwrap_or_default();
        // persist 之後 mtime 應該是「剛剛」，而不是兩小時前
        assert!(age < Duration::from_secs(60), "{age:?}");
    }

    #[tokio::test]
    async fn temp_file_name_is_recognized_by_gc() {
        // 暫存檔的命名格式必須與垃圾回收的辨識規則一致，否則殘留的暫存檔永遠不會被清除
        let dir = tempfile::tempdir().unwrap();
        let blob = TempBlob::create(dir.path(), 1024).await.unwrap();

        let name = blob.path.file_name().unwrap().to_str().unwrap();

        assert!(crate::storage::is_temp_file_name(name), "{name}");
    }
}
