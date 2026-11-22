//! 分享連結的紀錄。網頁與 API 在 web/shares.rs。

use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct Share {
    pub id: i64,
    pub token: String,
    pub owner_id: i64,
    /// 與網址參數 space= 相同
    pub space: String,
    pub path: String,
    /// file 或 folder
    pub kind: String,
    pub password_hash: Option<String>,
    /// 到期時間（Unix 秒）；None 不會到期
    pub expires_at: Option<i64>,
    pub downloads: i64,
    pub created_at: i64,
    /// 只能下載一次：第一次下載開始時刪除這個分享
    pub one_time: bool,
}

macro_rules! columns {
    () => {
        "id, token, owner_id, space, path, kind, password_hash, expires_at, downloads, created_at, \
         one_time"
    };
}

pub struct NewShare<'a> {
    pub token: &'a str,
    pub owner_id: i64,
    pub space: &'a str,
    pub path: &'a str,
    pub kind: &'a str,
    pub password_hash: Option<&'a str>,
    pub expires_at: Option<i64>,
    pub one_time: bool,
}

pub async fn create(pool: &SqlitePool, new: &NewShare<'_>) -> Result<Share, sqlx::Error> {
    const SQL: &str = concat!(
        "INSERT INTO shares (token, owner_id, space, path, kind, password_hash, expires_at, \
                             one_time) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
        columns!()
    );
    sqlx::query_as(SQL)
        .bind(new.token)
        .bind(new.owner_id)
        .bind(new.space)
        .bind(new.path)
        .bind(new.kind)
        .bind(new.password_hash)
        .bind(new.expires_at)
        .bind(new.one_time)
        .fetch_one(pool)
        .await
}

pub async fn find_by_token(pool: &SqlitePool, token: &str) -> Result<Option<Share>, sqlx::Error> {
    const SQL: &str = concat!("SELECT ", columns!(), " FROM shares WHERE token = ?");
    sqlx::query_as(SQL).bind(token).fetch_optional(pool).await
}

/// 某個人的分享，最新的在前
pub async fn list_for(pool: &SqlitePool, owner_id: i64) -> Result<Vec<Share>, sqlx::Error> {
    const SQL: &str = concat!(
        "SELECT ",
        columns!(),
        " FROM shares WHERE owner_id = ? ORDER BY id DESC"
    );
    sqlx::query_as(SQL).bind(owner_id).fetch_all(pool).await
}

/// 停止分享。回傳 false：沒有這個分享，或不是自己的
pub async fn delete(pool: &SqlitePool, id: i64, owner_id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM shares WHERE id = ? AND owner_id = ?")
        .bind(id)
        .bind(owner_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// 用掉只能下載一次的分享：刪除它。回傳 false：已經被別的請求用掉了
pub async fn consume(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM shares WHERE id = ? AND one_time = 1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// 下載次數加一
pub async fn count_download(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE shares SET downloads = downloads + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 刪除過期超過一段時間的分享（垃圾回收）
pub async fn delete_expired(pool: &SqlitePool, before: i64) -> Result<u64, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM shares WHERE expires_at IS NOT NULL AND expires_at < ?")
            .bind(before)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
