//! 每顆硬碟（儲存位置）上 ManorCloud 自己用的資料夾，以及磁碟容量。
//!
//! ```text
//! /mnt/disk2/
//! ├── .manorcloud/
//! │   ├── volume        ← 標記檔：硬碟有掛載（資料目錄沒有）
//! │   ├── tmp/          ← 複製、一般上傳的暫存
//! │   ├── uploads/      ← 分段上傳的暫存檔 {id}.part
//! │   └── trash/        ← 垃圾桶 {id}
//! ├── users/
//! │   └── jerry/        ← 一般使用者的 My files
//! └── 相片/              ← 管理員的檔案
//! ```
//!
//! 暫存與垃圾桶都和檔案在同一個檔案系統：放到正式的名稱上、放進垃圾桶都只是改名。

pub mod archive;
pub mod resumable;

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::tree::SYSTEM_DIR;

/// 隨機編號的長度：128 位元以小寫 hex 編碼為 32 個字元
pub const KEY_LEN: usize = 32;

/// 單一檔案的預設大小上限：10 GiB。
/// 串流寫入時記憶體用量與檔案大小無關，這個上限保護的是磁碟空間。
pub const DEFAULT_MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// 一般使用者的資料夾都在這裡：`{硬碟}/users/{名稱}`
pub const USERS_DIR: &str = "users";

/// 儲存位置的標記檔。加入位置時建立，之後每次使用前確認它還在。
///
/// 外接硬碟沒有掛載時，掛載點只是系統碟上一個空的資料夾，寫得進去：
/// 沒有這個檢查的話，檔案會默默寫到系統碟上，硬碟接回來之後就「消失」了
const VOLUME_MARKER: &str = "volume";

/// 舊版的標記檔，放在儲存位置的最上層。轉換時換成新的
pub const LEGACY_VOLUME_MARKER: &str = ".manorcloud-storage";

/// `{硬碟}/.manorcloud`
pub fn system_dir(root: &Path) -> PathBuf {
    root.join(SYSTEM_DIR)
}

/// 複製、一般上傳的暫存：`{硬碟}/.manorcloud/tmp`
pub fn tmp_dir(root: &Path) -> PathBuf {
    system_dir(root).join("tmp")
}

/// 分段上傳的暫存檔：`{硬碟}/.manorcloud/uploads`。與 tmp/ 分開：
/// 垃圾回收會刪除 tmp/ 中超過寬限期的檔案，而分段上傳的暫存檔只要還有進度，就必須保留
pub fn uploads_dir(root: &Path) -> PathBuf {
    system_dir(root).join("uploads")
}

/// 垃圾桶：`{硬碟}/.manorcloud/trash`
pub fn trash_dir(root: &Path) -> PathBuf {
    system_dir(root).join("trash")
}

/// 垃圾桶中的一個項目：`{硬碟}/.manorcloud/trash/{id}`
pub fn trash_path(root: &Path, id: i64) -> PathBuf {
    trash_dir(root).join(id.to_string())
}

/// 建立 `.manorcloud/` 底下的資料夾。已存在的不受影響，可以重複呼叫
pub fn init_dirs(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(tmp_dir(root))?;
    std::fs::create_dir_all(uploads_dir(root))?;
    std::fs::create_dir_all(trash_dir(root))?;
    Ok(())
}

/// 建立儲存位置的標記檔（見 `VOLUME_MARKER`）
pub fn mark_volume(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(system_dir(root))?;
    let marker = system_dir(root).join(VOLUME_MARKER);
    std::fs::write(
        &marker,
        b"This disk is a ManorCloud storage location. Don't delete this folder.\n",
    )?;
    std::fs::File::open(&marker)?.sync_all()
}

/// 儲存位置是否可以使用：標記檔還在（硬碟有掛載）。
/// 只有舊版的標記檔時（升級之後第一次使用、轉換時沒有掛載的硬碟），換成新的標記檔
pub fn volume_ready(root: &Path) -> bool {
    if system_dir(root).join(VOLUME_MARKER).is_file() {
        return true;
    }
    let legacy = root.join(LEGACY_VOLUME_MARKER);
    if legacy.is_file() && mark_volume(root).is_ok() {
        let _ = std::fs::remove_file(legacy);
        return true;
    }
    false
}

/// 使用者資料夾的名稱（來自帳號名稱：只有英文、數字、底線、連字號與點）。
/// 它組成的是硬碟上的路徑，這裡再檢查一次：不合法的名稱（空的、點開頭、含 `/`）回傳 false
pub fn is_valid_folder_name(folder: &str) -> bool {
    !folder.is_empty()
        && folder.len() <= 64
        && !folder.starts_with('.')
        && folder
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// 使用者的 My files：`{硬碟}/users/{folder}`；名稱不合法時回傳 None
pub fn user_dir(root: &Path, folder: &str) -> Option<PathBuf> {
    is_valid_folder_name(folder).then(|| root.join(USERS_DIR).join(folder))
}

/// 產生隨機編號：128 位元隨機數，小寫 hex 編碼。
///
/// 不使用 base64：它區分大小寫，而 macOS、Windows 與 exFAT 外接硬碟
/// 預設不分大小寫，"Ab" 與 "aB" 會指向同一個檔案。
pub fn generate_storage_key() -> String {
    let mut buf = [0u8; KEY_LEN / 2];
    getrandom::fill(&mut buf).expect("failed to get secure random bytes from the OS");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 驗證是否恰好為 32 個小寫 hex 字元
pub fn is_valid_key(key: &str) -> bool {
    key.len() == KEY_LEN && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 路徑所在檔案系統，一般使用者可用的空間（位元組）
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    disk_space(path).map(|space| space.available)
}

/// 檔案系統的容量（位元組）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub total: u64,
    /// 一般使用者可以使用的空間（不含保留給 root 的部分）
    pub available: u64,
}

/// 路徑所在檔案系統的總容量與可用空間
// statvfs 欄位的型別依平台而不同（u32 或 u64），一律轉成 u64；在 u64 的平台上 clippy 會說轉換多餘
#[allow(clippy::useless_conversion, unsafe_code)]
pub fn disk_space(path: &Path) -> io::Result<DiskSpace> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path 是以 NUL 結尾的有效路徑；stat 指向足夠大小的記憶體，成功時由 statvfs 填滿
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: rc == 0 代表 statvfs 已經完整寫入 stat
    let stat = unsafe { stat.assume_init() };
    let fragment = u64::from(stat.f_frsize);
    Ok(DiskSpace {
        total: u64::from(stat.f_blocks).saturating_mul(fragment),
        available: u64::from(stat.f_bavail).saturating_mul(fragment),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_lowercase_hex() {
        let key = generate_storage_key();
        assert!(is_valid_key(&key));
        assert_ne!(key, generate_storage_key());
        assert!(!is_valid_key("../../etc/passwd"));
        assert!(!is_valid_key(&key.to_uppercase()));
    }

    #[test]
    fn user_folders_stay_inside_users() {
        let root = Path::new("/mnt/disk2");
        assert_eq!(
            user_dir(root, "jerry"),
            Some(PathBuf::from("/mnt/disk2/users/jerry"))
        );
        for bad in ["", ".", "..", "a/b", ".hidden", "a b"] {
            assert_eq!(user_dir(root, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_marker_shows_the_disk_is_mounted() {
        let root = tempfile::tempdir().unwrap();
        assert!(!volume_ready(root.path()));
        mark_volume(root.path()).unwrap();
        assert!(volume_ready(root.path()));
    }
}
