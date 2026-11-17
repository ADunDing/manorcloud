//! 儲存位置（storage_volumes）、每個使用者的位置，以及更換位置時的搬移進度（storage_moves）。
//!
//! 內建的資料目錄不在 storage_volumes 中：位置的編號為 `None` 就代表它。
//!
//! 與 uploads.rs 相同，使用執行期查詢而不是 query! 巨集：新表剛加入時，
//! 巨集需要一個已經有這些表的資料庫才能編譯。#[sqlx::test] 會套用所有 migration，
//! 測試照樣能驗證每一條 SQL。

use std::collections::HashSet;

use sqlx::{FromRow, SqlitePool};

/// 一個儲存位置
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Volume {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub created_at: i64,
}

/// 所有儲存位置，依名稱排序
pub async fn list_volumes(pool: &SqlitePool) -> Result<Vec<Volume>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, path, created_at FROM storage_volumes ORDER BY name")
        .fetch_all(pool)
        .await
}

pub async fn find_volume(pool: &SqlitePool, id: i64) -> Result<Option<Volume>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, path, created_at FROM storage_volumes WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

#[derive(Debug)]
pub enum AddVolumeError {
    NameTaken,
    PathTaken,
    Db(sqlx::Error),
}

/// 新增儲存位置。路徑必須已經由呼叫端驗證並解析成絕對路徑
pub async fn add_volume(
    pool: &SqlitePool,
    name: &str,
    path: &str,
) -> Result<Volume, AddVolumeError> {
    sqlx::query_as(
        "INSERT INTO storage_volumes (name, path) VALUES (?, ?) \
         RETURNING id, name, path, created_at",
    )
    .bind(name)
    .bind(path)
    .fetch_one(pool)
    .await
    .map_err(|e| match e.as_database_error() {
        // 兩個欄位都是 UNIQUE：從錯誤訊息分辨是哪一個
        Some(db) if db.is_unique_violation() && db.message().contains("storage_volumes.name") => {
            AddVolumeError::NameTaken
        }
        Some(db) if db.is_unique_violation() => AddVolumeError::PathTaken,
        _ => AddVolumeError::Db(e),
    })
}

#[derive(Debug)]
pub enum DeleteVolumeError {
    NotFound,
    /// 還有使用者在這個位置，或正在搬過來
    InUse,
    /// 管理員在這個位置上的檔案空間還有檔案（含垃圾桶）
    HasFiles,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for DeleteVolumeError {
    fn from(e: sqlx::Error) -> Self {
        DeleteVolumeError::Db(e)
    }
}

/// 刪除儲存位置的紀錄，以及管理員在這裡的檔案空間（必須已經是空的）。
/// 回傳檔案空間的資料夾名稱，呼叫端把它從磁碟上移除；其他資料夾不受影響。
///
/// 檢查與刪除在同一個 IMMEDIATE 交易中：檢查之後不會有人剛好被換到這個位置
pub async fn delete_volume(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<String>, DeleteVolumeError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let in_use: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE storage_volume_id = ?1 \
                          AND id NOT IN (SELECT user_id FROM storage_spaces)) \
             OR EXISTS (SELECT 1 FROM storage_moves WHERE target_volume_id = ?1)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if in_use {
        return Err(DeleteVolumeError::InUse);
    }

    let space: Option<(i64, String)> = sqlx::query_as(
        "SELECT users.id, COALESCE(users.storage_folder, users.username) \
         FROM storage_spaces JOIN users ON users.id = storage_spaces.user_id \
         WHERE storage_spaces.volume_key = ?",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((space_user, _)) = &space {
        let has_files: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM files WHERE owner_id = ?)")
                .bind(space_user)
                .fetch_one(&mut *tx)
                .await?;
        if has_files {
            return Err(DeleteVolumeError::HasFiles);
        }
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(space_user)
            .execute(&mut *tx)
            .await?;
    }

    let deleted = sqlx::query("DELETE FROM storage_volumes WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(DeleteVolumeError::NotFound);
    }

    tx.commit().await?;
    Ok(space.map(|(_, folder)| folder))
}

// ==================== 管理員在各硬碟上的檔案空間 ====================

/// 檔案空間在磁碟上的資料夾名稱。含有 `.`，一般帳號名稱不允許，不會撞名
pub const SPACE_FOLDER: &str = "manorcloud.admin";

/// 管理員在這個位置上的檔案空間（隱藏帳號）的編號，第一次使用時建立。
/// 位置必須存在，由呼叫端先確認
pub async fn space_account(pool: &SqlitePool, volume: Option<i64>) -> Result<i64, sqlx::Error> {
    let key = volume.unwrap_or(0);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let existing: Option<i64> =
        sqlx::query_scalar("SELECT user_id FROM storage_spaces WHERE volume_key = ?")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(id) = existing {
        return Ok(id);
    }

    // 停用、沒有密碼：不能登入。帳號名稱含有 ~，一般帳號不允許
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO users (username, disabled, storage_volume_id, storage_folder) \
         VALUES (?, 1, ?, ?) RETURNING id",
    )
    .bind(format!("~disk-{key}"))
    .bind(volume)
    .bind(SPACE_FOLDER)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO storage_spaces (volume_key, user_id) VALUES (?, ?)")
        .bind(key)
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(id)
}

// ==================== 使用者的位置 ====================

/// 使用者的檔案目前在哪裡：`{位置}/{folder}/`
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UserLocation {
    /// None：資料目錄
    pub volume_id: Option<i64>,
    /// 位置的路徑；資料目錄時為 None
    pub path: Option<String>,
    /// 他在這個位置中的資料夾名稱
    pub folder: String,
    /// 正在搬到另一個位置（不含失敗、等待重試的搬移）
    pub moving: bool,
}

/// 使用者的檔案所在的位置；使用者不存在時回傳 None
pub async fn user_location(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<UserLocation>, sqlx::Error> {
    sqlx::query_as(
        "SELECT users.storage_volume_id AS volume_id, storage_volumes.path AS path, \
                COALESCE(users.storage_folder, users.username) AS folder, \
                EXISTS (SELECT 1 FROM storage_moves \
                        WHERE storage_moves.user_id = users.id AND storage_moves.error IS NULL) \
                    AS moving \
         FROM users \
         LEFT JOIN storage_volumes ON storage_volumes.id = users.storage_volume_id \
         WHERE users.id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// 每個使用者在哪個位置、哪個資料夾（管理頁的使用者列表與側邊欄用）
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Assignment {
    pub user_id: i64,
    pub username: String,
    pub volume_id: Option<i64>,
    pub folder: String,
}

pub async fn assignments(pool: &SqlitePool) -> Result<Vec<Assignment>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id AS user_id, username, storage_volume_id AS volume_id, \
                COALESCE(storage_folder, username) AS folder \
         FROM users \
         WHERE id NOT IN (SELECT user_id FROM storage_spaces) \
         ORDER BY username",
    )
    .fetch_all(pool)
    .await
}

/// 每個使用者的資料夾，以及正在搬過去的位置（垃圾回收與啟動時整理資料夾用）
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UserFolder {
    pub user_id: i64,
    pub volume_id: Option<i64>,
    pub folder: String,
    /// 有搬移紀錄（進行中或失敗）
    pub moving: bool,
    /// 搬移的目的地；沒有搬移、或搬到資料目錄時為 None
    pub target_volume_id: Option<i64>,
}

pub async fn user_folders(pool: &SqlitePool) -> Result<Vec<UserFolder>, sqlx::Error> {
    sqlx::query_as(
        "SELECT users.id AS user_id, users.storage_volume_id AS volume_id, \
                COALESCE(users.storage_folder, users.username) AS folder, \
                storage_moves.id IS NOT NULL AS moving, \
                storage_moves.target_volume_id AS target_volume_id \
         FROM users LEFT JOIN storage_moves ON storage_moves.user_id = users.id \
         ORDER BY users.id",
    )
    .fetch_all(pool)
    .await
}

/// 使用者未完成的上傳（啟動時整理資料夾用）
pub async fn upload_ids(pool: &SqlitePool, user_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM uploads WHERE owner_id = ? ORDER BY id")
        .bind(user_id)
        .fetch_all(pool)
        .await
}

/// 帳號名稱與資料夾名稱（帳號改名後，讓資料夾跟著改名用）
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct FolderName {
    pub username: String,
    pub volume_id: Option<i64>,
    pub folder: String,
    pub moving: bool,
}

pub async fn folder_name(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<FolderName>, sqlx::Error> {
    sqlx::query_as(
        "SELECT username, storage_volume_id AS volume_id, \
                COALESCE(storage_folder, username) AS folder, \
                EXISTS (SELECT 1 FROM storage_moves WHERE storage_moves.user_id = users.id) \
                    AS moving \
         FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// 修改資料夾名稱。回傳 false：名稱已經不是 `old`，或同一個位置中已經有人用了 `new`
pub async fn set_folder(
    pool: &SqlitePool,
    user_id: i64,
    old: &str,
    new: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE users SET storage_folder = ? \
         WHERE id = ? AND COALESCE(storage_folder, username) = ?",
    )
    .bind(new)
    .bind(user_id)
    .bind(old)
    .execute(pool)
    .await;
    match result {
        Ok(done) => Ok(done.rows_affected() == 1),
        Err(e)
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation()) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

// ==================== 搬移 ====================

/// 一筆搬移
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Move {
    pub id: i64,
    pub user_id: i64,
    pub target_volume_id: Option<i64>,
    pub total_bytes: i64,
    pub moved_bytes: i64,
    pub error: Option<String>,
}

macro_rules! move_columns {
    () => {
        "id, user_id, target_volume_id, total_bytes, moved_bytes, error"
    };
}

pub async fn list_moves(pool: &SqlitePool) -> Result<Vec<Move>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        move_columns!(),
        " FROM storage_moves ORDER BY id"
    );
    sqlx::query_as(SQL).fetch_all(pool).await
}

pub async fn find_move(pool: &SqlitePool, id: i64) -> Result<Option<Move>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        move_columns!(),
        " FROM storage_moves WHERE id = ?"
    );
    sqlx::query_as(SQL).bind(id).fetch_optional(pool).await
}

/// 進行中的搬移（伺服器啟動時接著做完）
pub async fn running_moves(pool: &SqlitePool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM storage_moves WHERE error IS NULL ORDER BY id")
        .fetch_all(pool)
        .await
}

/// 開始更換位置的結果
#[derive(Debug, PartialEq)]
pub enum StartMove {
    /// 沒有任何檔案：直接換好了，不需要搬移。
    /// `cancelled_uploads` 是被取消的未完成上傳，呼叫端刪除它們的暫存檔
    Switched { cancelled_uploads: Vec<String> },
    /// 已建立搬移紀錄，呼叫端接著啟動背景工作。
    /// `cancelled_uploads` 是被取消的未完成上傳，呼叫端刪除它們的暫存檔
    Started {
        move_id: i64,
        cancelled_uploads: Vec<String>,
    },
}

#[derive(Debug)]
pub enum StartMoveError {
    UserNotFound,
    /// 已經在這個位置
    AlreadyThere,
    /// 已經有一個進行中的搬移
    InProgress,
    /// 目的地已經有同名的資料夾（屬於另一個使用者）
    FolderTaken,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for StartMoveError {
    fn from(e: sqlx::Error) -> Self {
        StartMoveError::Db(e)
    }
}

/// 把使用者換到另一個位置。目的地是否存在由呼叫端先確認（外鍵也會擋下）。
///
/// - 沒有任何檔案（含垃圾桶）：直接修改位置
/// - 有檔案：建立搬移紀錄，並取消這個人所有未完成的上傳。
///   它們的暫存檔在原本的位置，搬完之後無法在新位置完成
///
/// 失敗、等待重試的搬移會被新的取代
pub async fn start_move(
    pool: &SqlitePool,
    user_id: i64,
    target: Option<i64>,
) -> Result<StartMove, StartMoveError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let current: Option<Option<i64>> =
        sqlx::query_scalar("SELECT storage_volume_id FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(current) = current else {
        return Err(StartMoveError::UserNotFound);
    };

    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM storage_moves WHERE user_id = ? AND error IS NULL)",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if running {
        return Err(StartMoveError::InProgress);
    }
    // 失敗的搬移：被這一次取代（換回原本的位置也算，等於取消）
    sqlx::query("DELETE FROM storage_moves WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    if current == target {
        tx.commit().await?;
        return Err(StartMoveError::AlreadyThere);
    }

    // 目的地的資料夾名稱不能和別人重複（帳號改名時資料夾沒能跟著改名才可能發生）
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users AS other, users AS me \
                        WHERE me.id = ?1 AND other.id <> me.id \
                          AND IFNULL(other.storage_volume_id, 0) = IFNULL(?2, 0) \
                          AND COALESCE(other.storage_folder, other.username) \
                              = COALESCE(me.storage_folder, me.username))",
    )
    .bind(user_id)
    .bind(target)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        tx.commit().await?;
        return Err(StartMoveError::FolderTaken);
    }

    let (files, total): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM files \
         WHERE owner_id = ? AND storage_key IS NOT NULL",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;

    if files == 0 {
        sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
            .bind(target)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        // 未完成的上傳也在原本的位置
        let cancelled = cancel_uploads(&mut tx, user_id).await?;
        tx.commit().await?;
        return Ok(StartMove::Switched {
            cancelled_uploads: cancelled,
        });
    }

    let move_id: i64 = sqlx::query_scalar(
        "INSERT INTO storage_moves (user_id, target_volume_id, total_bytes) \
         VALUES (?, ?, ?) RETURNING id",
    )
    .bind(user_id)
    .bind(target)
    .bind(total)
    .fetch_one(&mut *tx)
    .await?;
    let cancelled = cancel_uploads(&mut tx, user_id).await?;

    tx.commit().await?;
    Ok(StartMove::Started {
        move_id,
        cancelled_uploads: cancelled,
    })
}

async fn cancel_uploads(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM uploads WHERE owner_id = ? RETURNING id")
        .bind(user_id)
        .fetch_all(&mut **tx)
        .await
}

/// 要搬的檔案：(storage_key, 大小)，含垃圾桶
pub async fn move_keys(pool: &SqlitePool, user_id: i64) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT storage_key, COALESCE(size, 0) FROM files \
         WHERE owner_id = ? AND storage_key IS NOT NULL ORDER BY storage_key",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// 更新進度。回傳 false：這筆搬移已經被取消（或使用者被刪除），背景工作應該停下來
pub async fn set_progress(
    pool: &SqlitePool,
    move_id: i64,
    moved: i64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE storage_moves SET moved_bytes = MIN(?, total_bytes) WHERE id = ? AND error IS NULL",
    )
    .bind(moved)
    .bind(move_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 記錄失敗的原因。新增檔案不再被擋下，管理員可以重試或取消
pub async fn fail_move(pool: &SqlitePool, move_id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE storage_moves SET error = ? WHERE id = ?")
        .bind(error)
        .bind(move_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// `finish_move` 的結果
#[derive(Debug, PartialEq)]
pub enum Finish {
    /// 已經換到新位置
    Done,
    /// 紀錄已經不在或已經失敗（被取消、使用者被刪除），什麼都沒有改
    Cancelled,
    /// 還有沒複製到的檔案：搬移開始的那一刻剛好有上傳或複製正在完成。
    /// 呼叫端把它們也複製過去，再呼叫一次
    Missing(Vec<(String, i64)>),
}

/// 搬完了：確認每個檔案都已經複製，再修改使用者的位置並刪除搬移紀錄，在同一個交易中完成。
///
/// 為什麼要再確認一次：搬移開始時才被擋下的是「之後才開始」的上傳與複製，
/// 開始之前就已經在進行、剛好在這段期間完成的，它們的 blob 寫在原本的位置。
/// IMMEDIATE 交易讓確認與切換之間不會再有新的檔案寫入
pub async fn finish_move(
    pool: &SqlitePool,
    move_id: i64,
    copied: &HashSet<String>,
) -> Result<Finish, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let found: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, target_volume_id FROM storage_moves WHERE id = ? AND error IS NULL",
    )
    .bind(move_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, target)) = found else {
        return Ok(Finish::Cancelled);
    };

    let keys: Vec<(String, i64)> = sqlx::query_as(
        "SELECT storage_key, COALESCE(size, 0) FROM files \
         WHERE owner_id = ? AND storage_key IS NOT NULL ORDER BY storage_key",
    )
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    let missing: Vec<(String, i64)> = keys
        .into_iter()
        .filter(|(key, _)| !copied.contains(key))
        .collect();
    if !missing.is_empty() {
        // tx 在這裡被丟棄，自動 rollback
        return Ok(Finish::Missing(missing));
    }

    sqlx::query("DELETE FROM storage_moves WHERE id = ?")
        .bind(move_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
        .bind(target)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Finish::Done)
}

/// 重試失敗的搬移。回傳 false：沒有這個使用者的失敗紀錄
pub async fn retry_move(pool: &SqlitePool, user_id: i64) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE storage_moves SET error = NULL, moved_bytes = 0 \
         WHERE user_id = ? AND error IS NOT NULL RETURNING id",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// 取消搬移（進行中或失敗的）：檔案留在原本的位置。
/// 已經複製到目的地的部分沒有紀錄參照，由垃圾回收清除。回傳是否有紀錄被刪除
pub async fn cancel_move(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM storage_moves WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

// ==================== 垃圾回收 ====================

/// 某個使用者、某個分層中的 storage_key（垃圾回收用）。
///
/// 每個使用者有自己的資料夾，垃圾回收逐一檢查每個人的資料夾：
/// 資料夾中不屬於這個人的 blob 都是孤兒（例如搬移完成後留在原本位置的複本）。
///
/// 用範圍查詢 `>= 'ab' AND < 'abg'` 而不是 `substr(storage_key, 1, 2) = 'ab'`：
/// 後者會讓 storage_key 的唯一索引失效。
/// 上界成立的原因：key 只含 0-9a-f，第三個字元最大是 'f'，而 'g' 排在 'f' 之後。
pub async fn keys_in_shard(
    pool: &SqlitePool,
    user_id: i64,
    shard: &str,
) -> Result<Vec<String>, sqlx::Error> {
    let upper = format!("{shard}g");
    sqlx::query_scalar(
        "SELECT storage_key FROM files \
         WHERE storage_key >= ? AND storage_key < ? AND owner_id = ?",
    )
    .bind(shard)
    .bind(upper)
    .bind(user_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{files, user};

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    async fn volume(pool: &SqlitePool, name: &str) -> i64 {
        add_volume(pool, name, &format!("/mnt/{name}"))
            .await
            .unwrap()
            .id
    }

    async fn location(pool: &SqlitePool, user_id: i64) -> UserLocation {
        user_location(pool, user_id).await.unwrap().unwrap()
    }

    #[sqlx::test]
    async fn volume_names_and_paths_are_unique(pool: SqlitePool) {
        add_volume(&pool, "Disk 2", "/mnt/disk2").await.unwrap();

        assert!(matches!(
            add_volume(&pool, "disk 2", "/mnt/other").await,
            Err(AddVolumeError::NameTaken)
        ));
        assert!(matches!(
            add_volume(&pool, "Other", "/mnt/disk2").await,
            Err(AddVolumeError::PathTaken)
        ));
        // 相對路徑被 CHECK 擋下
        assert!(matches!(
            add_volume(&pool, "Bad", "mnt/x").await,
            Err(AddVolumeError::Db(_))
        ));
    }

    #[sqlx::test]
    async fn new_users_are_in_the_data_directory(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();

        let here = location(&pool, carol).await;

        assert_eq!(here.volume_id, None);
        assert_eq!(here.path, None);
        assert!(!here.moving);
        assert!(user_location(&pool, 9999).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn user_without_files_switches_immediately(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;

        let result = start_move(&pool, carol, Some(disk)).await.unwrap();

        assert_eq!(
            result,
            StartMove::Switched {
                cancelled_uploads: vec![]
            }
        );
        let here = location(&pool, carol).await;
        assert_eq!(here.volume_id, Some(disk));
        assert_eq!(here.path.as_deref(), Some("/mnt/disk2"));
        assert!(matches!(
            start_move(&pool, carol, Some(disk)).await,
            Err(StartMoveError::AlreadyThere)
        ));
    }

    #[sqlx::test]
    async fn moving_files_tracks_progress_and_finishes(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        files::create_file(&pool, carol, None, "a", 10, None, &key(1))
            .await
            .unwrap();
        files::create_file(&pool, carol, None, "b", 5, None, &key(2))
            .await
            .unwrap();

        let StartMove::Started { move_id, .. } =
            start_move(&pool, carol, Some(disk)).await.unwrap()
        else {
            panic!("expected a move");
        };

        assert!(location(&pool, carol).await.moving);
        assert!(matches!(
            start_move(&pool, carol, None).await,
            Err(StartMoveError::InProgress)
        ));
        assert_eq!(
            move_keys(&pool, carol).await.unwrap(),
            [(key(1), 10), (key(2), 5)]
        );
        assert!(set_progress(&pool, move_id, 10).await.unwrap());
        assert_eq!(
            find_move(&pool, move_id)
                .await
                .unwrap()
                .unwrap()
                .moved_bytes,
            10
        );

        // 只複製了一個：不能完成，回報還沒複製的
        let mut copied = HashSet::from([key(1)]);
        assert_eq!(
            finish_move(&pool, move_id, &copied).await.unwrap(),
            Finish::Missing(vec![(key(2), 5)])
        );
        assert!(location(&pool, carol).await.moving);

        copied.insert(key(2));
        assert_eq!(
            finish_move(&pool, move_id, &copied).await.unwrap(),
            Finish::Done
        );

        let here = location(&pool, carol).await;
        assert_eq!(here.volume_id, Some(disk));
        assert!(!here.moving);
        assert!(list_moves(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn starting_a_move_cancels_unfinished_uploads(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        files::create_file(&pool, carol, None, "a", 1, None, &key(1))
            .await
            .unwrap();
        sqlx::query("INSERT INTO uploads (id, owner_id, name, size) VALUES (?, ?, 'x', 3)")
            .bind(key(9))
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();

        let result = start_move(&pool, carol, Some(disk)).await.unwrap();

        let StartMove::Started {
            cancelled_uploads, ..
        } = result
        else {
            panic!("expected a move");
        };
        assert_eq!(cancelled_uploads, [key(9)]);
    }

    #[sqlx::test]
    async fn failed_move_stops_blocking_and_can_be_retried_or_cancelled(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        files::create_file(&pool, carol, None, "a", 1, None, &key(1))
            .await
            .unwrap();
        let StartMove::Started { move_id, .. } =
            start_move(&pool, carol, Some(disk)).await.unwrap()
        else {
            panic!("expected a move");
        };

        fail_move(&pool, move_id, "disk full").await.unwrap();

        assert!(!location(&pool, carol).await.moving);
        // 失敗的紀錄不能再推進或完成
        assert!(!set_progress(&pool, move_id, 1).await.unwrap());
        assert_eq!(
            finish_move(&pool, move_id, &HashSet::from([key(1)]))
                .await
                .unwrap(),
            Finish::Cancelled
        );
        assert_eq!(location(&pool, carol).await.volume_id, None);

        assert_eq!(retry_move(&pool, carol).await.unwrap(), Some(move_id));
        assert!(location(&pool, carol).await.moving);
        assert_eq!(retry_move(&pool, carol).await.unwrap(), None);

        assert!(cancel_move(&pool, carol).await.unwrap());
        assert!(!location(&pool, carol).await.moving);
        // 取消之後，舊的背景工作不能再更新任何東西
        assert!(!set_progress(&pool, move_id, 1).await.unwrap());

        // 立刻重新開始：新的紀錄不會拿到舊的編號，舊的工作仍然碰不到它
        let StartMove::Started { move_id: again, .. } =
            start_move(&pool, carol, Some(disk)).await.unwrap()
        else {
            panic!("expected a move");
        };
        assert_ne!(again, move_id);
        assert!(!set_progress(&pool, move_id, 1).await.unwrap());
    }

    #[sqlx::test]
    async fn volumes_in_use_cannot_be_deleted(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        start_move(&pool, carol, Some(disk)).await.unwrap();

        assert!(matches!(
            delete_volume(&pool, disk).await,
            Err(DeleteVolumeError::InUse)
        ));

        start_move(&pool, carol, None).await.unwrap();
        delete_volume(&pool, disk).await.unwrap();
        assert!(matches!(
            delete_volume(&pool, disk).await,
            Err(DeleteVolumeError::NotFound)
        ));
    }

    #[sqlx::test]
    async fn keys_in_shard_returns_only_that_users_shard(pool: SqlitePool) {
        let owner = user::create_user(&pool, "carol", "x").await.unwrap();
        let other = user::create_user(&pool, "alice", "x").await.unwrap();
        let in_00 = key(1); // "000...01"，分層 00
        let in_01 = format!("01{}", "0".repeat(30));
        let others = key(2);
        for (i, k) in [&in_00, &in_01].into_iter().enumerate() {
            files::create_file(&pool, owner, None, &format!("{i}.txt"), 0, None, k)
                .await
                .unwrap();
        }
        files::create_file(&pool, other, None, "x.txt", 0, None, &others)
            .await
            .unwrap();
        // 資料夾沒有 storage_key，不應出現在任何分層
        files::create_folder(&pool, owner, None, "docs")
            .await
            .unwrap();

        assert_eq!(
            keys_in_shard(&pool, owner, "00").await.unwrap(),
            vec![in_00]
        );
        assert_eq!(
            keys_in_shard(&pool, owner, "01").await.unwrap(),
            vec![in_01]
        );
        assert!(keys_in_shard(&pool, owner, "ff").await.unwrap().is_empty());
        assert_eq!(
            keys_in_shard(&pool, other, "00").await.unwrap(),
            vec![others]
        );
    }

    #[sqlx::test]
    async fn every_user_gets_a_folder_named_after_them(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();

        assert_eq!(location(&pool, carol).await.folder, "carol");

        // 改名：名稱必須還是舊的才會修改
        assert!(set_folder(&pool, carol, "carol", "caroline").await.unwrap());
        assert!(!set_folder(&pool, carol, "carol", "other").await.unwrap());
        assert_eq!(location(&pool, carol).await.folder, "caroline");
    }

    #[sqlx::test]
    async fn folder_names_are_unique_within_a_location(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let alice = user::create_user(&pool, "alice", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;

        // 同一個位置不能重複
        assert!(!set_folder(&pool, alice, "alice", "carol").await.unwrap());

        // 不同位置可以：alice 搬到 disk2 之後，在那裡叫 carol 也沒有關係
        start_move(&pool, alice, Some(disk)).await.unwrap();
        assert!(set_folder(&pool, alice, "alice", "carol").await.unwrap());

        // 但 carol 不能再搬到 disk2：那裡已經有叫 carol 的資料夾
        files::create_file(&pool, carol, None, "a", 1, None, &key(1))
            .await
            .unwrap();
        assert!(matches!(
            start_move(&pool, carol, Some(disk)).await,
            Err(StartMoveError::FolderTaken)
        ));
    }
}
