use sqlx::SqlitePool;

/// 透過有效 session 確認身分的使用者。
///
/// 停用的使用者在查詢中就被過濾掉，所以拿到 SessionInfo 代表「已登入且未停用」。
#[derive(Debug)]
pub struct SessionInfo {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    /// 密碼是臨時的，必須先修改。沒有密碼的使用者（將來只用 SSO）為 false
    pub must_change_password: bool,
}

/// 新增一筆 session
pub async fn create_session(
    pool: &SqlitePool,
    user_id: i64,
    token_hash: &[u8],
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?, ?, ?)",
        token_hash,
        user_id,
        expires_at
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// 用 token 雜湊查詢對應的使用者。
/// session 不存在、已過期，或使用者已被停用，都回傳 None：
/// 停用帳號的所有既有 session 立即全部失效，不必等到過期。
///
/// 用 LEFT JOIN 讀取密碼表：將來只用 SSO 的使用者沒有密碼紀錄，此時「必須修改」視為 false。
pub async fn find_user_by_token_hash(
    pool: &SqlitePool,
    token_hash: &[u8],
) -> Result<Option<SessionInfo>, sqlx::Error> {
    sqlx::query_as!(
        SessionInfo,
        r#"
        SELECT
            users.id       AS "user_id!",
            users.username AS "username!",
            users.role     AS "role!",
            COALESCE(password_credentials.must_change_password, 0)
                           AS "must_change_password!: bool"
        FROM sessions
        JOIN users ON users.id = sessions.user_id
        LEFT JOIN password_credentials ON password_credentials.user_id = users.id
        WHERE sessions.token_hash = ?
          AND sessions.expires_at > unixepoch()
          AND users.disabled = 0
        "#,
        token_hash
    )
    .fetch_optional(pool)
    .await
}

/// 刪除一筆 session（登出）
pub async fn delete_session(pool: &SqlitePool, token_hash: &[u8]) -> Result<(), sqlx::Error> {
    sqlx::query!("DELETE FROM sessions WHERE token_hash = ?", token_hash)
        .execute(pool)
        .await?;

    Ok(())
}

/// 刪除所有已過期的 session，回傳刪除的筆數。
///
/// 「現在」由呼叫端傳入，與查詢時的 `expires_at > unixepoch()` 相對應：
/// `expires_at <= now` 的 session 已經查不到，留著只佔空間。
/// sessions 表的 expires_at 索引就是為了這個查詢建立的。
pub async fn delete_expired(pool: &SqlitePool, now: i64) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!("DELETE FROM sessions WHERE expires_at <= ?", now)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// 建立測試用使用者（sessions 有外鍵，必須先有使用者）
    async fn setup_user(pool: &SqlitePool) -> i64 {
        user::create_user(pool, "alice", "fake_hash").await.unwrap()
    }

    #[sqlx::test]
    async fn create_and_find(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let hash = vec![1u8; 32];

        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();

        let found = find_user_by_token_hash(&pool, &hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.user_id, user_id);
        assert_eq!(found.username, "alice");
        assert_eq!(found.role, "member");
        assert!(!found.must_change_password);
    }

    #[sqlx::test]
    async fn must_change_flag_is_reported(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        sqlx::query("UPDATE password_credentials SET must_change_password = 1 WHERE user_id = ?")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        let hash = vec![8u8; 32];
        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();

        let found = find_user_by_token_hash(&pool, &hash)
            .await
            .unwrap()
            .unwrap();

        assert!(found.must_change_password);
    }

    #[sqlx::test]
    async fn user_without_password_is_found(pool: SqlitePool) {
        // 模擬將來只用 SSO 的使用者：LEFT JOIN 讓沒有密碼紀錄的使用者仍然查得到
        let user_id = setup_user(&pool).await;
        sqlx::query("DELETE FROM password_credentials WHERE user_id = ?")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        let hash = vec![9u8; 32];
        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();

        let found = find_user_by_token_hash(&pool, &hash)
            .await
            .unwrap()
            .unwrap();

        assert!(!found.must_change_password);
    }

    #[sqlx::test]
    async fn expired_session_not_found(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let hash = vec![2u8; 32];

        // 過期時間設在一小時前
        create_session(&pool, user_id, &hash, now() - 3600)
            .await
            .unwrap();

        let found = find_user_by_token_hash(&pool, &hash).await.unwrap();
        assert!(found.is_none());
    }

    #[sqlx::test]
    async fn unknown_hash_not_found(pool: SqlitePool) {
        let found = find_user_by_token_hash(&pool, &[9u8; 32]).await.unwrap();
        assert!(found.is_none());
    }

    #[sqlx::test]
    async fn delete_then_not_found(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let hash = vec![3u8; 32];

        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();
        delete_session(&pool, &hash).await.unwrap();

        let found = find_user_by_token_hash(&pool, &hash).await.unwrap();
        assert!(found.is_none());
    }

    #[sqlx::test]
    async fn deleting_user_cascades_sessions(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let hash = vec![4u8; 32];

        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();
        sqlx::query!("DELETE FROM users WHERE id = ?", user_id)
            .execute(&pool)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    async fn delete_expired_keeps_active_sessions(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let expired = vec![5u8; 32];
        let active = vec![6u8; 32];
        create_session(&pool, user_id, &expired, now() - 10)
            .await
            .unwrap();
        create_session(&pool, user_id, &active, now() + 3600)
            .await
            .unwrap();

        let deleted = delete_expired(&pool, now()).await.unwrap();

        assert_eq!(deleted, 1);
        assert!(
            find_user_by_token_hash(&pool, &active)
                .await
                .unwrap()
                .is_some()
        );
        let remaining: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[sqlx::test]
    async fn disabled_user_session_not_found(pool: SqlitePool) {
        let user_id = setup_user(&pool).await;
        let hash = vec![7u8; 32];
        create_session(&pool, user_id, &hash, now() + 3600)
            .await
            .unwrap();

        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        // session 本身還在、也還沒過期，但使用者已被停用：立即失效
        assert!(
            find_user_by_token_hash(&pool, &hash)
                .await
                .unwrap()
                .is_none()
        );
    }
}
