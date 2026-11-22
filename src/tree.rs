//! 硬碟上真實的檔案與資料夾：My files 與各顆硬碟共用。
//!
//! 安全重點：網址中的路徑完全不可信，必須保證永遠留在空間的根目錄裡面。
//! - 路徑一段一段解析，拒絕 `.`、`..`、空的段落、NUL 與 `\`
//! - 不跟隨符號連結：每一段都用 lstat 檢查，最後開檔時再加上 O_NOFOLLOW。
//!   否則一個指向 `/etc` 的連結，就能讀到整台伺服器
//! - 列表中不顯示符號連結與特殊檔案（裝置、FIFO、socket），也不能下載它們
//! - `.manorcloud` 開頭的名稱是 ManorCloud 自己用的（暫存檔、垃圾桶），永遠看不到、也不能使用
//!
//! 名稱不分大小寫：`a.jpg` 已經存在時，不能再建立 `A.jpg`。
//! 檔案下載到 macOS、Windows（預設不分大小寫）時兩者會互相覆蓋，對使用者而言它們也是同一個名稱。
//!
//! 新的內容（上傳、複製、還原）一律先放在暫存位置，完成後才出現在正式的名稱上：
//! 中途中斷不會留下只有一半的檔案，也永遠不會覆蓋別人剛放上去的同名項目。

use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::files::name::{numbered_name, validate_file_name};
pub use crate::relpath::RelPath;

/// ManorCloud 自己的資料夾：每顆硬碟的最上層各有一個
pub const SYSTEM_DIR: &str = ".manorcloud";

/// 改名時最多嘗試幾個「名稱 (n)」
const MAX_NUMBERED: u32 = 999;

/// `.manorcloud` 開頭的名稱：ManorCloud 自己用的，列表中不顯示，也不能用來命名
pub fn is_system_name(name: &str) -> bool {
    name.starts_with(SYSTEM_DIR)
}

/// 以點開頭的隱藏檔（.DS_Store、._相片.jpg）
pub fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

/// 一個項目
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改時間（Unix 秒）
    pub modified: i64,
}

fn not_found(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, what.to_string())
}

fn name_taken() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "An item with that name already exists",
    )
}

/// 把相對路徑解析成真實的路徑，每一段都不能是符號連結，也不能是 ManorCloud 自己的資料夾。
///
/// `must_exist` 為 false 時，最後一段可以不存在（建立新項目），但上層都必須存在而且是資料夾
pub fn resolve(root: &Path, rel: &RelPath, must_exist: bool) -> io::Result<PathBuf> {
    let mut path = root.to_path_buf();
    let last = rel.parts().len();
    for (index, part) in rel.parts().iter().enumerate() {
        if is_system_name(part) {
            return Err(not_found("reserved name"));
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(not_found("symbolic link")),
            Ok(meta) if index + 1 < last && !meta.is_dir() => {
                return Err(not_found("not a folder"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound && index + 1 == last && !must_exist => {}
            Err(e) => return Err(e),
        }
    }
    Ok(path)
}

/// 必須是存在的資料夾
pub fn resolve_dir(root: &Path, rel: &RelPath) -> io::Result<PathBuf> {
    let path = resolve(root, rel, true)?;
    if !std::fs::symlink_metadata(&path)?.is_dir() {
        return Err(not_found("not a folder"));
    }
    Ok(path)
}

/// 資料夾的內容，排序交給前端。
/// 略過符號連結、特殊檔案、名稱不是合法 UTF-8 的項目，以及 ManorCloud 自己的資料夾
pub fn list(root: &Path, rel: &RelPath) -> io::Result<Vec<Entry>> {
    let dir = resolve_dir(root, rel)?;
    let mut entries = Vec::new();
    for item in std::fs::read_dir(&dir)? {
        let item = item?;
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if is_system_name(&name) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
            continue;
        };
        let kind = meta.file_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        entries.push(Entry {
            name,
            is_dir: kind.is_dir(),
            size: if kind.is_dir() { 0 } else { meta.len() },
            modified: modified(&meta),
        });
    }
    Ok(entries)
}

/// 修改時間（Unix 秒）；讀不到時是 0
pub fn modified(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// 開啟一個檔案來讀取：不跟隨符號連結，而且必須是一般檔案
pub fn open_file(root: &Path, rel: &RelPath) -> io::Result<(std::fs::File, std::fs::Metadata)> {
    if rel.is_root() {
        return Err(not_found("not a file"));
    }
    let path = resolve(root, rel, true)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(not_found("not a file"));
    }
    Ok((file, meta))
}

/// 下載用的 ETag：路徑、大小、修改時間與 inode 組成。內容改變時這幾個值幾乎一定會變，
/// 斷點續傳不會把新舊兩個版本拼在一起
pub fn etag(path: &Path, meta: &std::fs::Metadata) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "manorcloud:{}:{}:{}:{}:{}",
            path.display(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ino()
        )
        .as_bytes(),
    );
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// 打包下載時走訪到的一個項目
#[derive(Debug, Clone, PartialEq)]
pub struct WalkItem {
    /// 以被選取的項目開頭，例如 `相片/2026/a.jpg`
    pub path: String,
    pub source: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: i64,
}

/// 一個項目（資料夾或檔案）以及資料夾底下的所有內容。
/// 略過符號連結、特殊檔案、ManorCloud 自己的資料夾，以及 `include` 回傳 false 的名稱
pub fn walk(
    root: &Path,
    rel: &RelPath,
    include: &dyn Fn(&str) -> bool,
) -> io::Result<Vec<WalkItem>> {
    let path = resolve(root, rel, true)?;
    let meta = std::fs::symlink_metadata(&path)?;
    let name = rel.name().unwrap_or("files").to_string();
    let mut out = Vec::new();
    walk_into(&path, &meta, name, include, &mut out)?;
    Ok(out)
}

fn walk_into(
    path: &Path,
    meta: &std::fs::Metadata,
    zip_path: String,
    include: &dyn Fn(&str) -> bool,
    out: &mut Vec<WalkItem>,
) -> io::Result<()> {
    let kind = meta.file_type();
    if !(kind.is_file() || kind.is_dir()) {
        return Ok(());
    }
    out.push(WalkItem {
        path: zip_path.clone(),
        source: path.to_path_buf(),
        is_dir: kind.is_dir(),
        size: if kind.is_dir() { 0 } else { meta.len() },
        modified: modified(meta),
    });
    if kind.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(path)?.collect::<Result<_, _>>()?;
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let Ok(name) = child.file_name().into_string() else {
                continue;
            };
            if is_system_name(&name) || !include(&name) {
                continue;
            }
            let child_meta = std::fs::symlink_metadata(child.path())?;
            walk_into(
                &child.path(),
                &child_meta,
                format!("{zip_path}/{name}"),
                include,
                out,
            )?;
        }
    }
    Ok(())
}

/// 檔案的大小，或資料夾底下所有一般檔案的大小總和。不跟隨符號連結
pub fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|items| {
            items
                .filter_map(Result::ok)
                .map(|item| tree_size(&item.path()))
                .sum()
        })
        .unwrap_or(0)
}

// ==================== 搜尋 ====================

/// 搜尋到的一個項目
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// 從搜尋的資料夾算起的路徑
    pub path: RelPath,
    pub entry: Entry,
}

/// 把搜尋的文字拆成關鍵字：以空白分隔，統一成 NFC 與小寫
pub fn search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(fold)
        .filter(|term| !term.is_empty())
        .collect()
}

/// 比對用的名稱：NFC（macOS 傳來的檔名可能是 NFD）與小寫
fn fold(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    text.nfc().collect::<String>().to_lowercase()
}

/// 在資料夾與它所有的子資料夾中，找名稱包含每一個關鍵字的檔案與資料夾。
///
/// 由淺到深走訪（上層的結果排在前面），找到 `limit` 個就停止，第二個回傳值表示還有更多。
/// 略過符號連結、特殊檔案、ManorCloud 自己的資料夾，以及 `include(所在資料夾, 名稱)` 回傳
/// false 的項目（資料夾被略過時，裡面的內容也不會搜尋）
pub fn search(
    root: &Path,
    rel: &RelPath,
    terms: &[String],
    include: &dyn Fn(&RelPath, &str) -> bool,
    limit: usize,
) -> io::Result<(Vec<Found>, bool)> {
    let start = resolve_dir(root, rel)?;
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([(start, RelPath::default())]);
    while let Some((dir, dir_rel)) = queue.pop_front() {
        let Ok(items) = std::fs::read_dir(&dir) else {
            // 讀不到的子資料夾（權限）跳過，不讓整個搜尋失敗
            continue;
        };
        let mut items: Vec<_> = items.filter_map(Result::ok).collect();
        items.sort_by_key(|item| item.file_name());
        for item in items {
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            if is_system_name(&name) || !include(&dir_rel, &name) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
                continue;
            };
            let kind = meta.file_type();
            if !(kind.is_file() || kind.is_dir()) {
                continue;
            }
            let path = dir_rel.child(&name);
            let folded = fold(&name);
            if terms.iter().all(|term| folded.contains(term.as_str())) {
                if found.len() == limit {
                    return Ok((found, true));
                }
                found.push(Found {
                    path: path.clone(),
                    entry: Entry {
                        name,
                        is_dir: kind.is_dir(),
                        size: if kind.is_dir() { 0 } else { meta.len() },
                        modified: modified(&meta),
                    },
                });
            }
            if kind.is_dir() {
                queue.push_back((item.path(), path));
            }
        }
    }
    Ok((found, false))
}

// ==================== 名稱 ====================

/// 新的名稱：一般檔名的規則，另外不能用 ManorCloud 保留的名稱
pub fn check_new_name(name: &str) -> Result<String, String> {
    let name = validate_file_name(name).map_err(|e| e.to_string())?;
    if is_system_name(&name) {
        return Err("That name is reserved".to_string());
    }
    Ok(name)
}

/// 資料夾中有沒有和 `name` 只差大小寫（或完全相同）的項目；有的話回傳它真正的名稱。
/// `except` 是自己（只改大小寫時，自己不算衝突）
pub fn find_same_name(dir: &Path, name: &str, except: Option<&Path>) -> io::Result<Option<String>> {
    let wanted = name.to_lowercase();
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        let Ok(existing) = item.file_name().into_string() else {
            continue;
        };
        if existing.to_lowercase() != wanted {
            continue;
        }
        if except.is_some_and(|except| same_file(except, &item.path())) {
            continue;
        }
        return Ok(Some(existing));
    }
    Ok(None)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// 同名時怎麼處理。網址參數與 JSON 都寫成 rename、fail、replace。
/// 預設自動改名：使用者上傳同名檔案時，最自然的期待是「兩個都留著」
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    #[default]
    Rename,
    Fail,
    Replace,
}

/// 把暫存的檔案或資料夾放到 `dir` 中的 `name`，回傳實際使用的名稱。
///
/// - Rename：同名時改用「名稱 (1)」、「名稱 (2)」⋯⋯
/// - Fail：同名時回傳 AlreadyExists
/// - Replace：同名的檔案被取代（同名的是資料夾，或放的是資料夾時，回傳 AlreadyExists）
///
/// 放到新名稱時用 hard link（資料夾用改名）：名稱已經存在時會失敗，
/// 不會有「檢查之後、改名之前」被別人搶先而覆蓋的問題
pub fn place(temp: &Path, dir: &Path, name: &str, on_conflict: OnConflict) -> io::Result<String> {
    let is_file = std::fs::symlink_metadata(temp)?.is_file();
    for n in 0..=MAX_NUMBERED {
        let candidate = if n == 0 {
            name.to_string()
        } else {
            numbered_name(name, n)
        };
        if validate_file_name(&candidate).is_err() {
            break;
        }
        if let Some(existing) = find_same_name(dir, &candidate, None)? {
            match on_conflict {
                OnConflict::Rename => continue,
                OnConflict::Fail => return Err(name_taken()),
                OnConflict::Replace => {
                    let target = dir.join(&existing);
                    let meta = std::fs::symlink_metadata(&target)?;
                    if !is_file || !meta.is_file() {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "A folder has that name",
                        ));
                    }
                    std::fs::rename(temp, &target)?;
                    return Ok(existing);
                }
            }
        }
        match place_new(temp, &dir.join(&candidate), is_file) {
            Ok(()) => return Ok(candidate),
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && on_conflict == OnConflict::Rename =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "Too many items with that name",
    ))
}

/// 把暫存的項目放到一個還不存在的名稱上
fn place_new(temp: &Path, target: &Path, is_file: bool) -> io::Result<()> {
    if is_file {
        match std::fs::hard_link(temp, target) {
            Ok(()) => {
                // 正式的名稱已經指向內容；暫存檔的名稱刪不掉也只是多一個看不到的檔案
                let _ = std::fs::remove_file(temp);
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
            // 不支援 hard link（部分網路檔案系統）：改用下面的檢查之後再改名
            Err(_) => {}
        }
    }
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(name_taken());
    }
    std::fs::rename(temp, target)
}

// ==================== 修改 ====================

/// 建立資料夾。同名（不分大小寫）時回傳 AlreadyExists
pub fn create_dir(root: &Path, parent: &RelPath, name: &str) -> io::Result<RelPath> {
    let dir = resolve_dir(root, parent)?;
    if find_same_name(&dir, name, None)?.is_some() {
        return Err(name_taken());
    }
    std::fs::create_dir(dir.join(name))?;
    Ok(parent.child(name))
}

/// 建立資料夾，連同還不存在的上層（還原到已經被刪除的資料夾時）。
/// 已經存在的每一層都不能是符號連結或檔案
pub fn create_dirs(root: &Path, rel: &RelPath) -> io::Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rel.parts() {
        if is_system_name(part) {
            return Err(not_found("reserved name"));
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(not_found("not a folder")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&path)?,
            Err(e) => return Err(e),
        }
    }
    Ok(path)
}

/// 刪除檔案，或資料夾連同底下所有內容。底下的符號連結只刪除連結本身
pub fn remove_path(path: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// 改名，或搬到同一個空間中的另一個資料夾。同名（不分大小寫）時回傳 AlreadyExists，不會覆蓋。
/// 不能搬進自己底下
pub fn rename(root: &Path, from: &RelPath, to_parent: &RelPath, name: &str) -> io::Result<RelPath> {
    if from.is_root() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "This folder can't be renamed",
        ));
    }
    let to = to_parent.child(name);
    if to.starts_with(from) && to != *from {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A folder can't be moved into itself",
        ));
    }
    let source = resolve(root, from, true)?;
    let dir = resolve_dir(root, to_parent)?;
    if to == *from {
        return Ok(to);
    }
    if find_same_name(&dir, name, Some(&source))?.is_some() {
        return Err(name_taken());
    }
    std::fs::rename(&source, dir.join(name))?;
    Ok(to)
}

/// 複製一個檔案或資料夾到 `to_dir`，同名時自動改名。
/// 先完整複製到 `tmp`（必須和目的地在同一個檔案系統），最後才放到正式的名稱上。
/// 略過符號連結、特殊檔案與 ManorCloud 自己的資料夾。回傳實際使用的名稱
pub fn copy(source: &Path, tmp: &Path, to_dir: &Path, name: &str) -> io::Result<String> {
    std::fs::create_dir_all(tmp)?;
    let staging = tmp.join(format!("copy-{}", crate::storage::generate_storage_key()));
    let result = copy_into(source, &staging)
        .and_then(|()| place(&staging, to_dir, name, OnConflict::Rename));
    if result.is_err() && std::fs::symlink_metadata(&staging).is_ok() {
        let _ = remove_path(&staging);
    }
    result
}

/// 複製到一個還不存在的路徑，保留修改時間
pub fn copy_into(source: &Path, target: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_file() {
        let mut from = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        if let Ok(time) = meta.modified() {
            let _ = to.set_modified(time);
        }
    } else if meta.is_dir() {
        std::fs::create_dir(target)?;
        for item in std::fs::read_dir(source)? {
            let item = item?;
            let Ok(name) = item.file_name().into_string() else {
                continue;
            };
            if is_system_name(&name) {
                continue;
            }
            let kind = item.file_type()?;
            if kind.is_file() || kind.is_dir() {
                copy_into(&item.path(), &target.join(&name))?;
            }
        }
        if let Ok(time) = meta.modified() {
            set_modified(target, time);
        }
    }
    Ok(())
}

/// 長時間複製（換儲存位置、跨硬碟傳輸）的進度，由背景執行緒更新、另一邊讀取
#[derive(Debug, Default)]
pub struct Progress {
    /// 已經複製的位元組數
    pub copied: AtomicU64,
    /// 設定之後，複製在下一個區塊停下來（回傳錯誤）
    pub stop: AtomicBool,
    /// 略過的項目（符號連結、特殊檔案、名稱不是 UTF-8 的項目），以複製的起點為準的路徑
    pub skipped: Mutex<Vec<String>>,
}

impl Progress {
    fn skip(&self, path: String) {
        if let Ok(mut skipped) = self.skipped.lock() {
            skipped.push(path);
        }
    }
}

/// 複製一個檔案或資料夾到還不存在的 `to`，一邊累計位元組數；`stop` 被設定時停下來。
/// 略過符號連結、特殊檔案與 ManorCloud 自己的資料夾（前兩者記錄在 `skipped`）
pub fn copy_tracked(from: &Path, to: &Path, progress: &Progress) -> io::Result<()> {
    let name = from
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    copy_tracked_into(from, to, &name, progress)
}

fn copy_tracked_into(from: &Path, to: &Path, shown: &str, progress: &Progress) -> io::Result<()> {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    let cancelled = || io::Error::new(io::ErrorKind::Interrupted, "cancelled");
    if progress.stop.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_file() {
        let mut input = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(from)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        let mut buffer = vec![0u8; 256 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            progress.copied.fetch_add(read as u64, Ordering::Relaxed);
            if progress.stop.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
        }
        output.sync_all()?;
        if let Ok(time) = meta.modified() {
            let _ = output.set_modified(time);
        }
    } else if meta.is_dir() {
        std::fs::create_dir(to)?;
        for item in std::fs::read_dir(from)? {
            let item = item?;
            let Ok(name) = item.file_name().into_string() else {
                progress.skip(format!("{shown}/{}", item.file_name().to_string_lossy()));
                continue;
            };
            if is_system_name(&name) {
                continue;
            }
            let child = format!("{shown}/{name}");
            let kind = item.file_type()?;
            if kind.is_file() || kind.is_dir() {
                copy_tracked_into(&item.path(), &to.join(&name), &child, progress)?;
            } else {
                progress.skip(child);
            }
        }
        if let Ok(time) = meta.modified() {
            set_modified(to, time);
        }
    } else {
        progress.skip(shown.to_string());
    }
    Ok(())
}

/// 設定修改時間（資料夾也可以）。失敗不影響內容，忽略
pub fn set_modified(path: &Path, time: SystemTime) {
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.set_times(std::fs::FileTimes::new().set_modified(time));
    }
}

/// Unix 秒 → SystemTime
pub fn unix_time(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
}

// ==================== 一般上傳 ====================

/// 一般上傳（一次送完）的暫存檔：放在 `tmp` 中，完成後用 `finish` 放到正式的名稱上。
/// 被丟棄時（失敗、取消）刪除暫存檔
pub struct Upload {
    temp: PathBuf,
    file: Option<tokio::fs::File>,
    size: u64,
    limit: u64,
}

impl Upload {
    pub async fn create(tmp: &Path, limit: u64) -> io::Result<Upload> {
        tokio::fs::create_dir_all(tmp).await?;
        let temp = tmp.join(format!("upload-{}", crate::storage::generate_storage_key()));
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        Ok(Upload {
            temp,
            file: Some(file),
            size: 0,
            limit,
        })
    }

    pub async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.size = self.size.saturating_add(chunk.len() as u64);
        if self.size > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "The file is larger than the upload limit",
            ));
        }
        match &mut self.file {
            Some(file) => file.write_all(chunk).await,
            None => Err(io::Error::other("the upload is already finished")),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// 寫入磁碟並放到 `dir` 中的 `name`，回傳實際使用的名稱
    pub async fn finish(
        mut self,
        dir: PathBuf,
        name: String,
        on_conflict: OnConflict,
    ) -> io::Result<String> {
        if let Some(file) = self.file.take() {
            file.sync_all().await?;
        }
        let temp = self.temp.clone();
        tokio::task::spawn_blocking(move || place(&temp, &dir, &name, on_conflict))
            .await
            .map_err(io::Error::other)?
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // 已經放到正式名稱的暫存檔不存在了，刪除失敗是正常的
        let _ = std::fs::remove_file(&self.temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relpath::InvalidPath;

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).unwrap()
    }

    fn names(root: &Path, dir: &str) -> Vec<String> {
        let mut names: Vec<_> = list(root, &rel(dir))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn paths_cannot_leave_the_space() {
        for bad in ["..", "a/../b", "./a", "a//b", "a\\b", "a\0b"] {
            assert_eq!(RelPath::parse(bad), Err(InvalidPath), "{bad:?}");
        }
        assert!(rel("").is_root());
        assert!(rel("/").is_root());
        assert_eq!(rel("/相片/2026/").display(), "相片/2026");
        assert!(rel("users/a/b").starts_with(&rel("users")));
        assert!(!rel("usersx").starts_with(&rel("users")));
    }

    #[test]
    fn symlinks_and_the_system_folder_are_never_reachable() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.path().join("file"))
            .unwrap();
        std::fs::create_dir(root.path().join(SYSTEM_DIR)).unwrap();
        std::fs::write(root.path().join(SYSTEM_DIR).join("x"), b"x").unwrap();

        assert!(open_file(root.path(), &rel("link/secret")).is_err());
        assert!(open_file(root.path(), &rel("file")).is_err());
        assert!(open_file(root.path(), &rel(".manorcloud/x")).is_err());
        assert!(list(root.path(), &rel("link")).is_err());
        // 列表中看不到連結與 .manorcloud
        assert!(list(root.path(), &rel("")).unwrap().is_empty());
    }

    #[test]
    fn names_are_case_insensitive() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(root.path().join("Docs")).unwrap();

        let err = create_dir(root.path(), &rel(""), "docs").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let err = rename(root.path(), &rel("a.txt"), &rel(""), "DOCS").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // 只改自己的大小寫可以
        rename(root.path(), &rel("a.txt"), &rel(""), "A.txt").unwrap();
        assert_eq!(names(root.path(), ""), ["A.txt", "Docs"]);
    }

    #[test]
    fn rename_never_overwrites_and_cannot_move_into_itself() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"a").unwrap();
        std::fs::write(root.path().join("b.txt"), b"b").unwrap();
        std::fs::create_dir(root.path().join("docs")).unwrap();

        let err = rename(root.path(), &rel("a.txt"), &rel(""), "b.txt").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(rename(root.path(), &rel("docs"), &rel("docs"), "x").is_err());

        let moved = rename(root.path(), &rel("a.txt"), &rel("docs"), "c.txt").unwrap();
        assert_eq!(moved.display(), "docs/c.txt");
        assert_eq!(std::fs::read(root.path().join("docs/c.txt")).unwrap(), b"a");
    }

    #[test]
    fn place_follows_the_conflict_choice() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::write(dir.join("a.txt"), b"old").unwrap();
        std::fs::create_dir(dir.join("Docs")).unwrap();
        let temp = |content: &[u8]| {
            let path = dir.join(format!(".manorcloud-t{}", content.len()));
            std::fs::write(&path, content).unwrap();
            path
        };

        assert_eq!(
            place(&temp(b"1"), dir, "A.TXT", OnConflict::Rename).unwrap(),
            "A (1).TXT"
        );
        let err = place(&temp(b"22"), dir, "a.txt", OnConflict::Fail).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            place(&temp(b"333"), dir, "A.txt", OnConflict::Replace).unwrap(),
            "a.txt"
        );
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"333");
        let err = place(&temp(b"4444"), dir, "docs", OnConflict::Replace).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn copy_keeps_the_tree_and_renames_on_conflict() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        std::fs::create_dir_all(dir.join("docs/sub")).unwrap();
        std::fs::write(dir.join("docs/sub/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("docs/link")).unwrap();

        let name = copy(&dir.join("docs"), &dir.join(".manorcloud/tmp"), dir, "docs").unwrap();

        assert_eq!(name, "docs (1)");
        assert_eq!(std::fs::read(dir.join("docs (1)/sub/a.txt")).unwrap(), b"a");
        assert!(std::fs::symlink_metadata(dir.join("docs (1)/link")).is_err());
        assert_eq!(
            std::fs::read_dir(dir.join(".manorcloud/tmp"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(tree_size(&dir.join("docs (1)")), 1);
    }

    #[tokio::test]
    async fn uploads_appear_only_when_finished() {
        let root = tempfile::tempdir().unwrap();
        let tmp = root.path().join(".manorcloud/tmp");
        std::fs::write(root.path().join("a.txt"), b"old").unwrap();

        let mut upload = Upload::create(&tmp, 100).await.unwrap();
        upload.write_chunk(b"new").await.unwrap();
        assert_eq!(names(root.path(), ""), ["a.txt"]);
        let name = upload
            .finish(
                root.path().to_path_buf(),
                "a.txt".into(),
                OnConflict::Rename,
            )
            .await
            .unwrap();

        assert_eq!(name, "a (1).txt");
        assert_eq!(
            std::fs::read(root.path().join("a (1).txt")).unwrap(),
            b"new"
        );
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_upload_leaves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let tmp = root.path().join("tmp");
        let mut upload = Upload::create(&tmp, 4).await.unwrap();
        upload.write_chunk(b"abc").await.unwrap();
        assert!(upload.write_chunk(b"de").await.is_err());

        drop(upload);

        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    }
    #[test]
    fn search_finds_names_in_subfolders_shallow_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("相片/2026/旅行")).unwrap();
        std::fs::write(root.join("Invoice 2026.pdf"), b"x").unwrap();
        std::fs::write(root.join("相片/2026/旅行/invoice-2026-03.pdf"), b"x").unwrap();
        std::fs::write(root.join("相片/2026/other.pdf"), b"x").unwrap();
        std::fs::write(root.join(".invoice-2026"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc", root.join("invoice-link-2026")).unwrap();
        // macOS 的 NFD 檔名
        std::fs::write(root.join("cafe\u{301} 2026"), b"x").unwrap();

        let visible = |_: &RelPath, name: &str| !is_hidden_name(name);
        let names =
            |found: Vec<Found>| -> Vec<String> { found.iter().map(|f| f.path.display()).collect() };

        let (found, more) = search(
            root,
            &RelPath::default(),
            &search_terms("INVOICE  2026"),
            &visible,
            10,
        )
        .unwrap();
        assert!(!more);
        assert_eq!(
            names(found),
            ["Invoice 2026.pdf", "相片/2026/旅行/invoice-2026-03.pdf"]
        );

        let (found, _) = search(
            root,
            &RelPath::default(),
            &search_terms("café"),
            &visible,
            10,
        )
        .unwrap();
        assert_eq!(found.len(), 1);

        // 只在子資料夾中搜尋；路徑從那個資料夾算起
        let photos = RelPath::parse("相片").unwrap();
        let (found, _) = search(root, &photos, &search_terms("2026"), &visible, 10).unwrap();
        assert_eq!(names(found), ["2026", "2026/旅行/invoice-2026-03.pdf"]);

        let (found, more) =
            search(root, &RelPath::default(), &search_terms("pdf"), &visible, 2).unwrap();
        assert_eq!((found.len(), more), (2, true));
    }
}
