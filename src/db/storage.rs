//! 儲存位置（storage_volumes）、每個使用者的位置，以及更換位置時的搬移進度（storage_moves）。
//!
//! 內建的資料目錄不在 storage_volumes 中：位置的編號為 `None` 就代表它。
//!
//! 與 uploads.rs 相同，使用執行期查詢而不是 query! 巨集：新表剛加入時，
//! 巨集需要一個已經有這些表的資料庫才能編譯。#[sqlx::test] 會套用所有 migration，
//! 測試照樣能驗證每一條 SQL。

use sqlx::{FromRow, SqlitePool};

use super::mounts;

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
    /// 有複製或搬移正在讀寫這個位置
    Busy,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for DeleteVolumeError {
    fn from(e: sqlx::Error) -> Self {
        DeleteVolumeError::Db(e)
    }
}

/// 移除儲存位置：ManorCloud 不再使用它，但硬碟上的檔案一個都不刪（由呼叫端保證不碰硬碟）。
///
/// 同一個 IMMEDIATE 交易中確認沒有人的檔案在這裡、沒有傳輸正在進行，
/// 檢查之後不會有人剛好被換過來，也不會剛好開始新的傳輸。
///
/// 指向這個位置的紀錄一併刪除：分享連結、未完成的上傳、傳輸的紀錄（以 space= 的文字記錄），
/// 以及垃圾桶的紀錄（外鍵 ON DELETE CASCADE）。位置的編號之後可能被新加入的位置重新使用，
/// 留著的話，舊的分享連結會指到另一顆硬碟上的同一個路徑
pub async fn delete_volume(pool: &SqlitePool, id: i64) -> Result<(), DeleteVolumeError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let space = id.to_string();

    let in_use: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE storage_volume_id = ?1) \
             OR EXISTS (SELECT 1 FROM storage_moves WHERE target_volume_id = ?1)",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if in_use {
        return Err(DeleteVolumeError::InUse);
    }

    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM transfers \
             WHERE state = 'running' AND (from_space = ?1 OR to_space = ?1))",
    )
    .bind(&space)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(DeleteVolumeError::Busy);
    }

    for sql in [
        "DELETE FROM shares WHERE space = ?",
        "DELETE FROM uploads WHERE space = ?",
        "DELETE FROM transfers WHERE from_space = ?1 OR to_space = ?1",
    ] {
        sqlx::query(sql).bind(&space).execute(&mut *tx).await?;
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
    Ok(())
}

// ==================== 使用者的位置 ====================

/// 使用者的檔案目前在哪裡：一般使用者是 `{位置}/users/{folder}/`，管理員是位置的最上層
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
         FROM users ORDER BY username",
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
    /// 目的地掛載給了其他人：不能放任何人的 My files（見 db::mounts）
    Mounted,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for StartMoveError {
    fn from(e: sqlx::Error) -> Self {
        StartMoveError::Db(e)
    }
}

/// 把使用者換到另一個位置。目的地是否存在由呼叫端先確認（外鍵也會擋下）。
///
/// `total_bytes` 是要搬的大小（My files 與垃圾桶），由呼叫端從硬碟上算出：
/// - None（沒有任何檔案）：直接修改位置
/// - Some：建立搬移紀錄，由背景工作複製過去
///
/// 兩種情況都會取消這個人上傳到 My files、還沒完成的上傳：
/// 它們的暫存檔在原本的位置，搬完之後無法在新位置完成。
/// 失敗、等待重試的搬移會被新的取代
pub async fn start_move(
    pool: &SqlitePool,
    user_id: i64,
    target: Option<i64>,
    total_bytes: Option<i64>,
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
    if let Some(target) = target
        && mounts::is_mounted(&mut tx, target).await?
    {
        tx.commit().await?;
        return Err(StartMoveError::Mounted);
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

    let cancelled = cancel_uploads(&mut tx, user_id).await?;
    let Some(total) = total_bytes else {
        sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
            .bind(target)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(StartMove::Switched {
            cancelled_uploads: cancelled,
        });
    };

    let move_id: i64 = sqlx::query_scalar(
        "INSERT INTO storage_moves (user_id, target_volume_id, total_bytes) \
         VALUES (?, ?, ?) RETURNING id",
    )
    .bind(user_id)
    .bind(target)
    .bind(total.max(0))
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(StartMove::Started {
        move_id,
        cancelled_uploads: cancelled,
    })
}

/// 直接修改使用者的位置，不搬任何檔案（管理員：他的 My files 就是那顆硬碟的最上層）
pub async fn set_volume(
    pool: &SqlitePool,
    user_id: i64,
    target: Option<i64>,
) -> Result<(), StartMoveError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(target) = target
        && mounts::is_mounted(&mut tx, target).await?
    {
        return Err(StartMoveError::Mounted);
    }
    let updated = sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
        .bind(target)
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(StartMoveError::UserNotFound);
    }
    tx.commit().await?;
    Ok(())
}

async fn cancel_uploads(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    user_id: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM uploads WHERE owner_id = ? AND space = 'me' RETURNING id")
        .bind(user_id)
        .fetch_all(&mut **tx)
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
}

/// 搬完了：修改使用者的位置（垃圾桶的紀錄也一起），並刪除搬移紀錄，在同一個交易中完成
pub async fn finish_move(pool: &SqlitePool, move_id: i64) -> Result<Finish, sqlx::Error> {
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

    sqlx::query("DELETE FROM storage_moves WHERE id = ?")
        .bind(move_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
        .bind(target)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE trash SET volume_id = ? WHERE owner_id = ?")
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
/// 已經複製到目的地的部分在那顆硬碟的 .manorcloud/tmp 中，由垃圾回收清除。回傳是否有紀錄被刪除
pub async fn cancel_move(pool: &SqlitePool, user_id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM storage_moves WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

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

        let result = start_move(&pool, carol, Some(disk), None).await.unwrap();

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
            start_move(&pool, carol, Some(disk), None).await,
            Err(StartMoveError::AlreadyThere)
        ));
    }

    #[sqlx::test]
    async fn moving_files_tracks_progress_and_finishes(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        sqlx::query("INSERT INTO trash (owner_id, path, kind) VALUES (?, 'a', 'file')")
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();

        let StartMove::Started { move_id, .. } = start_move(&pool, carol, Some(disk), Some(15))
            .await
            .unwrap()
        else {
            panic!("expected a move");
        };

        assert!(location(&pool, carol).await.moving);
        assert!(matches!(
            start_move(&pool, carol, None, None).await,
            Err(StartMoveError::InProgress)
        ));
        assert!(set_progress(&pool, move_id, 10).await.unwrap());
        assert_eq!(
            find_move(&pool, move_id)
                .await
                .unwrap()
                .unwrap()
                .moved_bytes,
            10
        );

        assert_eq!(finish_move(&pool, move_id).await.unwrap(), Finish::Done);

        let here = location(&pool, carol).await;
        assert_eq!(here.volume_id, Some(disk));
        assert!(!here.moving);
        assert!(list_moves(&pool).await.unwrap().is_empty());
        // 垃圾桶的紀錄跟著換到新的位置
        let trash_volume: Option<i64> = sqlx::query_scalar("SELECT volume_id FROM trash")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(trash_volume, Some(disk));
    }

    #[sqlx::test]
    async fn changing_location_cancels_unfinished_uploads(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        let id = format!("{:032x}", 9);
        sqlx::query(
            "INSERT INTO uploads (id, owner_id, space, dir, name, size) \
             VALUES (?, ?, 'me', '', 'x', 3)",
        )
        .bind(&id)
        .bind(carol)
        .execute(&pool)
        .await
        .unwrap();

        let result = start_move(&pool, carol, Some(disk), Some(1)).await.unwrap();

        let StartMove::Started {
            cancelled_uploads, ..
        } = result
        else {
            panic!("expected a move");
        };
        assert_eq!(cancelled_uploads, [id]);
    }

    #[sqlx::test]
    async fn failed_move_stops_blocking_and_can_be_retried_or_cancelled(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let disk = volume(&pool, "disk2").await;
        let StartMove::Started { move_id, .. } =
            start_move(&pool, carol, Some(disk), Some(1)).await.unwrap()
        else {
            panic!("expected a move");
        };

        fail_move(&pool, move_id, "disk full").await.unwrap();

        assert!(!location(&pool, carol).await.moving);
        // 失敗的紀錄不能再推進或完成
        assert!(!set_progress(&pool, move_id, 1).await.unwrap());
        assert_eq!(
            finish_move(&pool, move_id).await.unwrap(),
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
            start_move(&pool, carol, Some(disk), Some(1)).await.unwrap()
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
        start_move(&pool, carol, Some(disk), None).await.unwrap();

        assert!(matches!(
            delete_volume(&pool, disk).await,
            Err(DeleteVolumeError::InUse)
        ));

        start_move(&pool, carol, None, None).await.unwrap();
        delete_volume(&pool, disk).await.unwrap();
        assert!(matches!(
            delete_volume(&pool, disk).await,
            Err(DeleteVolumeError::NotFound)
        ));
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
        start_move(&pool, alice, Some(disk), None).await.unwrap();
        assert!(set_folder(&pool, alice, "alice", "carol").await.unwrap());

        // 但 carol 不能再搬到 disk2：那裡已經有叫 carol 的資料夾
        assert!(matches!(
            start_move(&pool, carol, Some(disk), Some(1)).await,
            Err(StartMoveError::FolderTaken)
        ));
    }
}
