//! 儲存層：決定 blob 在磁碟上的位置。
//!
//! 檔案內容以隨機的 storage_key 命名，使用者輸入永遠不會進入這裡的路徑。
//!
//! 目錄結構：
//!   {data_dir}/tmp/     上傳中的暫存檔
//!   {data_dir}/blobs/   完成的檔案，以 key 前兩字元分層
//! 兩者必須在同一個檔案系統，rename 才是原子操作。

mod temp;

pub mod resumable;

pub use temp::{DEFAULT_MAX_UPLOAD_BYTES, StorageError, TempBlob};

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// storage_key 的長度：128 位元以小寫 hex 編碼為 32 個字元
pub const KEY_LEN: usize = 32;

/// 暫存檔目錄名稱
const TMP_DIR: &str = "tmp";
/// 完成檔案的目錄名稱
const BLOBS_DIR: &str = "blobs";

/// 分段上傳的暫存檔目錄。與 tmp/ 分開：垃圾回收會刪除 tmp/ 中超過 24 小時的檔案，
/// 而分段上傳的暫存檔只要還有進度，就必須保留
const UPLOADS_DIR: &str = "uploads";

/// 啟動時呼叫：建立 `tmp/` 與 `blobs/`，並讓它們在 `data_dir` 中的紀錄落盤。
///
/// 在啟動時一次處理好，之後 `persist` 只需要負責分層目錄（blobs/ab/）的落盤，
/// 不必在每次上傳時都去 sync 最上層的 `data_dir`。
/// 已存在的目錄不受影響，可以重複呼叫。
pub fn init_dirs(data_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(data_dir.join(TMP_DIR))?;
    std::fs::create_dir_all(data_dir.join(BLOBS_DIR))?;
    std::fs::create_dir_all(data_dir.join(UPLOADS_DIR))?;

    // 讓 tmp/ 與 blobs/ 這兩筆目錄紀錄寫進磁碟
    #[cfg(unix)]
    std::fs::File::open(data_dir)?.sync_all()?;

    Ok(())
}

/// 路徑所在檔案系統，一般使用者可用的空間（位元組）。匯出與分段上傳共用。
///
/// u64::from：statvfs 的欄位在 64 位元平台是 u64、在 32 位元的 armv7 可能是 u32，兩者都能無損轉換。
/// clippy 只檢查目前的平台，在 64 位元上會認為轉換多餘；但在 32 位元上它是必要的
#[allow(clippy::useless_conversion)]
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path 是以 NUL 結尾的合法字串；stat 指向足夠大小的記憶體，成功時由 statvfs 填滿
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: rc == 0 代表 statvfs 已經填滿整個結構
    let stat = unsafe { stat.assume_init() };
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(stat.f_frsize)))
}

/// 刪除一個已完成的 blob。
///
/// 用於「blob 已 persist、但資料庫寫入失敗」時的清理。
/// key 一樣經過 `blob_path` 驗證，格式不符時回傳錯誤而不是去刪任意路徑。
pub async fn remove_blob(data_dir: &Path, key: &str) -> io::Result<()> {
    let path = blob_path(data_dir, key)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid storage key"))?;
    tokio::fs::remove_file(path).await
}

/// 完成檔案的根目錄：`{data_dir}/blobs`
pub fn blobs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(BLOBS_DIR)
}

/// 分段上傳暫存檔的目錄
pub fn uploads_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(UPLOADS_DIR)
}

/// 暫存檔目錄：`{data_dir}/tmp`
pub fn tmp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(TMP_DIR)
}

/// 是否為合法的分層目錄名稱：恰好 2 個小寫 hex 字元
pub fn is_shard_name(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 是否為 TempBlob 產生的暫存檔名稱：`<合法 key>.part`
pub fn is_temp_file_name(name: &str) -> bool {
    name.strip_suffix(".part").is_some_and(is_valid_key)
}

/// 產生新的 storage_key：128 位元隨機數，小寫 hex 編碼。
///
/// 不使用 base64：它區分大小寫，而 macOS、Windows 與 exFAT 外接硬碟
/// 預設不分大小寫，"Ab" 與 "aB" 會指向同一個檔案。
pub fn generate_storage_key() -> String {
    let mut buf = [0u8; KEY_LEN / 2];
    getrandom::fill(&mut buf).expect("failed to get secure random bytes from the OS");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 驗證 key 是否恰好為 32 個小寫 hex 字元
pub fn is_valid_key(key: &str) -> bool {
    key.len() == KEY_LEN && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 取得 blob 在磁碟上的路徑：`{data_dir}/blobs/{前兩字元}/{key}`。
///
/// key 雖由伺服器自行產生，仍必須驗證格式（深度防禦）：
/// 程式可能有 bug，資料庫裡的值也可能被竄改。格式不符時回傳 None。
///
/// 以前兩字元分層，讓檔案分散到 256 個子目錄，
/// 避免單一資料夾檔案過多導致效能下降或超出檔案系統上限。
pub fn blob_path(data_dir: &Path, key: &str) -> Option<PathBuf> {
    if !is_valid_key(key) {
        return None;
    }

    // 已驗證全為 ASCII，以位元組切片是安全的
    let shard = &key[..2];
    Some(data_dir.join(BLOBS_DIR).join(shard).join(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const VALID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn init_dirs_creates_tmp_and_blobs() {
        let dir = tempfile::tempdir().unwrap();

        init_dirs(dir.path()).unwrap();

        assert!(dir.path().join(TMP_DIR).is_dir());
        assert!(dir.path().join(BLOBS_DIR).is_dir());
    }

    #[test]
    fn init_dirs_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();

        init_dirs(dir.path()).unwrap();
        init_dirs(dir.path()).unwrap();

        assert!(dir.path().join(BLOBS_DIR).is_dir());
    }

    #[test]
    fn init_dirs_creates_missing_data_dir() {
        // data_dir 本身不存在時，create_dir_all 會一併建立
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("not").join("yet");

        init_dirs(&nested).unwrap();

        assert!(nested.join(TMP_DIR).is_dir());
    }

    #[tokio::test]
    async fn remove_blob_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = blob_path(dir.path(), VALID).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();

        remove_blob(dir.path(), VALID).await.unwrap();

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn remove_blob_rejects_invalid_key() {
        let dir = tempfile::tempdir().unwrap();

        let err = remove_blob(dir.path(), "../../etc/passwd")
            .await
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn generated_keys_are_valid() {
        for _ in 0..100 {
            let key = generate_storage_key();
            assert!(is_valid_key(&key), "{key}");
        }
    }

    #[test]
    fn generated_keys_are_unique() {
        let keys: HashSet<String> = (0..1000).map(|_| generate_storage_key()).collect();
        assert_eq!(keys.len(), 1000);
    }

    #[test]
    fn valid_key_produces_sharded_path() {
        let path = blob_path(Path::new("/srv/data"), VALID).unwrap();
        assert_eq!(
            path,
            Path::new("/srv/data/blobs/01/0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn attack_strings_are_rejected() {
        let data = Path::new("/srv/data");
        for key in [
            "",
            "../x",
            "../../../../etc/passwd",
            "/etc/passwd",
            "0123456789abcdef/123456789abcdef", // 含 /，長度剛好 32
            "0123456789abcdef\\123456789abcdef", // 含 \，長度剛好 32
            "..23456789abcdef0123456789abcdef", // 開頭是 ..，長度剛好 32
            "0123456789abcdef0123456789abcde",  // 31 字元
            "0123456789abcdef0123456789abcdef0", // 33 字元
            "0123456789ABCDEF0123456789ABCDEF", // 大寫
            "0123456789abcdeg0123456789abcdef", // 含非 hex 的 g
            "0123456789abcdef0123456789abcd\0f", // 含 NUL
            "é123456789abcdef0123456789abcde",  // 多位元組字元湊成 32 位元組
        ] {
            assert_eq!(blob_path(data, key), None, "{key:?}");
        }
    }

    /// Rust 的陷阱：join 一個絕對路徑時，會整個「取代」原路徑，而不是接在後面。
    /// 這就是為什麼不能把未驗證的輸入直接交給 join。
    #[test]
    #[cfg(unix)]
    fn join_with_absolute_path_replaces_base() {
        let joined = Path::new("/data").join("/etc/passwd");
        assert_eq!(joined, Path::new("/etc/passwd"));
    }

    /// 同樣地，join 不會解析 ..，結果仍然是一條會跳出去的路徑
    #[test]
    #[cfg(unix)]
    fn join_does_not_resolve_parent_components() {
        let joined = Path::new("/data").join("../etc/passwd");
        assert_eq!(joined, Path::new("/data/../etc/passwd"));
        // 字串上看起來在 /data 底下，實際上指向 /etc/passwd
        assert!(joined.starts_with("/data"));
    }

    #[test]
    fn shard_names() {
        assert!(is_shard_name("00"));
        assert!(is_shard_name("af"));
        for name in ["", "a", "abc", "AF", "g0", "..", "./"] {
            assert!(!is_shard_name(name), "{name:?}");
        }
    }

    #[test]
    fn temp_file_names() {
        assert!(is_temp_file_name(&format!("{VALID}.part")));
        for name in [
            VALID,                        // 沒有 .part
            "notes.part",                 // 不是合法 key
            &format!("{VALID}.part.bak"), // 多了尾巴
            &format!("{VALID}.PART"),     // 大寫副檔名
            ".part",
        ] {
            assert!(!is_temp_file_name(name), "{name:?}");
        }
    }
}
