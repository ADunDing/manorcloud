//! 整個伺服器的設定，在管理頁面修改（server_settings，只有一列）。

use sqlx::SqlitePool;

/// 管理頁面設定的上傳上限；None：使用預設值
pub async fn max_upload_bytes(pool: &SqlitePool) -> Result<Option<u64>, sqlx::Error> {
    let value: Option<i64> =
        sqlx::query_scalar("SELECT max_upload_bytes FROM server_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(value.and_then(|bytes| u64::try_from(bytes).ok()))
}

pub async fn set_max_upload_bytes(
    pool: &SqlitePool,
    bytes: Option<u64>,
) -> Result<(), sqlx::Error> {
    let bytes = bytes.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
    sqlx::query("UPDATE server_settings SET max_upload_bytes = ? WHERE id = 1")
        .bind(bytes)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 管理頁面設定的「每人未完成的上傳數」；None：使用預設值
pub async fn max_pending_uploads(pool: &SqlitePool) -> Result<Option<u32>, sqlx::Error> {
    let value: Option<i64> =
        sqlx::query_scalar("SELECT max_pending_uploads FROM server_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(value.and_then(|n| u32::try_from(n).ok()))
}

pub async fn set_max_pending_uploads(
    pool: &SqlitePool,
    limit: Option<u32>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE server_settings SET max_pending_uploads = ? WHERE id = 1")
        .bind(limit.map(i64::from))
        .execute(pool)
        .await
        .map(|_| ())
}

/// 反向代理的位址或網段，以逗號分隔；空字串：沒有
pub async fn trusted_proxies(pool: &SqlitePool) -> Result<String, sqlx::Error> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT trusted_proxies FROM server_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(value.unwrap_or_default())
}

pub async fn set_trusted_proxies(pool: &SqlitePool, proxies: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE server_settings SET trusted_proxies = ? WHERE id = 1")
        .bind(proxies)
        .execute(pool)
        .await
        .map(|_| ())
}

/// 是否開放 WebDAV
pub async fn webdav_enabled(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let value: Option<bool> =
        sqlx::query_scalar("SELECT webdav_enabled FROM server_settings WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(value.unwrap_or(false))
}

pub async fn set_webdav_enabled(pool: &SqlitePool, enabled: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE server_settings SET webdav_enabled = ? WHERE id = 1")
        .bind(enabled)
        .execute(pool)
        .await
        .map(|_| ())
}
