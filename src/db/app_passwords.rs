//! 應用程式密碼：WebDAV 用戶端登入用，每台裝置一組（見 migration add_app_passwords）。

use sqlx::{FromRow, SqlitePool};

/// 每個人最多幾組
pub const MAX_PER_USER: i64 = 50;

#[derive(Debug, Clone, FromRow)]
pub struct AppPassword {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// 用應用程式密碼登入的人
#[derive(Debug, Clone, FromRow)]
pub struct DavLogin {
    pub password_id: i64,
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub last_used_at: Option<i64>,
    /// 臨時密碼還沒改，或管理員要求 2FA 而還沒設定：和網頁一樣，要先完成才能使用
    pub blocked: bool,
}

pub async fn list(pool: &SqlitePool, user_id: i64) -> Result<Vec<AppPassword>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, created_at, last_used_at FROM app_passwords
         WHERE user_id = ? ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn count(pool: &SqlitePool, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM app_passwords WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
}

pub async fn create(
    pool: &SqlitePool,
    user_id: i64,
    name: &str,
    token_hash: &[u8],
) -> Result<AppPassword, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO app_passwords (user_id, name, token_hash) VALUES (?, ?, ?)
         RETURNING id, name, created_at, last_used_at",
    )
    .bind(user_id)
    .bind(name)
    .bind(token_hash)
    .fetch_one(pool)
    .await
}

/// 撤銷自己的一組；回傳是否真的刪除了（別人的或不存在的都是 false）
pub async fn delete(pool: &SqlitePool, user_id: i64, id: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM app_passwords WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// 帳號與應用程式密碼相符、而且帳號沒有停用時，回傳登入的人
pub async fn find_login(
    pool: &SqlitePool,
    username: &str,
    token_hash: &[u8],
) -> Result<Option<DavLogin>, sqlx::Error> {
    sqlx::query_as(
        r#"
        SELECT
            app_passwords.id AS password_id,
            users.id         AS user_id,
            users.username   AS username,
            users.role       AS role,
            app_passwords.last_used_at AS last_used_at,
            (
                COALESCE(password_credentials.must_change_password, 0) = 1
                OR (
                    password_credentials.user_id IS NOT NULL
                    AND NOT EXISTS (
                        SELECT 1 FROM totp_credentials
                        WHERE totp_credentials.user_id = users.id AND totp_credentials.enabled = 1
                    )
                    AND (SELECT require_two_factor FROM security_settings WHERE id = 1) = 1
                )
            ) AS blocked
        FROM app_passwords
        JOIN users ON users.id = app_passwords.user_id
        LEFT JOIN password_credentials ON password_credentials.user_id = users.id
        WHERE app_passwords.token_hash = ?
          AND users.username = ?
          AND users.disabled = 0
        "#,
    )
    .bind(token_hash)
    .bind(username)
    .fetch_optional(pool)
    .await
}

/// 記錄使用時間間隔多久寫一次：每個 WebDAV 請求都寫資料庫會白白磨損硬碟
pub const TOUCH_EVERY_SECONDS: i64 = 300;

/// 記錄使用時間
pub async fn touch(pool: &SqlitePool, id: i64, now: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE app_passwords SET last_used_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await
        .map(|_| ())
}
