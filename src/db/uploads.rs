//! 分段上傳的進度紀錄。
//!
//! 使用執行期查詢（query_as + FromRow）而不是 query! 巨集：巨集在編譯時就要連到
//! 已經有 uploads 表的資料庫，新表剛加入時會陷入「無法編譯 → 無法執行 migration」的循環。
//! #[sqlx::test] 會套用所有 migration，測試照樣能驗證每一條 SQL。

use std::fmt;

use sqlx::{FromRow, SqlitePool};

/// 所有查詢回傳的欄位，與 Upload 的欄位一一對應。
///
/// 寫成巨集而不是 `const`：sqlx 只接受編譯時就固定的 SQL 字串（`&'static str`），
/// 執行時用 format! 組出來的 String 會被拒絕（防止把使用者輸入拼進 SQL）。
/// `concat!` 在編譯時把字串接起來，結果仍是 `&'static str`，欄位清單也只寫一次。
macro_rules! columns {
    () => {
        "id, owner_id, parent_id, name, size, received, file_modified, created_at, updated_at"
    };
}

/// uploads 表的一列
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Upload {
    pub id: String,
    pub owner_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub size: i64,
    pub received: i64,
    pub file_modified: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 宣告一個新的上傳所需的資料
pub struct NewUpload<'a> {
    pub id: &'a str,
    pub owner_id: i64,
    pub parent_id: Option<i64>,
    /// 必須已經通過 validate_file_name
    pub name: &'a str,
    pub size: i64,
    pub file_modified: Option<i64>,
}

#[derive(Debug)]
pub enum CreateUploadError {
    /// 這個使用者未完成的上傳已達上限
    TooManyPending {
        limit: u32,
    },
    /// 父資料夾不存在、或屬於其他使用者（由複合外鍵擋下）。
    /// HTTP 層與 files 相同，一律回傳 404，不透露資料夾是否存在
    ParentNotFound,
    Db(sqlx::Error),
}

impl fmt::Display for CreateUploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateUploadError::TooManyPending { limit } => {
                write!(f, "too many unfinished uploads (limit {limit})")
            }
            CreateUploadError::ParentNotFound => f.write_str("parent folder not found"),
            CreateUploadError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for CreateUploadError {}

/// 建立一筆上傳紀錄；這個使用者未完成的上傳已達 `max_pending` 時拒絕。
///
/// 數量檢查與寫入在同一條敘述中：先查再寫的話，兩個同時的請求都會看到「還沒滿」而都寫入。
/// SQLite 同一時間只有一個寫入者，這條敘述是原子的，超過上限時一筆都不會寫入。
pub async fn create_upload(
    pool: &SqlitePool,
    new: &NewUpload<'_>,
    max_pending: u32,
) -> Result<Upload, CreateUploadError> {
    const SQL: &str = concat!(
        "INSERT INTO uploads (id, owner_id, parent_id, name, size, file_modified) ",
        "SELECT ?, ?, ?, ?, ?, ? ",
        "WHERE (SELECT COUNT(*) FROM uploads WHERE owner_id = ?) < ? ",
        "RETURNING ",
        columns!()
    );

    let inserted = sqlx::query_as::<_, Upload>(SQL)
        .bind(new.id)
        .bind(new.owner_id)
        .bind(new.parent_id)
        .bind(new.name)
        .bind(new.size)
        .bind(new.file_modified)
        .bind(new.owner_id)
        .bind(max_pending)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|db| db.is_foreign_key_violation())
            {
                CreateUploadError::ParentNotFound
            } else {
                CreateUploadError::Db(e)
            }
        })?;

    // 沒有寫入任何一列：WHERE 的數量條件不成立
    inserted.ok_or(CreateUploadError::TooManyPending { limit: max_pending })
}

/// 取得自己的一筆上傳；不存在或屬於別人時都是 None（HTTP 層回 404，不透露是否存在）
pub async fn find_upload(
    pool: &SqlitePool,
    owner_id: i64,
    id: &str,
) -> Result<Option<Upload>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM uploads WHERE id = ? AND owner_id = ?"
    );

    sqlx::query_as::<_, Upload>(SQL)
        .bind(id)
        .bind(owner_id)
        .fetch_optional(pool)
        .await
}

/// 列出自己所有未完成的上傳，最早開始的在前
pub async fn list_uploads(pool: &SqlitePool, owner_id: i64) -> Result<Vec<Upload>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM uploads WHERE owner_id = ? ORDER BY created_at, id"
    );

    sqlx::query_as::<_, Upload>(SQL)
        .bind(owner_id)
        .fetch_all(pool)
        .await
}

/// 把已確認收到的位元組數從 `from` 推進到 `to`，並更新「最後有進度」的時間。
///
/// 只有在目前的值仍是 `from` 時才更新：兩個請求同時對同一個上傳寫入時，只有一個會成功，
/// 另一個回傳 false，知道自己慢了一步。呼叫前，資料必須已經寫進暫存檔並 fsync。
pub async fn advance_received(
    pool: &SqlitePool,
    owner_id: i64,
    id: &str,
    from: i64,
    to: i64,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE uploads SET received = ?, updated_at = unixepoch()
         WHERE id = ? AND owner_id = ? AND received = ?",
    )
    .bind(to)
    .bind(id)
    .bind(owner_id)
    .bind(from)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() == 1)
}

/// 刪除自己的一筆上傳紀錄（完成或取消時）。回傳是否真的刪除了一筆
pub async fn delete_upload(
    pool: &SqlitePool,
    owner_id: i64,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM uploads WHERE id = ? AND owner_id = ?")
        .bind(id)
        .bind(owner_id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() == 1)
}

/// 所有使用者未完成的上傳，總共還差多少位元組。
/// 宣告新的上傳時，這些空間視為已被預留
pub async fn pending_bytes(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(SUM(size - received), 0) FROM uploads")
        .fetch_one(pool)
        .await
}

/// 刪除超過 `cutoff`（Unix 秒數）沒有進度的上傳紀錄，回傳被刪除的編號。
///
/// 一條敘述同時找出並刪除：不會有「查出來之後、刪除之前剛好有了新進度」的空窗。
/// 垃圾回收接著刪除這些編號的暫存檔
pub async fn delete_stale(pool: &SqlitePool, cutoff: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM uploads WHERE updated_at < ? RETURNING id")
        .bind(cutoff)
        .fetch_all(pool)
        .await
}

/// 所有上傳的編號（垃圾回收用來判斷暫存檔是不是孤兒）
pub async fn all_ids(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM uploads")
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{files as files_db, user};

    fn id(n: u32) -> String {
        format!("{n:032x}")
    }

    fn new_upload(id: &str, owner_id: i64, parent_id: Option<i64>) -> NewUpload<'_> {
        NewUpload {
            id,
            owner_id,
            parent_id,
            name: "movie.mp4",
            size: 100,
            file_modified: Some(1_700_000_000_000),
        }
    }

    async fn make_user(pool: &SqlitePool, name: &str) -> i64 {
        user::create_user(pool, name, "x").await.unwrap()
    }

    // ==================== 建立 ====================

    #[sqlx::test]
    async fn creates_and_finds_an_upload(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;

        let created = create_upload(&pool, &new_upload(&id(1), carol, None), 5)
            .await
            .unwrap();

        assert_eq!(created.received, 0);
        assert_eq!(created.size, 100);
        assert_eq!(created.file_modified, Some(1_700_000_000_000));
        assert_eq!(
            find_upload(&pool, carol, &id(1)).await.unwrap(),
            Some(created)
        );
    }

    #[sqlx::test]
    async fn pending_limit_is_per_user(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let dave = make_user(&pool, "dave").await;

        for n in 0..3 {
            create_upload(&pool, &new_upload(&id(n), carol, None), 3)
                .await
                .unwrap();
        }

        // carol 已達上限
        let result = create_upload(&pool, &new_upload(&id(10), carol, None), 3).await;
        assert!(matches!(
            result,
            Err(CreateUploadError::TooManyPending { limit: 3 })
        ));
        // 沒有寫入任何一列
        assert_eq!(list_uploads(&pool, carol).await.unwrap().len(), 3);

        // dave 不受 carol 的數量影響
        create_upload(&pool, &new_upload(&id(20), dave, None), 3)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn finishing_one_frees_a_slot(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        create_upload(&pool, &new_upload(&id(1), carol, None), 1)
            .await
            .unwrap();

        assert!(delete_upload(&pool, carol, &id(1)).await.unwrap());

        create_upload(&pool, &new_upload(&id(2), carol, None), 1)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn other_users_folder_is_rejected(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let dave = make_user(&pool, "dave").await;
        let daves_folder = files_db::create_folder(&pool, dave, None, "dave's")
            .await
            .unwrap()
            .id;

        let result = create_upload(&pool, &new_upload(&id(1), carol, Some(daves_folder)), 5).await;

        assert!(matches!(result, Err(CreateUploadError::ParentNotFound)));
    }

    #[sqlx::test]
    async fn invalid_ids_are_rejected_by_the_database(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;

        // 暫存檔的檔名來自 id：任何其他格式都必須被資料庫擋下
        for bad in [
            "../../etc/passwd".to_string(),
            // 必須含有字母，轉大寫才會不同（id(1) 全是數字，大寫後一模一樣）
            id(0xabc).to_uppercase(),
            format!("{}g", &id(1)[..31]),
            id(1)[..31].to_string(),
        ] {
            let result = create_upload(&pool, &new_upload(&bad, carol, None), 5).await;
            assert!(matches!(result, Err(CreateUploadError::Db(_))), "{bad}");
        }
    }

    // ==================== 查詢 ====================

    #[sqlx::test]
    async fn other_users_upload_is_not_found(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let dave = make_user(&pool, "dave").await;
        create_upload(&pool, &new_upload(&id(1), carol, None), 5)
            .await
            .unwrap();

        assert_eq!(find_upload(&pool, dave, &id(1)).await.unwrap(), None);
        assert!(!delete_upload(&pool, dave, &id(1)).await.unwrap());
        assert!(!advance_received(&pool, dave, &id(1), 0, 10).await.unwrap());
    }

    // ==================== 進度 ====================

    #[sqlx::test]
    async fn advance_requires_the_expected_starting_point(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        create_upload(&pool, &new_upload(&id(1), carol, None), 5)
            .await
            .unwrap();

        assert!(advance_received(&pool, carol, &id(1), 0, 40).await.unwrap());
        // 另一個請求以為還在 0：慢了一步，不更新
        assert!(!advance_received(&pool, carol, &id(1), 0, 40).await.unwrap());
        assert!(
            advance_received(&pool, carol, &id(1), 40, 100)
                .await
                .unwrap()
        );

        let upload = find_upload(&pool, carol, &id(1)).await.unwrap().unwrap();
        assert_eq!(upload.received, 100);
    }

    #[sqlx::test]
    async fn received_cannot_exceed_size(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        create_upload(&pool, &new_upload(&id(1), carol, None), 5)
            .await
            .unwrap();

        // 資料庫的 CHECK 是最後一道防線：HTTP 層應該先擋下，這裡確認萬一沒擋也寫不進去
        assert!(
            advance_received(&pool, carol, &id(1), 0, 101)
                .await
                .is_err()
        );
    }

    // ==================== 連帶刪除 ====================

    #[sqlx::test]
    async fn deleting_the_folder_removes_its_uploads(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let folder = files_db::create_folder(&pool, carol, None, "videos")
            .await
            .unwrap()
            .id;
        create_upload(&pool, &new_upload(&id(1), carol, Some(folder)), 5)
            .await
            .unwrap();

        sqlx::query("DELETE FROM files WHERE id = ?")
            .bind(folder)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(find_upload(&pool, carol, &id(1)).await.unwrap(), None);
    }

    // ==================== 空間與同名 ====================

    #[sqlx::test]
    async fn pending_bytes_counts_what_is_still_missing(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        assert_eq!(pending_bytes(&pool).await.unwrap(), 0);

        create_upload(&pool, &new_upload(&id(1), carol, None), 5)
            .await
            .unwrap();
        create_upload(&pool, &new_upload(&id(2), carol, None), 5)
            .await
            .unwrap();
        advance_received(&pool, carol, &id(1), 0, 30).await.unwrap();

        // 兩個各 100，其中一個已收到 30：還差 70 + 100
        assert_eq!(pending_bytes(&pool).await.unwrap(), 170);
    }
}
