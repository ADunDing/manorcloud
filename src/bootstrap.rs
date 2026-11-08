//! 初始管理員。
//!
//! 全新安裝時沒有任何帳號，也沒有公開的註冊。伺服器啟動時（開始監聽之前）
//! 自動建立管理員 `admin`，產生隨機密碼並印在 log。
//!
//! 安全性的基礎：只有能看到伺服器 log 的人（也就是安裝者）才能取得這組密碼。
//! 帳號名稱是可預測的，所以安全性全部落在隨機密碼上。
//!
//! log 會被長期保存，所以這組密碼必須「用一次就失效」：
//! 它被標記為必須修改，首次登入後就要換掉（由第 3 步在後端強制）。
//! 若錯過 log 中的密碼，只要初始管理員尚未被認領，重新啟動就會產生新的密碼。

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::SqlitePool;

use crate::{auth, db::user};

/// 初始管理員的帳號名稱。首次登入時可以修改。
pub const INITIAL_ADMIN_USERNAME: &str = "admin";

/// 隨機密碼的位元組數：128 位元，編碼後 22 個字元
const PASSWORD_BYTES: usize = 16;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// 這次啟動產生的初始管理員帳密，要印給安裝者看。
///
/// 刻意不 derive Debug：它帶著明文密碼，除了刻意印出的那一次，
/// 不應該因為某處的 `{:?}` 出現在其他地方。
pub struct InitialAdmin {
    pub username: String,
    pub password: String,
    /// true：這次新建立；false：尚未認領，重新產生了密碼
    pub created: bool,
}

/// 確保系統可以被登入。
///
/// - 沒有任何使用者：建立初始管理員
/// - 初始管理員尚未被認領：重新產生密碼（舊密碼與它的所有 session 立即失效）
/// - 其他情況：什麼都不做，回傳 None
pub async fn ensure_initial_admin(pool: &SqlitePool) -> Result<Option<InitialAdmin>, Error> {
    // 1. 尚未被認領：重新產生密碼
    if let Some(admin) = user::find_unclaimed_initial_admin(pool).await? {
        let password = generate_password();
        let hash = hash(password.clone()).await?;
        user::replace_password(pool, admin.id, &hash, true).await?;
        return Ok(Some(InitialAdmin {
            username: admin.username,
            password,
            created: false,
        }));
    }

    // 2. 已經有使用者：正常的啟動。先查詢再雜湊，避免每次啟動都白跑一次 Argon2
    if user::has_any_user(pool).await? {
        return Ok(None);
    }

    // 3. 全新安裝：建立初始管理員
    let password = generate_password();
    let hash = hash(password.clone()).await?;
    match user::create_initial_admin(pool, INITIAL_ADMIN_USERNAME, &hash).await? {
        Some(_) => Ok(Some(InitialAdmin {
            username: INITIAL_ADMIN_USERNAME.to_string(),
            password,
            created: true,
        })),
        // 查詢與建立之間有人先建立了使用者（理論上不會發生，伺服器尚未開始監聽）
        None => Ok(None),
    }
}

/// 要印在 log 中的訊息
pub fn banner(admin: &InitialAdmin) -> String {
    let title = if admin.created {
        "ManorCloud initial administrator created"
    } else {
        "ManorCloud initial administrator not claimed yet; a new password was generated"
    };
    let line = "=".repeat(64);
    format!(
        "\n{line}\n  {title}\n    username: {}\n    password: {}\n  \
         You must change the username and password at first login; this password stops working after that.\n  \
         Missed this message? Restart the service to get a new password.\n{line}\n",
        admin.username, admin.password
    )
}

/// 產生隨機密碼：128 位元的安全亂數，以 URL-safe base64 編碼（只含英數字、`-`、`_`）。
/// 管理員建立帳號與重設密碼時也使用同一個函式，臨時密碼的強度一致。
pub(crate) fn generate_password() -> String {
    let mut bytes = [0u8; PASSWORD_BYTES];
    getrandom::fill(&mut bytes).expect("failed to get secure random bytes from the OS");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 雜湊密碼。Argon2 是重 CPU 運算，放到專用執行緒。
async fn hash(password: String) -> Result<String, Error> {
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await?
        .map_err(|e| format!("failed to hash the password: {e}"))?;
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::session;

    async fn role_and_must_change(pool: &SqlitePool, username: &str) -> (String, bool) {
        sqlx::query_as::<_, (String, bool)>(
            "SELECT users.role, password_credentials.must_change_password
             FROM users JOIN password_credentials ON password_credentials.user_id = users.id
             WHERE users.username = ?",
        )
        .bind(username)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// 這組帳密能否通過密碼驗證
    async fn password_works(pool: &SqlitePool, username: &str, password: &str) -> bool {
        let login = user::find_password_login(pool, username)
            .await
            .unwrap()
            .unwrap();
        auth::verify_password(password, &login.password_hash)
    }

    async fn user_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn generated_password_is_strong_and_typeable() {
        let a = generate_password();
        let b = generate_password();

        assert_eq!(a.len(), 22);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_ne!(a, b);
    }

    #[sqlx::test]
    async fn fresh_install_creates_admin(pool: SqlitePool) {
        let admin = ensure_initial_admin(&pool).await.unwrap().unwrap();

        assert!(admin.created);
        assert_eq!(admin.username, "admin");
        assert_eq!(
            role_and_must_change(&pool, "admin").await,
            ("admin".to_string(), true)
        );
        assert!(password_works(&pool, "admin", &admin.password).await);
    }

    #[sqlx::test]
    async fn restart_while_unclaimed_regenerates_password(pool: SqlitePool) {
        let first = ensure_initial_admin(&pool).await.unwrap().unwrap();
        // 有人用舊密碼登入了
        let admin_id = user::find_password_login(&pool, "admin")
            .await
            .unwrap()
            .unwrap()
            .user_id;
        session::create_session(&pool, admin_id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        let second = ensure_initial_admin(&pool).await.unwrap().unwrap();

        assert!(!second.created);
        assert_ne!(first.password, second.password);
        assert!(
            !password_works(&pool, "admin", &first.password).await,
            "舊密碼必須失效"
        );
        assert!(password_works(&pool, "admin", &second.password).await);
        // 舊密碼建立的 session 也一併失效
        assert!(
            session::find_user_by_token_hash(&pool, &[1u8; 32])
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(user_count(&pool).await, 1);
    }

    #[sqlx::test]
    async fn claimed_admin_is_left_alone(pool: SqlitePool) {
        ensure_initial_admin(&pool).await.unwrap().unwrap();
        // 模擬首次登入後已修改密碼
        sqlx::query("UPDATE password_credentials SET must_change_password = 0")
            .execute(&pool)
            .await
            .unwrap();

        assert!(ensure_initial_admin(&pool).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn existing_users_are_left_alone(pool: SqlitePool) {
        user::create_user(&pool, "carol", "x").await.unwrap();

        assert!(ensure_initial_admin(&pool).await.unwrap().is_none());
        // 沒有多建立一個 admin
        assert_eq!(user_count(&pool).await, 1);
    }

    #[sqlx::test]
    async fn family_member_with_temporary_password_is_not_reset(pool: SqlitePool) {
        ensure_initial_admin(&pool).await.unwrap().unwrap();
        // 管理員（尚未修改自己的密碼）建立了一個家人帳號，家人的密碼也是臨時的
        let family = user::create_user(&pool, "mom", "temporary").await.unwrap();
        sqlx::query("UPDATE password_credentials SET must_change_password = 1 WHERE user_id = ?")
            .bind(family)
            .execute(&pool)
            .await
            .unwrap();

        // 系統已不只一個使用者：不再視為「尚未認領」，誰的密碼都不碰
        assert!(ensure_initial_admin(&pool).await.unwrap().is_none());
        let mom = user::find_password_login(&pool, "mom")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mom.password_hash, "temporary");
    }

    #[test]
    fn banner_shows_credentials() {
        let text = banner(&InitialAdmin {
            username: "admin".to_string(),
            password: "Xk3m9_Qv2LpR7wNz4TbYcA".to_string(),
            created: true,
        });

        assert!(text.contains("admin"));
        assert!(text.contains("Xk3m9_Qv2LpR7wNz4TbYcA"));
    }
}
