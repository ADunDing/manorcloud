//! SSO 的資料：設定、帳號與身分的對應、進行中的登入流程。
//!
//! 全部使用執行期的查詢（不是 query! 巨集）：新增這個模組不需要重新產生 .sqlx。

use std::fmt;

use sqlx::SqlitePool;

/// 同時最多幾個進行中的登入流程。開始登入不需要帳號，
/// 沒有上限的話，任何人都能不斷地開始登入、塞滿這張表
const MAX_ACTIVE_FLOWS: i64 = 500;

// ==================== 設定 ====================

/// Administration → Sign-in 的設定。
///
/// 刻意不 derive Debug：自訂的 Debug 會遮蔽 client secret，避免被印到 log 裡
#[derive(Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SsoSettings {
    pub enabled: bool,
    pub display_name: String,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub public_url: String,
    pub auto_create: bool,
    /// 身分提供者中的管理員群組：設定時，每次用 SSO 登入都依這個群組決定角色；空字串時不使用
    pub admin_group: String,
}

impl fmt::Debug for SsoSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SsoSettings")
            .field("enabled", &self.enabled)
            .field("display_name", &self.display_name)
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("public_url", &self.public_url)
            .field("auto_create", &self.auto_create)
            .field("admin_group", &self.admin_group)
            .finish()
    }
}

/// 目前的設定；還沒有設定過時為 None
pub async fn load_settings(pool: &SqlitePool) -> Result<Option<SsoSettings>, sqlx::Error> {
    sqlx::query_as(
        "SELECT enabled, display_name, issuer_url, client_id, client_secret, public_url, auto_create,
                admin_group
         FROM sso_settings WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
}

/// 儲存設定（只有一列：有就更新，沒有就新增）。值必須已經驗證過
pub async fn save_settings(pool: &SqlitePool, settings: &SsoSettings) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sso_settings
             (id, enabled, display_name, issuer_url, client_id, client_secret, public_url, auto_create,
              admin_group)
         VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
             enabled = excluded.enabled,
             display_name = excluded.display_name,
             issuer_url = excluded.issuer_url,
             client_id = excluded.client_id,
             client_secret = excluded.client_secret,
             public_url = excluded.public_url,
             auto_create = excluded.auto_create,
             admin_group = excluded.admin_group,
             updated_at = unixepoch()",
    )
    .bind(settings.enabled)
    .bind(&settings.display_name)
    .bind(&settings.issuer_url)
    .bind(&settings.client_id)
    .bind(&settings.client_secret)
    .bind(&settings.public_url)
    .bind(settings.auto_create)
    .bind(&settings.admin_group)
    .execute(pool)
    .await?;
    Ok(())
}

// ==================== 帳號與身分的對應 ====================

/// 這個身分對應的帳號；沒有對應時為 None
pub async fn find_user_by_identity(
    pool: &SqlitePool,
    issuer: &str,
    subject: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar("SELECT user_id FROM sso_identities WHERE issuer = ? AND subject = ?")
        .bind(issuer)
        .bind(subject)
        .fetch_optional(pool)
        .await
}

/// 帳號的登入方式：有沒有密碼、有沒有連結 SSO
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignInMethods {
    pub has_password: bool,
    pub sso_linked: bool,
}

pub async fn sign_in_methods(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<SignInMethods, sqlx::Error> {
    let (has_password, sso_linked): (bool, bool) = sqlx::query_as(
        "SELECT
             EXISTS (SELECT 1 FROM password_credentials WHERE user_id = ?1),
             EXISTS (SELECT 1 FROM sso_identities WHERE user_id = ?1)",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(SignInMethods {
        has_password,
        sso_linked,
    })
}

#[derive(Debug)]
pub enum LinkError {
    /// 這個 SSO 身分已經連結到另一個帳號
    IdentityTaken,
    /// 這個帳號已經連結了一個 SSO 身分
    AlreadyLinked,
    Db(sqlx::Error),
}

/// 把 SSO 身分連結到既有的帳號
pub async fn link_identity(
    pool: &SqlitePool,
    user_id: i64,
    issuer: &str,
    subject: &str,
) -> Result<(), LinkError> {
    // 先查身分是否被別人使用：兩個 UNIQUE（user_id、issuer + subject）的錯誤訊息
    // 無法可靠地分辨是哪一個，所以明確地查。之後的 INSERT 仍受兩個約束保護
    if let Some(owner) = find_user_by_identity(pool, issuer, subject)
        .await
        .map_err(LinkError::Db)?
    {
        return Err(if owner == user_id {
            LinkError::AlreadyLinked
        } else {
            LinkError::IdentityTaken
        });
    }
    sqlx::query("INSERT INTO sso_identities (user_id, issuer, subject) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(issuer)
        .bind(subject)
        .execute(pool)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation())
            {
                LinkError::AlreadyLinked
            } else {
                LinkError::Db(e)
            }
        })?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum UnlinkOutcome {
    Unlinked,
    NotLinked,
    /// 帳號沒有密碼：取消連結之後就再也無法登入
    NoPassword,
}

/// 取消帳號的 SSO 連結。只有能用密碼登入的帳號可以取消
pub async fn unlink_identity(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<UnlinkOutcome, sqlx::Error> {
    // 條件寫在同一條 DELETE 裡：檢查與刪除之間不會有空檔
    let deleted = sqlx::query(
        "DELETE FROM sso_identities
         WHERE user_id = ?1
           AND EXISTS (SELECT 1 FROM password_credentials WHERE user_id = ?1)",
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if deleted > 0 {
        return Ok(UnlinkOutcome::Unlinked);
    }
    Ok(if sign_in_methods(pool, user_id).await?.sso_linked {
        UnlinkOutcome::NoPassword
    } else {
        UnlinkOutcome::NotLinked
    })
}

#[derive(Debug)]
pub enum CreateUserError {
    UsernameTaken,
    /// 同時的另一個登入已經用這個身分建立了帳號
    IdentityTaken,
    Db(sqlx::Error),
}

/// 第一次用 SSO 登入時建立帳號：一般成員、沒有密碼，連結到這個身分。
/// 兩張表在同一個交易中寫入，不會出現「有帳號卻沒有任何登入方式」的半成品。
/// 名稱必須已通過帳號名稱的驗證
pub async fn create_user_with_identity(
    pool: &SqlitePool,
    username: &str,
    issuer: &str,
    subject: &str,
) -> Result<i64, CreateUserError> {
    let mut tx = pool.begin().await.map_err(CreateUserError::Db)?;

    let user_id = sqlx::query("INSERT INTO users (username) VALUES (?)")
        .bind(username)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation())
            {
                CreateUserError::UsernameTaken
            } else {
                CreateUserError::Db(e)
            }
        })?
        .last_insert_rowid();

    sqlx::query("INSERT INTO sso_identities (user_id, issuer, subject) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(issuer)
        .bind(subject)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation())
            {
                CreateUserError::IdentityTaken
            } else {
                CreateUserError::Db(e)
            }
        })?;

    tx.commit().await.map_err(CreateUserError::Db)?;
    Ok(user_id)
}

// ==================== 登入流程 ====================

/// 進行中的登入流程
#[derive(Debug, sqlx::FromRow)]
pub struct Flow {
    pub nonce: String,
    pub pkce_verifier: String,
    /// 連結既有帳號時是那個帳號；一般登入是 None
    pub link_user_id: Option<i64>,
}

/// 記錄一個新的登入流程。進行中的流程已經太多時回傳 false（不記錄）。
/// 順便清除已經過期的流程
pub async fn create_flow(
    pool: &SqlitePool,
    state_hash: &[u8],
    flow: &Flow,
    now: i64,
    expires_at: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query("DELETE FROM sso_flows WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;

    // 數量的檢查與新增在同一條 SQL：同時開始的流程不會一起超過上限
    let inserted = sqlx::query(
        "INSERT INTO sso_flows (state_hash, nonce, pkce_verifier, link_user_id, expires_at)
         SELECT ?, ?, ?, ?, ?
         WHERE (SELECT COUNT(*) FROM sso_flows) < ?",
    )
    .bind(state_hash)
    .bind(&flow.nonce)
    .bind(&flow.pkce_verifier)
    .bind(flow.link_user_id)
    .bind(expires_at)
    .bind(MAX_ACTIVE_FLOWS)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted > 0)
}

/// 取出並刪除一個登入流程：每個流程只能使用一次（同一個回呼網址被重送也不會登入兩次）。
/// 不存在或已過期時回傳 None
pub async fn take_flow(
    pool: &SqlitePool,
    state_hash: &[u8],
    now: i64,
) -> Result<Option<Flow>, sqlx::Error> {
    sqlx::query_as(
        "DELETE FROM sso_flows WHERE state_hash = ? AND expires_at > ?
         RETURNING nonce, pkce_verifier, link_user_id",
    )
    .bind(state_hash)
    .bind(now)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    fn settings() -> SsoSettings {
        SsoSettings {
            enabled: true,
            display_name: "Authentik".to_string(),
            issuer_url: "https://auth.example.com/application/o/manorcloud/".to_string(),
            client_id: "manorcloud".to_string(),
            client_secret: "s3cret".to_string(),
            public_url: "https://cloud.example.com".to_string(),
            auto_create: false,
            admin_group: String::new(),
        }
    }

    async fn sso_only_user(pool: &SqlitePool, name: &str, subject: &str) -> i64 {
        create_user_with_identity(pool, name, "https://idp", subject)
            .await
            .unwrap()
    }

    async fn set_role(pool: &SqlitePool, id: i64, role: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET role = ? WHERE id = ?")
            .bind(role)
            .bind(id)
            .execute(pool)
            .await
            .map(|_| ())
    }

    #[sqlx::test]
    async fn settings_round_trip(pool: SqlitePool) {
        assert!(load_settings(&pool).await.unwrap().is_none());

        save_settings(&pool, &settings()).await.unwrap();
        let mut changed = settings();
        changed.auto_create = true;
        save_settings(&pool, &changed).await.unwrap();

        assert_eq!(load_settings(&pool).await.unwrap(), Some(changed));
    }

    #[test]
    fn settings_debug_hides_the_secret() {
        let output = format!("{:?}", settings());
        assert!(!output.contains("s3cret"), "{output}");
        assert!(output.contains("Authentik"), "{output}");
    }

    #[sqlx::test]
    async fn sso_only_accounts_have_no_password(pool: SqlitePool) {
        let id = sso_only_user(&pool, "mom", "sub-1").await;

        assert_eq!(
            find_user_by_identity(&pool, "https://idp", "sub-1")
                .await
                .unwrap(),
            Some(id)
        );
        assert_eq!(
            sign_in_methods(&pool, id).await.unwrap(),
            SignInMethods {
                has_password: false,
                sso_linked: true
            }
        );
        // 不能用密碼登入
        assert!(
            user::find_password_login(&pool, "mom")
                .await
                .unwrap()
                .is_none()
        );
        // 對應只認 issuer + subject
        assert!(
            find_user_by_identity(&pool, "https://other", "sub-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn creating_an_account_is_all_or_nothing(pool: SqlitePool) {
        sso_only_user(&pool, "mom", "sub-1").await;

        let taken_name = create_user_with_identity(&pool, "mom", "https://idp", "sub-2").await;
        let taken_identity = create_user_with_identity(&pool, "dad", "https://idp", "sub-1").await;

        assert!(matches!(taken_name, Err(CreateUserError::UsernameTaken)));
        assert!(matches!(
            taken_identity,
            Err(CreateUserError::IdentityTaken)
        ));
        // 交易已 rollback：dad 沒有被建立
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, 1);
    }

    #[sqlx::test]
    async fn linking_rules(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let dave = user::create_user(&pool, "dave", "x").await.unwrap();

        link_identity(&pool, carol, "https://idp", "sub-1")
            .await
            .unwrap();

        // 同一個身分不能再連結到別人；同一個帳號不能再連結第二個身分
        assert!(matches!(
            link_identity(&pool, dave, "https://idp", "sub-1").await,
            Err(LinkError::IdentityTaken)
        ));
        assert!(matches!(
            link_identity(&pool, carol, "https://idp", "sub-2").await,
            Err(LinkError::AlreadyLinked)
        ));
        assert!(matches!(
            link_identity(&pool, carol, "https://idp", "sub-1").await,
            Err(LinkError::AlreadyLinked)
        ));
    }

    #[sqlx::test]
    async fn only_accounts_with_a_password_can_unlink(pool: SqlitePool) {
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        link_identity(&pool, carol, "https://idp", "sub-1")
            .await
            .unwrap();
        let mom = sso_only_user(&pool, "mom", "sub-2").await;

        assert_eq!(
            unlink_identity(&pool, mom).await.unwrap(),
            UnlinkOutcome::NoPassword
        );
        assert_eq!(
            unlink_identity(&pool, carol).await.unwrap(),
            UnlinkOutcome::Unlinked
        );
        assert_eq!(
            unlink_identity(&pool, carol).await.unwrap(),
            UnlinkOutcome::NotLinked
        );
        assert!(sign_in_methods(&pool, mom).await.unwrap().sso_linked);
    }

    #[sqlx::test]
    async fn the_last_admin_with_a_password_is_protected(pool: SqlitePool) {
        let admin = user::create_user(&pool, "admin", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();
        let mom = sso_only_user(&pool, "mom", "sub-1").await;
        // 只用 SSO 的帳號可以成為管理員
        set_role(&pool, mom, "admin").await.unwrap();

        // 但不能因此拿掉最後一個能用密碼登入的管理員
        let demote = set_role(&pool, admin, "member").await;
        let delete = sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(admin)
            .execute(&pool)
            .await;

        for result in [demote.map(|_| ()), delete.map(|_| ())] {
            let error = result.unwrap_err();
            assert_eq!(
                error.as_database_error().unwrap().message(),
                user::LAST_ADMIN_MSG
            );
        }
        // 只用 SSO 的管理員可以被降級：還有一個能用密碼登入的管理員
        set_role(&pool, mom, "member").await.unwrap();
    }

    #[sqlx::test]
    async fn flows_are_single_use_and_expire(pool: SqlitePool) {
        let flow = Flow {
            nonce: "n".to_string(),
            pkce_verifier: "v".to_string(),
            link_user_id: None,
        };
        assert!(
            create_flow(&pool, b"state-a", &flow, 100, 700)
                .await
                .unwrap()
        );
        assert!(
            create_flow(&pool, b"state-b", &flow, 100, 200)
                .await
                .unwrap()
        );

        let taken = take_flow(&pool, b"state-a", 150).await.unwrap().unwrap();
        assert_eq!(taken.nonce, "n");
        // 第二次就沒有了
        assert!(take_flow(&pool, b"state-a", 150).await.unwrap().is_none());
        // 過期的不能使用
        assert!(take_flow(&pool, b"state-b", 300).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn the_number_of_flows_is_limited(pool: SqlitePool) {
        let flow = Flow {
            nonce: "n".to_string(),
            pkce_verifier: "v".to_string(),
            link_user_id: None,
        };
        for n in 0..MAX_ACTIVE_FLOWS {
            assert!(
                create_flow(&pool, &n.to_le_bytes(), &flow, 100, 700)
                    .await
                    .unwrap()
            );
        }

        assert!(
            !create_flow(&pool, b"one-more", &flow, 100, 700)
                .await
                .unwrap()
        );
        // 過期的被清掉之後又可以開始
        assert!(
            create_flow(&pool, b"one-more", &flow, 800, 1400)
                .await
                .unwrap()
        );
    }
}
