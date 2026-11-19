//! 每位一般使用者在硬碟上的資料夾：`{硬碟}/users/{名稱}`，也就是他的 My files。
//!
//! 管理員的 My files 是硬碟的最上層，沒有自己的資料夾。
//!
//! - `user_folder`：使用者的資料夾在哪裡
//! - `create_user_folder`：新增使用者、換位置之後建立資料夾
//! - `sync_name`：帳號改名之後，讓資料夾跟著改名

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::db::storage as storage_db;
use crate::storage;

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

/// 使用者在 `users` 中的資料夾（不論他是不是管理員）。使用者不存在時回傳 None
pub async fn user_folder(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
) -> Result<Option<PathBuf>, String> {
    let Some(here) = storage_db::user_location(pool, user_id)
        .await
        .map_err(|e| format!("database error: {e}"))?
    else {
        return Ok(None);
    };
    let root = volume_root(pool, data_dir, here.volume_id).await?;
    storage::user_dir(&root, &here.folder)
        .map(Some)
        .ok_or_else(|| format!("invalid storage folder name {:?}", here.folder))
}

/// 建立一般使用者的資料夾。失敗只記錄下來：第一次使用時會再建立
pub async fn create_user_folder(pool: &SqlitePool, data_dir: &Path, user_id: i64) {
    let result = async {
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("database error: {e}"))?;
        if role.as_deref() != Some("member") {
            return Ok(());
        }
        let Some(folder) = user_folder(pool, data_dir, user_id).await? else {
            return Ok(());
        };
        let root = folder
            .parent()
            .and_then(Path::parent)
            .ok_or("invalid folder")?;
        if root != data_dir && !storage::volume_ready(root) {
            return Err(format!("{} isn't available", root.display()));
        }
        tokio::fs::create_dir_all(&folder)
            .await
            .map_err(|e| format!("cannot create {}: {e}", folder.display()))
    }
    .await;
    if let Err(e) = result {
        eprintln!("storage: cannot create the folder of user {user_id}: {e}");
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
        storage::user_dir(&volume, &name.folder),
        storage::user_dir(&volume, &name.username),
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
    // 管理員通常沒有自己的資料夾：只改資料庫
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    #[sqlx::test]
    async fn members_get_a_folder_under_users(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();

        create_user_folder(&pool, data.path(), carol).await;

        assert!(data.path().join("users/carol").is_dir());
        assert_eq!(
            user_folder(&pool, data.path(), carol).await.unwrap(),
            Some(data.path().join("users/carol"))
        );
    }

    #[sqlx::test]
    async fn the_folder_follows_a_rename(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        create_user_folder(&pool, data.path(), carol).await;
        std::fs::write(data.path().join("users/carol/a.txt"), b"a").unwrap();

        sqlx::query("UPDATE users SET username = 'caroline' WHERE id = ?")
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();
        sync_name(&pool, data.path(), carol).await;

        assert_eq!(
            std::fs::read(data.path().join("users/caroline/a.txt")).unwrap(),
            b"a"
        );
        assert!(!data.path().join("users/carol").exists());
    }
}
