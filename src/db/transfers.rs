//! 跨硬碟複製與移動的紀錄。工作本身在 transfer.rs。
//!
//! 只有狀態是 running 的紀錄可以被推進：取消（改成 cancelled）之後，
//! 還在跑的背景工作每次更新都會失敗，因此會停下來。

use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct Transfer {
    pub id: i64,
    pub owner_id: i64,
    /// copy 或 move
    pub kind: String,
    pub from_space: String,
    pub from_dir: String,
    /// 選取的名稱，JSON 陣列
    pub names: String,
    pub to_space: String,
    pub to_dir: String,
    pub done_items: i64,
    pub placed: bool,
    pub total_bytes: i64,
    pub done_bytes: i64,
    pub state: String,
    pub error: Option<String>,
    /// 略過的項目，JSON 陣列
    pub skipped: String,
}

macro_rules! columns {
    () => {
        "id, owner_id, kind, from_space, from_dir, names, to_space, to_dir, done_items, placed, \
         total_bytes, done_bytes, state, error, skipped"
    };
}

pub struct NewTransfer<'a> {
    pub owner_id: i64,
    pub kind: &'a str,
    pub from_space: &'a str,
    pub from_dir: &'a str,
    pub names: &'a [String],
    pub to_space: &'a str,
    pub to_dir: &'a str,
    pub total_bytes: i64,
}

pub async fn create(pool: &SqlitePool, new: &NewTransfer<'_>) -> Result<Transfer, sqlx::Error> {
    const SQL: &str = concat!(
        "INSERT INTO transfers (owner_id, kind, from_space, from_dir, names, to_space, to_dir, \
                                total_bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
        columns!()
    );
    sqlx::query_as(SQL)
        .bind(new.owner_id)
        .bind(new.kind)
        .bind(new.from_space)
        .bind(new.from_dir)
        .bind(serde_json::to_string(new.names).unwrap_or_else(|_| "[]".to_string()))
        .bind(new.to_space)
        .bind(new.to_dir)
        .bind(new.total_bytes)
        .fetch_one(pool)
        .await
}

pub async fn find(pool: &SqlitePool, id: i64) -> Result<Option<Transfer>, sqlx::Error> {
    const SQL: &str = concat!("SELECT ", columns!(), " FROM transfers WHERE id = ?");
    sqlx::query_as(SQL).bind(id).fetch_optional(pool).await
}

/// 某個人的紀錄，最新的在前
pub async fn list_for(pool: &SqlitePool, owner_id: i64) -> Result<Vec<Transfer>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM transfers WHERE owner_id = ? ORDER BY id DESC"
    );
    sqlx::query_as(SQL).bind(owner_id).fetch_all(pool).await
}

/// 還在進行的紀錄（啟動時接著做）
pub async fn running(pool: &SqlitePool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM transfers WHERE state = 'running' ORDER BY id")
        .fetch_all(pool)
        .await
}

/// 更新已複製的位元組數。回傳 false：已經不是 running（被取消），工作應該停下來
pub async fn set_progress(
    pool: &SqlitePool,
    id: i64,
    done_bytes: i64,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE transfers SET done_bytes = ?, updated_at = unixepoch() \
         WHERE id = ? AND state = 'running'",
    )
    .bind(done_bytes)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 移動：目前的項目已經放到目的地，接下來刪除來源
pub async fn set_placed(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query("UPDATE transfers SET placed = 1 WHERE id = ? AND state = 'running'")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(updated == 1)
}

/// 一個項目完成：推進到下一個
pub async fn finish_item(
    pool: &SqlitePool,
    id: i64,
    done_bytes: i64,
    skipped: &[String],
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE transfers SET done_items = done_items + 1, placed = 0, done_bytes = ?, \
                skipped = ?, updated_at = unixepoch() \
         WHERE id = ? AND state = 'running'",
    )
    .bind(done_bytes)
    .bind(serde_json::to_string(skipped).unwrap_or_else(|_| "[]".to_string()))
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 結束：done 或 failed。只有 running 的紀錄會被修改
pub async fn finish(
    pool: &SqlitePool,
    id: i64,
    state: &str,
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE transfers SET state = ?, error = ?, updated_at = unixepoch() \
         WHERE id = ? AND state = 'running'",
    )
    .bind(state)
    .bind(error)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 取消自己進行中的工作。已經完成的項目留在目的地
pub async fn cancel(pool: &SqlitePool, id: i64, owner_id: i64) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE transfers SET state = 'cancelled', updated_at = unixepoch() \
         WHERE id = ? AND owner_id = ? AND state = 'running'",
    )
    .bind(id)
    .bind(owner_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// 從清單中移除自己已經結束的紀錄
pub async fn dismiss(pool: &SqlitePool, id: i64, owner_id: i64) -> Result<bool, sqlx::Error> {
    let deleted =
        sqlx::query("DELETE FROM transfers WHERE id = ? AND owner_id = ? AND state <> 'running'")
            .bind(id)
            .bind(owner_id)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(deleted == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    #[sqlx::test]
    async fn cancelled_transfers_stop_advancing(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let names = vec!["a".to_string(), "b".to_string()];
        let t = create(
            &pool,
            &NewTransfer {
                owner_id: carol,
                kind: "move",
                from_space: "default",
                from_dir: "",
                names: &names,
                to_space: "1",
                to_dir: "",
                total_bytes: 10,
            },
        )
        .await
        .unwrap();

        assert!(finish_item(&pool, t.id, 5, &[]).await.unwrap());
        assert_eq!(running(&pool).await.unwrap(), [t.id]);
        assert!(!dismiss(&pool, t.id, carol).await.unwrap());
        assert!(cancel(&pool, t.id, carol).await.unwrap());

        assert!(!set_progress(&pool, t.id, 6).await.unwrap());
        assert!(!finish(&pool, t.id, "done", None).await.unwrap());
        let t = find(&pool, t.id).await.unwrap().unwrap();
        assert_eq!((t.state.as_str(), t.done_items), ("cancelled", 1));
        assert!(dismiss(&pool, t.id, carol).await.unwrap());
    }
}
