//! 每個使用者在儲存位置中的資料夾。
//!
//! ```text
//! /mnt/disk2/manorcloud/          ← 儲存位置（或資料目錄）
//! ├── .manorcloud-storage         ← 標記檔（資料目錄沒有）
//! ├── alice/                      ← alice 的資料夾
//! │   ├── blobs/  tmp/  uploads/
//! └── bob/
//!     ├── blobs/  tmp/  uploads/
//! ```
//!
//! - `user_root`：使用者的資料夾在哪裡
//! - `sync_name`：帳號改名之後，讓資料夾跟著改名
//! - `relayout`：舊版把所有人的檔案放在同一個 blobs/ 中，啟動時搬進各自的資料夾

use std::io;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::db::storage::{self as storage_db, UserLocation};
use crate::storage;

/// 舊版的共用目錄在整理期間暫放的位置。
/// 先把它們整個改名到這裡，才不會和名為 blobs、tmp、uploads 的使用者的資料夾混在一起
const LEGACY_DIR: &str = ".manorcloud-legacy";

/// 位置的目錄：None 是資料目錄
pub async fn volume_root(
    pool: &SqlitePool,
    data_dir: &Path,
    volume: Option<i64>,
) -> Result<PathBuf, String> {
    match volume {
        None => Ok(data_dir.to_path_buf()),
        Some(id) => match storage_db::find_volume(pool, id).await {
            Ok(Some(v)) => Ok(PathBuf::from(v.path)),
            Ok(None) => Err(format!("storage location {id} no longer exists")),
            Err(e) => Err(format!("database error: {e}")),
        },
    }
}

/// 使用者的資料夾。資料夾名稱不合法（資料庫被竄改）時回傳錯誤，而不是組出一個奇怪的路徑
pub fn user_root(data_dir: &Path, here: &UserLocation) -> Result<PathBuf, String> {
    let volume = here
        .path
        .as_deref()
        .map_or_else(|| data_dir.to_path_buf(), PathBuf::from);
    storage::user_root(&volume, &here.folder)
        .ok_or_else(|| format!("invalid storage folder name {:?}", here.folder))
}

/// 使用者所有的資料夾：目前的位置，以及正在搬過去的位置。使用者不存在時是空的
pub async fn folders_of(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
) -> Result<Vec<PathBuf>, String> {
    let all = storage_db::user_folders(pool)
        .await
        .map_err(|e| format!("database error: {e}"))?;
    let Some(user) = all.into_iter().find(|u| u.user_id == user_id) else {
        return Ok(Vec::new());
    };
    let mut volumes = vec![user.volume_id];
    if user.moving {
        volumes.push(user.target_volume_id);
    }
    let mut result = Vec::new();
    for volume in volumes {
        let root = volume_root(pool, data_dir, volume).await?;
        if let Some(folder) = storage::user_root(&root, &user.folder) {
            result.push(folder);
        }
    }
    Ok(result)
}

/// 建立使用者的資料夾（`{位置}/{資料夾名稱}/blobs` 等）。失敗只記錄下來：第一次寫入時會再建立
pub async fn create_user_folder(pool: &SqlitePool, data_dir: &Path, user_id: i64) {
    let result = async {
        let here = storage_db::user_location(pool, user_id)
            .await
            .map_err(|e| format!("database error: {e}"))?
            .ok_or_else(|| "the user no longer exists".to_string())?;
        if let Some(path) = &here.path
            && !storage::volume_ready(Path::new(path))
        {
            return Err(format!("{path} isn't available"));
        }
        init_dirs(&user_root(data_dir, &here)?).await
    }
    .await;
    if let Err(e) = result {
        eprintln!("storage: cannot create the folder of user {user_id}: {e}");
    }
}

/// 刪除使用者的資料夾（使用者已經從資料庫刪除之後）。
///
/// 只刪除 ManorCloud 自己建立的 blobs/、tmp/、uploads/，再移除變空的資料夾：
/// 萬一有人在裡面放了別的東西，那些東西會留著。失敗的只記錄下來
pub async fn remove_user_folder(folder: &Path) {
    for name in ["blobs", "tmp", "uploads"] {
        let dir = folder.join(name);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("storage: cannot remove {}: {e}", dir.display()),
        }
    }
    match tokio::fs::remove_dir(folder).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("storage: cannot remove {}: {e}", folder.display()),
    }
}

// ==================== 帳號改名 ====================

/// 帳號改名之後，讓資料夾跟著改名。失敗時沿用原本的名稱（照常可以使用），不影響改名本身。
///
/// 順序：先改硬碟上的名稱，再改資料庫；資料庫沒改成時把名稱改回去。
/// 兩者之間的一瞬間，正在進行的請求可能找不到檔案而失敗一次，重試就好。
///
/// 搬移期間不改：搬移的背景工作完成後會再呼叫一次
pub async fn sync_name(pool: &SqlitePool, data_dir: &Path, user_id: i64) {
    if let Err(e) = try_sync_name(pool, data_dir, user_id).await {
        eprintln!("storage: cannot rename the folder of user {user_id}: {e}");
    }
}

async fn try_sync_name(pool: &SqlitePool, data_dir: &Path, user_id: i64) -> Result<(), String> {
    let db = |e: sqlx::Error| format!("database error: {e}");
    let Some(name) = storage_db::folder_name(pool, user_id).await.map_err(db)? else {
        return Ok(());
    };
    if name.folder == name.username || name.moving {
        return Ok(());
    }

    let volume = volume_root(pool, data_dir, name.volume_id).await?;
    if name.volume_id.is_some() && !storage::volume_ready(&volume) {
        return Err(format!("{} isn't available", volume.display()));
    }
    let (Some(old), Some(new)) = (
        storage::user_root(&volume, &name.folder),
        storage::user_root(&volume, &name.username),
    ) else {
        return Err("invalid folder name".to_string());
    };

    if tokio::fs::try_exists(&new).await.unwrap_or(true) {
        return Err(format!(
            "{} already exists, keeping the folder name {}",
            new.display(),
            name.folder
        ));
    }
    // 還沒有任何檔案時資料夾可能還不存在：只改資料庫
    let on_disk = tokio::fs::try_exists(&old).await.unwrap_or(false);
    if on_disk {
        tokio::fs::rename(&old, &new)
            .await
            .map_err(|e| format!("cannot rename {} to {}: {e}", old.display(), new.display()))?;
    }

    let renamed = storage_db::set_folder(pool, user_id, &name.folder, &name.username).await;
    if !matches!(renamed, Ok(true)) {
        if on_disk && let Err(e) = tokio::fs::rename(&new, &old).await {
            return Err(format!(
                "the folder was renamed to {} but the database was not updated, \
                 and renaming it back failed: {e}",
                new.display()
            ));
        }
        return Err(match renamed {
            Err(e) => db(e),
            _ => "the folder name is already taken in this location".to_string(),
        });
    }

    println!(
        "storage: renamed the folder of user {user_id} to {}",
        new.display()
    );
    Ok(())
}

/// 啟動時：之前沒能改名的資料夾再試一次
pub async fn sync_all_names(pool: &SqlitePool, data_dir: &Path) {
    match storage_db::user_folders(pool).await {
        Ok(folders) => {
            for folder in folders {
                sync_name(pool, data_dir, folder.user_id).await;
            }
        }
        Err(e) => eprintln!("storage: cannot list the user folders: {e}"),
    }
}

// ==================== 舊版的目錄結構 ====================

/// 把舊版的共用目錄（`{位置}/blobs/` 等）中的檔案搬進每個人自己的資料夾。
///
/// 同一個位置內只是改名，很快，也不會用到額外的空間。每次啟動都會執行，
/// 已經整理好的位置什麼都不做；中途中斷時，下次啟動會接著做完。
///
/// 被參照的 blob 有任何一個搬不過去就回傳錯誤，伺服器不會啟動：
/// 否則那個檔案在新的位置找不到，整理完之後剩下的舊目錄又會被刪除。
///
/// 沒有掛載的硬碟跳過，下次啟動時再整理
pub async fn relayout(pool: &SqlitePool, data_dir: &Path) -> Result<(), String> {
    let db = |e: sqlx::Error| format!("database error: {e}");
    let mut roots = vec![(None, data_dir.to_path_buf())];
    for v in storage_db::list_volumes(pool).await.map_err(db)? {
        let path = PathBuf::from(v.path);
        if storage::volume_ready(&path) {
            roots.push((Some(v.id), path));
        }
    }
    let users = storage_db::user_folders(pool).await.map_err(db)?;

    for (volume, root) in roots {
        let legacy = root.join(LEGACY_DIR);
        if !set_aside(&root, &legacy).await? {
            continue;
        }
        println!(
            "storage: moving the files in {} into each user's folder",
            root.display()
        );

        let mut moved = 0usize;
        for user in users.iter().filter(|u| u.volume_id == volume) {
            let mine = storage::user_root(&root, &user.folder)
                .ok_or_else(|| format!("invalid storage folder name {:?}", user.folder))?;
            init_dirs(&mine).await?;

            for (key, _) in storage_db::move_keys(pool, user.user_id)
                .await
                .map_err(db)?
            {
                if let Some(from) = storage::blob_path(&legacy, &key)
                    && let Some(to) = storage::blob_path(&mine, &key)
                    && rename_if_present(&from, &to).await?
                {
                    moved += 1;
                }
            }
            for id in storage_db::upload_ids(pool, user.user_id)
                .await
                .map_err(db)?
            {
                if let Some(from) = storage::resumable::part_path(&legacy, &id)
                    && let Some(to) = storage::resumable::part_path(&mine, &id)
                {
                    rename_if_present(&from, &to).await?;
                }
            }
        }

        // 剩下的是沒有人參照的孤兒（垃圾回收本來就會刪掉），
        // 以及正在搬到這個位置的使用者已經複製過來的部分（搬移會重新複製到他的資料夾）
        tokio::fs::remove_dir_all(&legacy)
            .await
            .map_err(|e| format!("cannot remove {}: {e}", legacy.display()))?;
        println!("storage: moved {moved} files in {}", root.display());
    }
    Ok(())
}

/// 把舊版的 blobs/、tmp/、uploads/ 改名到 `legacy` 底下。回傳是否有需要整理的東西。
///
/// 只在 `legacy` 底下還沒有同名目錄時才改名：上次整理到一半中斷時，
/// `{位置}/blobs` 可能已經是名為 blobs 的使用者的資料夾，不能再把它當成舊目錄
async fn set_aside(root: &Path, legacy: &Path) -> Result<bool, String> {
    let mut found = tokio::fs::try_exists(legacy).await.unwrap_or(false);
    for name in ["blobs", "tmp", "uploads"] {
        let old = root.join(name);
        let aside = legacy.join(name);
        // 只看舊的結構：blobs/ 底下是兩個字元的分層目錄，而使用者資料夾底下是 blobs/ 等
        if !is_legacy_dir(&old).await || tokio::fs::try_exists(&aside).await.unwrap_or(true) {
            continue;
        }
        tokio::fs::create_dir_all(legacy)
            .await
            .map_err(|e| format!("cannot create {}: {e}", legacy.display()))?;
        tokio::fs::rename(&old, &aside)
            .await
            .map_err(|e| format!("cannot move {} aside: {e}", old.display()))?;
        found = true;
    }
    Ok(found)
}

/// 這個目錄是舊版的共用目錄，而不是名為 blobs、tmp、uploads 的使用者的資料夾：
/// 使用者的資料夾底下一定有 blobs/ 目錄，舊版的目錄底下不會有
async fn is_legacy_dir(path: &Path) -> bool {
    let is_dir = |m: std::fs::Metadata| m.is_dir();
    tokio::fs::metadata(path).await.is_ok_and(is_dir)
        && !tokio::fs::metadata(path.join("blobs"))
            .await
            .is_ok_and(is_dir)
}

async fn init_dirs(dir: &Path) -> Result<(), String> {
    let owned = dir.to_path_buf();
    tokio::task::spawn_blocking(move || storage::init_dirs(&owned))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// 改名；來源不存在時什麼都不做（已經搬過了，或檔案本來就不在）。回傳是否有改名
async fn rename_if_present(from: &Path, to: &Path) -> Result<bool, String> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 也可能是目的地的上層目錄不存在
            if tokio::fs::try_exists(from).await.unwrap_or(false) {
                if let Some(parent) = to.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                tokio::fs::rename(from, to)
                    .await
                    .map_err(|e| format!("cannot move {}: {e}", from.display()))?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(format!("cannot move {}: {e}", from.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{files, user};
    use crate::storage::TempBlob;

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    async fn store(root: &Path, key: &str) {
        storage::init_dirs(root).unwrap();
        let mut blob = TempBlob::create(root, u64::MAX).await.unwrap();
        blob.write_chunk(b"abc").await.unwrap();
        blob.persist(key).await.unwrap();
    }

    #[sqlx::test]
    async fn legacy_files_move_into_each_users_folder(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let alice = user::create_user(&pool, "alice", "x").await.unwrap();
        // 名為 blobs 的使用者：他的資料夾和舊版的共用目錄同名
        let blobs = user::create_user(&pool, "blobs", "x").await.unwrap();
        store(data.path(), &key(1)).await;
        store(data.path(), &key(2)).await;
        // 沒有人參照的孤兒
        store(data.path(), &key(3)).await;
        files::create_file(&pool, alice, None, "a", 3, None, &key(1))
            .await
            .unwrap();
        files::create_file(&pool, blobs, None, "b", 3, None, &key(2))
            .await
            .unwrap();

        relayout(&pool, data.path()).await.unwrap();

        let path =
            |folder: &str, k: &str| storage::blob_path(&data.path().join(folder), k).unwrap();
        assert!(path("alice", &key(1)).is_file());
        assert!(path("blobs", &key(2)).is_file());
        assert!(!data.path().join(LEGACY_DIR).exists());
        assert!(!data.path().join("tmp").exists());
        assert!(!data.path().join("uploads").exists());

        // 再執行一次：已經整理好了，什麼都不變
        relayout(&pool, data.path()).await.unwrap();
        assert!(path("alice", &key(1)).is_file());
        assert!(path("blobs", &key(2)).is_file());
    }

    #[sqlx::test]
    async fn folder_follows_the_new_username(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        store(&data.path().join("carol"), &key(1)).await;
        sqlx::query("UPDATE users SET username = 'caroline' WHERE id = ?")
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();

        sync_name(&pool, data.path(), carol).await;

        assert!(!data.path().join("carol").exists());
        assert!(
            storage::blob_path(&data.path().join("caroline"), &key(1))
                .unwrap()
                .is_file()
        );
        let here = storage_db::user_location(&pool, carol)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(here.folder, "caroline");
    }

    #[sqlx::test]
    async fn an_existing_folder_is_never_overwritten(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        store(&data.path().join("carol"), &key(1)).await;
        std::fs::create_dir(data.path().join("caroline")).unwrap();
        sqlx::query("UPDATE users SET username = 'caroline' WHERE id = ?")
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();

        sync_name(&pool, data.path(), carol).await;

        // 沿用原本的名稱
        assert!(data.path().join("carol").exists());
        let here = storage_db::user_location(&pool, carol)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(here.folder, "carol");
    }
}
