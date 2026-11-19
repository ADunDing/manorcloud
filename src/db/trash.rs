//! 垃圾桶的紀錄。被刪除的項目本身搬到 `{硬碟}/.manorcloud/trash/{id}`，
//! 這裡記錄它原本在哪裡，還原時放回去。
//!
//! 每顆硬碟上有兩種垃圾桶：
//! - 一般使用者的：`owner_id` 是他，路徑相對於他的 My files（users/{名稱}）
//! - 硬碟最上層的（管理員共用）：`owner_id` 是 NULL，路徑相對於硬碟的最上層
//!
//! 順序與檔案的關係（垃圾回收依賴這些不變式）：
//! - 放進垃圾桶：先寫紀錄，再搬檔案。中途失敗只會留下「有紀錄、沒檔案」，無害
//! - 永久刪除：先刪檔案，再刪紀錄。所以「有檔案、沒紀錄」只會在使用者被刪除時出現

use sqlx::{FromRow, SqlitePool};

/// 哪一個垃圾桶
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// None：資料目錄
    pub volume_id: Option<i64>,
    /// None：硬碟的最上層
    pub owner_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct TrashItem {
    pub id: i64,
    pub volume_id: Option<i64>,
    pub owner_id: Option<i64>,
    /// 原本的位置，相對於那個空間
    pub path: String,
    pub kind: String,
    pub size: i64,
    pub trashed_at: i64,
}

macro_rules! columns {
    () => {
        "id, volume_id, owner_id, path, kind, size, trashed_at"
    };
}

/// 新增一筆紀錄，回傳編號（也是垃圾桶中的檔名）
pub async fn add(
    pool: &SqlitePool,
    place: Place,
    path: &str,
    is_dir: bool,
    size: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO trash (volume_id, owner_id, path, kind, size) \
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(place.volume_id)
    .bind(place.owner_id)
    .bind(path)
    .bind(if is_dir { "folder" } else { "file" })
    .bind(i64::try_from(size).unwrap_or(i64::MAX))
    .fetch_one(pool)
    .await
}

/// 一個垃圾桶的內容，最近刪除的在前
pub async fn list(pool: &SqlitePool, place: Place) -> Result<Vec<TrashItem>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM trash WHERE volume_id IS ? AND owner_id IS ? ORDER BY trashed_at DESC, id DESC"
    );
    sqlx::query_as(SQL)
        .bind(place.volume_id)
        .bind(place.owner_id)
        .fetch_all(pool)
        .await
}

/// 這個垃圾桶中的一個項目；不存在或在別的垃圾桶時回傳 None
pub async fn find(
    pool: &SqlitePool,
    place: Place,
    id: i64,
) -> Result<Option<TrashItem>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM trash WHERE id = ? AND volume_id IS ? AND owner_id IS ?"
    );
    sqlx::query_as(SQL)
        .bind(id)
        .bind(place.volume_id)
        .bind(place.owner_id)
        .fetch_optional(pool)
        .await
}

/// 刪除一筆紀錄。回傳是否真的刪除了
pub async fn remove(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM trash WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// 某個使用者的所有項目（換儲存位置時一起搬）
pub async fn of_owner(pool: &SqlitePool, owner_id: i64) -> Result<Vec<TrashItem>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM trash WHERE owner_id = ? ORDER BY id"
    );
    sqlx::query_as(SQL).bind(owner_id).fetch_all(pool).await
}

/// 某顆硬碟上的所有項目（垃圾回收用來判斷垃圾桶中的檔案是不是孤兒）
pub async fn on_volume(
    pool: &SqlitePool,
    volume_id: Option<i64>,
) -> Result<Vec<TrashItem>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM trash WHERE volume_id IS ? ORDER BY id"
    );
    sqlx::query_as(SQL).bind(volume_id).fetch_all(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    #[sqlx::test]
    async fn each_place_has_its_own_trash(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let mine = Place {
            volume_id: None,
            owner_id: Some(carol),
        };
        let shared = Place {
            volume_id: None,
            owner_id: None,
        };

        let a = add(&pool, mine, "相片/a.jpg", false, 10).await.unwrap();
        let b = add(&pool, shared, "docs", true, 0).await.unwrap();

        assert_eq!(list(&pool, mine).await.unwrap().len(), 1);
        assert_eq!(list(&pool, shared).await.unwrap()[0].kind, "folder");
        assert!(find(&pool, mine, b).await.unwrap().is_none());
        assert_eq!(find(&pool, mine, a).await.unwrap().unwrap().size, 10);
        assert_eq!(on_volume(&pool, None).await.unwrap().len(), 2);

        assert!(remove(&pool, a).await.unwrap());
        assert!(!remove(&pool, a).await.unwrap());
    }

    #[sqlx::test]
    async fn ids_are_never_reused(pool: SqlitePool) {
        let place = Place {
            volume_id: None,
            owner_id: None,
        };
        let first = add(&pool, place, "a", false, 0).await.unwrap();
        remove(&pool, first).await.unwrap();

        // 垃圾桶中可能還留著同名的舊檔案：編號不能重複使用
        assert!(add(&pool, place, "b", false, 0).await.unwrap() > first);
    }
}
