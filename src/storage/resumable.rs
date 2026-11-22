//! 分段上傳的暫存檔：{空間的根目錄}/.manorcloud/uploads/{id}.part。
//!
//! 這裡的 `dir` 一律是那個 uploads 資料夾本身。
//!
//! ## 不變式：資料庫記錄的「已確認長度」（uploads.received）之前的內容一定已經落盤
//!
//! - 寫入時：先寫檔案並 fsync，呼叫端才推進已確認長度。斷電在兩步之間時，
//!   檔案中多出一段沒被承認的資料，無害：之後會被重送的同一段內容覆蓋
//! - 檔案比已確認的還短：不可能是正常流程造成的，回報 Damaged，絕不在缺了一段的檔案上繼續
//!
//! 建立時同樣先檔案、後資料庫：create 讓「檔案存在」這件事落盤之後，呼叫端才寫入紀錄。
//! 建立時就預留整個檔案的空間：段落可以不按順序到達，檔案在硬碟上仍是連續的。
//!
//! ## 同時寫入多段
//!
//! 同一個上傳可以同時有好幾個請求，各自寫在檔案中自己的位置（共用鎖）。
//! 同一個位置的內容永遠相同（來自同一個檔案），重送的段落只是寫入相同的位元組。
//! 哪些段落已經落盤、已確認長度可以推進到哪裡，由呼叫端記錄（見 web::upload_progress）。
//!
//! 完成時需要獨佔鎖：還有請求在寫入時回報 Busy，不會把寫到一半的檔案放到正式名稱上。
//! 拿到獨佔鎖之後，呼叫端才讀取資料庫的已確認長度，確認已經傳完。
//!
//! 鎖跟著開啟的檔案走：被丟棄時（包括請求中途斷線、程式當掉）作業系統自動釋放。

use std::fmt;
use std::io::{self, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};

use super::is_valid_key;
use crate::tree::{self, OnConflict};

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
pub fn part_path(dir: &Path, id: &str) -> Option<PathBuf> {
    is_valid_key(id).then(|| dir.join(format!("{id}.part")))
}

/// 宣告上傳時建立空的暫存檔。
///
/// create_new：檔案已存在就失敗，而且不跟隨符號連結。
/// 之後同步目錄，讓「檔案存在」這件事落盤：呼叫端接著才寫入資料庫紀錄，
/// 斷電後不會出現「有紀錄、沒有檔案」。
///
/// `size` 是宣告的總大小：盡量一次預留好空間（見 preallocate）。
pub async fn create_part(dir: &Path, id: &str, size: u64) -> Result<(), PartError> {
    let path = part_path(dir, id).ok_or(PartError::InvalidId)?;
    fs::create_dir_all(dir).await?;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await?;
    preallocate(&file, size).await;
    sync_dir(dir).await?;
    Ok(())
}

/// 預留整個檔案的空間：段落不按順序到達時，檔案在硬碟上仍然盡量連續，之後讀取不必跳來跳去。
///
/// 只是最佳化：檔案系統不支援（例如 ZFS）或不是 Linux 時什麼都不做，
/// 檔案隨寫入長大，結果一樣正確。不用 posix_fallocate：不支援時它會改成逐塊寫入零，非常慢
async fn preallocate(file: &File, size: u64) {
    #[cfg(target_os = "linux")]
    {
        let fd = file.as_raw_fd();
        let Ok(len) = i64::try_from(size) else {
            return;
        };
        if len == 0 {
            return;
        }
        // 檔案在等待期間一直開著，fd 不會失效
        let _ = tokio::task::spawn_blocking(move || {
            // SAFETY: fd 來自仍然開啟中的檔案，見上面
            unsafe { libc::fallocate(fd, 0, 0, len) }
        })
        .await;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (file, size);
}

/// 取消上傳時刪除暫存檔。本來就不存在也算成功（目標是「不存在」）
pub async fn remove_part(dir: &Path, id: &str) -> Result<(), PartError> {
    let path = part_path(dir, id).ok_or(PartError::InvalidId)?;
    match fs::remove_file(&path).await {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

// ==================== 第一步：取得鎖 ====================

/// 持有鎖的暫存檔：寫入用共用鎖（`acquire_shared` → `write_at`），
/// 完成用獨佔鎖（`acquire` → `resume` → `finish`）。
///
/// 拿到它之後，呼叫端才讀取資料庫的已確認長度。
pub struct PartLock {
    file: File,
    path: PathBuf,
}

impl PartLock {
    /// 開啟暫存檔並取得獨佔鎖（完成時使用）；有人在寫入時立刻回報 Busy，不等待
    pub async fn acquire(dir: &Path, id: &str) -> Result<PartLock, PartError> {
        Self::open(dir, id, libc::LOCK_EX).await
    }

    /// 開啟暫存檔並取得共用鎖（寫入時使用）：好幾個請求可以同時寫不同的段落；
    /// 正在完成時立刻回報 Busy
    pub async fn acquire_shared(dir: &Path, id: &str) -> Result<PartLock, PartError> {
        Self::open(dir, id, libc::LOCK_SH).await
    }

    async fn open(dir: &Path, id: &str, mode: libc::c_int) -> Result<PartLock, PartError> {
        let path = part_path(dir, id).ok_or(PartError::InvalidId)?;

        let file = match OpenOptions::new().write(true).open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(PartError::NotFound),
            Err(e) => return Err(e.into()),
        };
        let file = lock_current(file, &path, mode).await?;

        Ok(PartLock { file, path })
    }

    /// 準備從 `offset` 開始寫入一段（不截斷：其他請求可能正在寫後面的段落）。
    ///
    /// `confirmed` 是拿到鎖之後從資料庫讀出的已確認長度，`limit` 是宣告的總大小。
    pub async fn write_at(
        self,
        confirmed: u64,
        offset: u64,
        limit: u64,
    ) -> Result<PartFile, PartError> {
        let mut file = self.file;

        let actual = file.metadata().await?.len();
        if actual < confirmed {
            return Err(PartError::Damaged {
                expected: confirmed,
                actual,
            });
        }
        file.seek(SeekFrom::Start(offset)).await?;

        Ok(PartFile {
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
            path: self.path,
            len: offset,
            limit,
            written_back: offset,
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
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
            path: self.path,
            len: confirmed,
            limit,
            written_back: confirmed,
        })
    }
}

/// 對已開啟的檔案取得獨佔鎖，並確認它仍然是路徑現在指向的那個檔案。
///
/// 另一個請求可能在 finish 之前開啟了檔案、之後才拿到鎖：
/// 那時它握著的是已經放到正式名稱上的完成檔案。比對裝置與 inode 編號，不同就當作暫存檔已經不在。
async fn lock_current(file: File, path: &Path, mode: libc::c_int) -> Result<File, PartError> {
    lock(&file, mode)?;

    let opened = file.metadata().await?;
    match fs::symlink_metadata(path).await {
        Ok(current) if current.dev() == opened.dev() && current.ino() == opened.ino() => Ok(file),
        Ok(_) => Err(PartError::NotFound),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(PartError::NotFound),
        Err(e) => Err(e.into()),
    }
}

/// 取得檔案鎖（LOCK_EX 或 LOCK_SH），拿不到時立刻回報 Busy，不等待
fn lock(file: &File, mode: libc::c_int) -> Result<(), PartError> {
    // SAFETY: fd 來自仍然開啟中的 File，在這次呼叫期間一定有效
    let rc = unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) };
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

/// 收到的資料先累積到這麼多才寫入檔案：網路送來的是很多小塊，一塊一塊寫浪費系統呼叫
const WRITE_BUFFER: usize = 1024 * 1024;

/// 每寫入這麼多，就請系統在背景開始寫進硬碟（不等待）。
/// 最後 fsync 時剩下要寫的就不多，不會在每段結束時才一次寫完整段、讓上傳停下來等
const WRITE_BACK_EVERY: u64 = 8 * 1024 * 1024;

/// 持有獨佔鎖、已經截斷到已確認長度、可以寫入的暫存檔
pub struct PartFile {
    file: BufWriter<File>,
    path: PathBuf,
    /// 目前寫到的位置（開始時是這一段的起點，之後隨寫入增加）
    len: u64,
    /// 宣告的總大小：不能寫超過它
    limit: u64,
    /// 已經請系統開始寫進硬碟的位置
    written_back: u64,
}

impl PartFile {
    /// 目前寫到的位置
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
        if self.len - self.written_back >= WRITE_BACK_EVERY {
            self.start_write_back().await?;
        }
        Ok(())
    }

    /// 請系統在背景把目前為止的資料寫進硬碟，不等它完成。
    /// 只是提示：做不到（例如不是 Linux）也不影響正確性，fsync 仍然在 sync 與 finish 裡
    async fn start_write_back(&mut self) -> io::Result<()> {
        self.file.flush().await?;
        #[cfg(target_os = "linux")]
        {
            let fd = self.file.get_ref().as_raw_fd();
            let (offset, len) = (
                self.written_back as i64,
                (self.len - self.written_back) as i64,
            );
            // 硬碟忙的時候可能要等一下：不要卡住處理其他請求的執行緒。
            // 等待期間 self.file 一直開著，fd 不會失效
            let _ = tokio::task::spawn_blocking(move || {
                // SAFETY: fd 來自仍然開啟中的檔案，見上面
                unsafe { libc::sync_file_range(fd, offset, len, libc::SYNC_FILE_RANGE_WRITE) }
            })
            .await;
        }
        self.written_back = self.len;
        Ok(())
    }

    /// 讓寫入的資料落盤，回傳目前寫到的位置。之後呼叫端才可以把這一段算進已確認的長度。
    ///
    /// sync_data（fdatasync）就足夠：它會一併寫出讀回資料所需的中繼資料，包括檔案大小。
    pub async fn sync(&mut self) -> Result<u64, PartError> {
        self.file.flush().await?;
        self.file.get_mut().sync_data().await?;
        Ok(self.len)
    }

    /// 完成上傳：確認長度等於宣告的總大小，放到 `dest_dir` 中的 `name`，回傳實際使用的名稱。
    /// 規則與一般上傳相同，見 `tree::place`
    pub async fn finish(
        mut self,
        dest_dir: &Path,
        name: &str,
        on_conflict: OnConflict,
    ) -> Result<String, PartError> {
        if self.len != self.limit {
            return Err(PartError::Incomplete {
                expected: self.limit,
                actual: self.len,
            });
        }
        self.file.flush().await?;
        self.file.get_mut().sync_all().await?;

        let path = self.path.clone();
        let dest_dir = dest_dir.to_path_buf();
        let name = name.to_string();
        let placed =
            tokio::task::spawn_blocking(move || tree::place(&path, &dest_dir, &name, on_conflict))
                .await
                .map_err(io::Error::other)??;
        // 鎖在 self.file 被丟棄時才釋放：那時暫存檔的名稱已經不在了
        drop(self.file);
        Ok(placed)
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

        create_part(dir.path(), ID, 0).await.unwrap();

        assert_eq!(read(&part(dir.path())).await, b"");
    }

    #[tokio::test]
    async fn create_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();

        assert!(create_part(dir.path(), ID, 0).await.is_err());
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();

        for bad in ["../../etc/passwd", "ABCDEF0123456789ABCDEF0123456789", "x"] {
            assert!(matches!(
                create_part(dir.path(), bad, 0).await,
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
        create_part(dir.path(), ID, 0).await.unwrap();

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
        create_part(dir.path(), ID, 0).await.unwrap();

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
        create_part(dir.path(), ID, 0).await.unwrap();

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
        create_part(dir.path(), ID, 0).await.unwrap();

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
        create_part(dir.path(), ID, 0).await.unwrap();

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
        create_part(dir.path(), ID, 0).await.unwrap();
        let mut file = open(dir.path(), 0, 5).await.unwrap();
        file.write_chunk(b"hello").await.unwrap();
        file.sync().await.unwrap();

        // 另一個請求在改名之前開啟了暫存檔，但還沒拿到鎖
        let early = OpenOptions::new()
            .write(true)
            .open(part(dir.path()))
            .await
            .unwrap();

        // 完成：暫存檔放到正式的名稱上，鎖隨之釋放
        let dest = tempfile::tempdir().unwrap();
        file.finish(dest.path(), "a.txt", OnConflict::Rename)
            .await
            .unwrap();

        // 它現在拿得到鎖，但握著的是完成的檔案：必須當作暫存檔已經不在
        let result = lock_current(early, &part(dir.path()), libc::LOCK_EX).await;
        assert!(matches!(result, Err(PartError::NotFound)));
        // 完成的檔案沒有被動到
        assert_eq!(read(&dest.path().join("a.txt")).await, b"hello");
    }

    // ==================== 同時寫入多段 ====================

    #[tokio::test]
    async fn creating_reserves_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();

        create_part(dir.path(), ID, 1000).await.unwrap();

        // 支援預留空間的檔案系統（Linux 的 ext4、tmpfs 等）：一開始就是完整的長度
        if cfg!(target_os = "linux") {
            assert_eq!(read(&part(dir.path())).await.len(), 1000);
        }
    }

    #[tokio::test]
    async fn segments_can_be_written_out_of_order() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 11).await.unwrap();

        // 後面那段先寫，兩個請求同時握著共用鎖
        let mut second = PartLock::acquire_shared(dir.path(), ID)
            .await
            .unwrap()
            .write_at(0, 6, 11)
            .await
            .unwrap();
        let mut first = PartLock::acquire_shared(dir.path(), ID)
            .await
            .unwrap()
            .write_at(0, 0, 11)
            .await
            .unwrap();
        second.write_chunk(b"world").await.unwrap();
        assert_eq!(second.sync().await.unwrap(), 11);
        first.write_chunk(b"hello ").await.unwrap();
        assert_eq!(first.sync().await.unwrap(), 6);
        drop((first, second));

        assert_eq!(read(&part(dir.path())).await, b"hello world");
    }

    #[tokio::test]
    async fn writers_share_the_lock_but_block_finishing() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();

        let a = PartLock::acquire_shared(dir.path(), ID).await.unwrap();
        let b = PartLock::acquire_shared(dir.path(), ID).await.unwrap();
        // 還有人在寫：不能完成
        assert!(matches!(
            PartLock::acquire(dir.path(), ID).await,
            Err(PartError::Busy)
        ));

        drop((a, b));
        let finishing = PartLock::acquire(dir.path(), ID).await.unwrap();
        // 正在完成：不能再寫
        assert!(matches!(
            PartLock::acquire_shared(dir.path(), ID).await,
            Err(PartError::Busy)
        ));
        drop(finishing);
    }

    #[tokio::test]
    async fn writing_never_truncates() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();
        let mut file = open(dir.path(), 0, 11).await.unwrap();
        file.write_chunk(b"hello world").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        // 只確認了前 6 個位元組，從 6 開始寫一段較短的：後面別人寫的資料不受影響
        let mut file = PartLock::acquire_shared(dir.path(), ID)
            .await
            .unwrap()
            .write_at(6, 6, 11)
            .await
            .unwrap();
        file.write_chunk(b"W").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        assert_eq!(read(&part(dir.path())).await, b"hello World");
    }

    // ==================== 完成與取消 ====================

    #[tokio::test]
    async fn finish_places_the_part_under_its_name() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();
        let mut file = open(dir.path(), 0, 5).await.unwrap();
        file.write_chunk(b"hello").await.unwrap();
        file.sync().await.unwrap();
        drop(file);

        let dest = tempfile::tempdir().unwrap();
        std::fs::write(dest.path().join("a.txt"), b"old").unwrap();
        let file = open(dir.path(), 5, 5).await.unwrap();
        let name = file
            .finish(dest.path(), "a.txt", OnConflict::Rename)
            .await
            .unwrap();

        assert_eq!(name, "a (1).txt");
        assert_eq!(read(&dest.path().join("a (1).txt")).await, b"hello");
        assert!(!part(dir.path()).exists());
    }

    #[tokio::test]
    async fn incomplete_upload_cannot_finish() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        let file = open(dir.path(), 0, 5).await.unwrap();
        let result = file.finish(dest.path(), "a.txt", OnConflict::Rename).await;

        assert!(matches!(
            result,
            Err(PartError::Incomplete {
                expected: 5,
                actual: 0
            })
        ));
        assert!(!dest.path().join("a.txt").exists());
        assert!(part(dir.path()).exists());
    }

    #[tokio::test]
    async fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        create_part(dir.path(), ID, 0).await.unwrap();

        remove_part(dir.path(), ID).await.unwrap();
        assert!(!part(dir.path()).exists());
        // 已經不存在：仍然成功
        remove_part(dir.path(), ID).await.unwrap();
    }
}
