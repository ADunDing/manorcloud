//! 兩步驟驗證的資料：TOTP 密鑰、備用碼、等待輸入驗證碼的登入，以及「是否強制」的設定。
//!
//! 全部使用執行期的查詢（不是 query! 巨集）：新增這個模組不需要重新產生 .sqlx。

use std::collections::HashSet;

use sqlx::SqlitePool;

/// 一個等待輸入驗證碼的登入，最多可以猜錯幾次（之後要從帳號密碼重新開始）。
/// 另外還有登入次數限制（每個 IP、每個帳號）在外層保護
pub const MAX_CHALLENGE_FAILURES: i64 = 5;

// ==================== 狀態 ====================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    /// 還沒用過的備用碼
    pub recovery_codes_left: i64,
}

pub async fn status(pool: &SqlitePool, user_id: i64) -> Result<Status, sqlx::Error> {
    let (enabled, recovery_codes_left): (bool, i64) = sqlx::query_as(
        "SELECT
             EXISTS (SELECT 1 FROM totp_credentials WHERE user_id = ?1 AND enabled = 1),
             (SELECT COUNT(*) FROM recovery_codes WHERE user_id = ?1 AND used_at IS NULL)",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(Status {
        enabled,
        recovery_codes_left,
    })
}

/// 已經開啟 2FA 的使用者（管理員的使用者列表用）
pub async fn enabled_user_ids(pool: &SqlitePool) -> Result<HashSet<i64>, sqlx::Error> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM totp_credentials WHERE enabled = 1")
            .fetch_all(pool)
            .await?;
    Ok(ids.into_iter().collect())
}

// ==================== 設定與啟用 ====================

/// 一個帳號的 TOTP
#[derive(Clone, sqlx::FromRow)]
pub struct Totp {
    pub secret: Vec<u8>,
    pub enabled: bool,
    pub last_used_step: i64,
}

/// 刻意不印出密鑰
impl std::fmt::Debug for Totp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Totp")
            .field("secret", &"<redacted>")
            .field("enabled", &self.enabled)
            .field("last_used_step", &self.last_used_step)
            .finish()
    }
}

pub async fn find_totp(pool: &SqlitePool, user_id: i64) -> Result<Option<Totp>, sqlx::Error> {
    sqlx::query_as("SELECT secret, enabled, last_used_step FROM totp_credentials WHERE user_id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// 開始設定：存入新的密鑰（還沒啟用）。重新開始設定時換成新的密鑰。
/// 已經啟用時回傳 false，什麼都不改：要先關閉才能重新設定
pub async fn begin_setup(
    pool: &SqlitePool,
    user_id: i64,
    secret: &[u8],
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "INSERT INTO totp_credentials (user_id, secret, enabled) VALUES (?, ?, 0)
         ON CONFLICT (user_id) DO UPDATE SET
             secret = excluded.secret, last_used_step = 0, created_at = unixepoch()
         WHERE totp_credentials.enabled = 0",
    )
    .bind(user_id)
    .bind(secret)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed > 0)
}

/// 確認設定：啟用 TOTP、換上新的備用碼、登出這個帳號的所有裝置，在同一個交易中完成。
/// （呼叫端再為目前的裝置建立新的 session，與修改密碼相同的做法。）
///
/// `step` 是確認時用掉的驗證碼的時間段：它不能再被用來登入。
/// 沒有等待確認的設定（例如已經啟用，或從來沒有開始）時回傳 false
pub async fn enable(
    pool: &SqlitePool,
    user_id: i64,
    step: i64,
    recovery_hashes: &[Vec<u8>],
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE totp_credentials SET enabled = 1, last_used_step = ?
         WHERE user_id = ? AND enabled = 0",
    )
    .bind(step)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    replace_codes(&mut tx, user_id, recovery_hashes).await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// 換上一組新的備用碼（舊的全部作廢）
pub async fn replace_recovery_codes(
    pool: &SqlitePool,
    user_id: i64,
    recovery_hashes: &[Vec<u8>],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    replace_codes(&mut tx, user_id, recovery_hashes).await?;
    tx.commit().await?;
    Ok(())
}

async fn replace_codes(
    tx: &mut sqlx::SqliteConnection,
    user_id: i64,
    recovery_hashes: &[Vec<u8>],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for hash in recovery_hashes {
        sqlx::query("INSERT INTO recovery_codes (user_id, code_hash) VALUES (?, ?)")
            .bind(user_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// 關閉 2FA：刪除密鑰與備用碼。
/// `log_out` 為 true 時一併登出這個帳號的所有裝置（管理員替別人關閉時：
/// 很可能是手機遺失，撿到手機的人不應該繼續登入著）。回傳原本是否有設定
pub async fn disable(pool: &SqlitePool, user_id: i64, log_out: bool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query("DELETE FROM totp_credentials WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    if log_out {
        sqlx::query("DELETE FROM sessions WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(deleted > 0)
}

// ==================== 登入時的驗證 ====================

/// 記下用掉的時間段。條件寫在同一條 UPDATE 裡：同時送出的兩個相同驗證碼，只有一個會成功
pub async fn use_step(pool: &SqlitePool, user_id: i64, step: i64) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE totp_credentials SET last_used_step = ?1
         WHERE user_id = ?2 AND enabled = 1 AND last_used_step < ?1",
    )
    .bind(step)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// 使用一組備用碼。沒有這組、或已經用過時回傳 false
pub async fn use_recovery_code(
    pool: &SqlitePool,
    user_id: i64,
    code_hash: &[u8],
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE recovery_codes SET used_at = unixepoch()
         WHERE user_id = ? AND code_hash = ? AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(code_hash)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

// ==================== 等待輸入驗證碼的登入 ====================

/// 記錄一個等待輸入驗證碼的登入，順便清除過期的
pub async fn create_challenge(
    pool: &SqlitePool,
    token_hash: &[u8],
    user_id: i64,
    now: i64,
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM two_factor_challenges WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;
    sqlx::query(
        "INSERT INTO two_factor_challenges (token_hash, user_id, expires_at) VALUES (?, ?, ?)",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// 這個等待中的登入屬於哪個帳號；不存在或已過期時為 None
pub async fn find_challenge(
    pool: &SqlitePool,
    token_hash: &[u8],
    now: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT user_id FROM two_factor_challenges WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(token_hash)
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// 記一次猜錯。到達上限時作廢這個登入，回傳 true
pub async fn fail_challenge(pool: &SqlitePool, token_hash: &[u8]) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE two_factor_challenges SET failures = failures + 1 WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    let deleted =
        sqlx::query("DELETE FROM two_factor_challenges WHERE token_hash = ? AND failures >= ?")
            .bind(token_hash)
            .bind(MAX_CHALLENGE_FAILURES)
            .execute(pool)
            .await?
            .rows_affected();
    Ok(deleted > 0)
}

/// 驗證通過：這個等待中的登入用掉了。回傳 false 代表它已經被另一個請求用掉
pub async fn finish_challenge(pool: &SqlitePool, token_hash: &[u8]) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM two_factor_challenges WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted > 0)
}

// ==================== 是否強制 ====================

pub async fn require_two_factor(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT require_two_factor FROM security_settings WHERE id = 1")
        .fetch_one(pool)
        .await
}

pub async fn set_require_two_factor(pool: &SqlitePool, required: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE security_settings SET require_two_factor = ? WHERE id = 1")
        .bind(required)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{session, user};

    const SECRET: &[u8] = b"12345678901234567890";

    async fn carol(pool: &SqlitePool) -> i64 {
        user::create_user(pool, "carol", "x").await.unwrap()
    }

    async fn sessions(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn hashes(n: u8) -> Vec<Vec<u8>> {
        (0..n).map(|i| vec![i; 32]).collect()
    }

    #[sqlx::test]
    async fn setup_is_pending_until_confirmed(pool: SqlitePool) {
        let id = carol(&pool).await;
        session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        assert!(begin_setup(&pool, id, SECRET).await.unwrap());
        assert!(!status(&pool, id).await.unwrap().enabled);

        assert!(enable(&pool, id, 100, &hashes(10)).await.unwrap());
        assert_eq!(
            status(&pool, id).await.unwrap(),
            Status {
                enabled: true,
                recovery_codes_left: 10
            }
        );
        // 啟用時登出所有裝置
        assert_eq!(sessions(&pool).await, 0);
        // 已經啟用：不能直接換掉密鑰，也不能再確認一次
        assert!(
            !begin_setup(&pool, id, b"another-secret-value")
                .await
                .unwrap()
        );
        assert!(!enable(&pool, id, 200, &hashes(10)).await.unwrap());
        assert_eq!(find_totp(&pool, id).await.unwrap().unwrap().secret, SECRET);
    }

    #[sqlx::test]
    async fn steps_and_recovery_codes_are_single_use(pool: SqlitePool) {
        let id = carol(&pool).await;
        begin_setup(&pool, id, SECRET).await.unwrap();
        enable(&pool, id, 100, &hashes(3)).await.unwrap();

        // 確認時用掉的時間段不能再用；之後的可以，但只能一次
        assert!(!use_step(&pool, id, 100).await.unwrap());
        assert!(use_step(&pool, id, 101).await.unwrap());
        assert!(!use_step(&pool, id, 101).await.unwrap());

        assert!(use_recovery_code(&pool, id, &[1u8; 32]).await.unwrap());
        assert!(!use_recovery_code(&pool, id, &[1u8; 32]).await.unwrap());
        assert!(!use_recovery_code(&pool, id, &[9u8; 32]).await.unwrap());
        assert_eq!(status(&pool, id).await.unwrap().recovery_codes_left, 2);

        replace_recovery_codes(&pool, id, &hashes(10))
            .await
            .unwrap();
        assert_eq!(status(&pool, id).await.unwrap().recovery_codes_left, 10);
    }

    #[sqlx::test]
    async fn disabling_removes_everything(pool: SqlitePool) {
        let id = carol(&pool).await;
        begin_setup(&pool, id, SECRET).await.unwrap();
        enable(&pool, id, 100, &hashes(10)).await.unwrap();
        session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        assert!(disable(&pool, id, true).await.unwrap());

        assert_eq!(
            status(&pool, id).await.unwrap(),
            Status {
                enabled: false,
                recovery_codes_left: 0
            }
        );
        assert_eq!(sessions(&pool).await, 0);
        assert!(!disable(&pool, id, false).await.unwrap());
        assert!(enabled_user_ids(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn challenges_expire_and_wear_out(pool: SqlitePool) {
        let id = carol(&pool).await;
        create_challenge(&pool, b"a", id, 100, 400).await.unwrap();
        create_challenge(&pool, b"b", id, 100, 200).await.unwrap();

        assert_eq!(find_challenge(&pool, b"a", 150).await.unwrap(), Some(id));
        assert_eq!(find_challenge(&pool, b"b", 300).await.unwrap(), None);

        for n in 1..=MAX_CHALLENGE_FAILURES {
            let worn_out = fail_challenge(&pool, b"a").await.unwrap();
            assert_eq!(worn_out, n == MAX_CHALLENGE_FAILURES, "{n}");
        }
        assert_eq!(find_challenge(&pool, b"a", 150).await.unwrap(), None);

        create_challenge(&pool, b"c", id, 100, 400).await.unwrap();
        assert!(finish_challenge(&pool, b"c").await.unwrap());
        assert!(!finish_challenge(&pool, b"c").await.unwrap());
    }

    #[sqlx::test]
    async fn requiring_two_factor_is_off_by_default(pool: SqlitePool) {
        assert!(!require_two_factor(&pool).await.unwrap());
        set_require_two_factor(&pool, true).await.unwrap();
        assert!(require_two_factor(&pool).await.unwrap());
    }

    #[test]
    fn debug_hides_the_secret() {
        let totp = Totp {
            secret: SECRET.to_vec(),
            enabled: true,
            last_used_step: 1,
        };
        assert!(!format!("{totp:?}").contains("1234"));
    }
}
