use std::fmt;

use sqlx::SqlitePool;

/// 與 migration 中 trigger 的 RAISE 訊息完全一致，之後的管理員 API 用它辨識錯誤。
/// 兩邊必須同步修改；本模組的測試會在不一致時失敗。
pub const LAST_ADMIN_MSG: &str = "at least one admin required";

/// 密碼登入所需的資料。只在驗證密碼時使用。
///
/// 停用狀態不在這裡：密碼驗證成功後，由 `start_session` 統一檢查，
/// 將來的 SSO 登入走同一個入口，不會漏掉。
///
/// 刻意不 derive Debug：自訂的 Debug 會遮蔽雜湊值，避免被印到 log 裡。
pub struct PasswordLogin {
    pub user_id: i64,
    pub username: String,
    pub role: String,
    pub password_hash: String,
    pub must_change_password: bool,
}

impl fmt::Debug for PasswordLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordLogin")
            .field("user_id", &self.user_id)
            .field("username", &self.username)
            .field("role", &self.role)
            .field("password_hash", &"<redacted>")
            .field("must_change_password", &self.must_change_password)
            .finish()
    }
}

/// 建立一般成員帳號與它的密碼，回傳新使用者的 id。
/// password_hash 必須是呼叫端已雜湊好的字串。
/// 目前只有測試使用：正式流程中的帳號一律由初始管理員或管理員建立，都帶有臨時密碼
#[cfg(test)]
pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<i64, sqlx::Error> {
    insert_user(pool, username, password_hash, false).await
}

/// 建立一般成員帳號，密碼為臨時密碼（首次登入必須修改）。用於管理員建立家人的帳號。
pub async fn create_user_with_temporary_password(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<i64, sqlx::Error> {
    insert_user(pool, username, password_hash, true).await
}

/// 兩張表在同一個交易中寫入：不會出現「有帳號卻沒有密碼」的半成品。
/// 這裡用預設的 DEFERRED 交易就夠了：第一個語句就是寫入，沒有「先讀後寫」，
/// 不會遇到刪除那一課討論的 SQLITE_BUSY_SNAPSHOT。
async fn insert_user(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
    must_change: bool,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let user_id = sqlx::query!("INSERT INTO users (username) VALUES (?)", username)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();

    sqlx::query!(
        r#"
        INSERT INTO password_credentials (user_id, password_hash, must_change_password)
        VALUES (?, ?, ?)
        "#,
        user_id,
        password_hash,
        must_change
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(user_id)
}

/// 依帳號名稱查詢密碼登入所需的資料。
///
/// 帳號不存在、或帳號存在但沒有設定密碼（例如將來只用 SSO 的使用者），都回傳 None。
/// 兩者對登入流程來說都是「無法用密碼登入」，回應必須完全相同。
pub async fn find_password_login(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<PasswordLogin>, sqlx::Error> {
    sqlx::query_as!(
        PasswordLogin,
        r#"
        SELECT
            users.id                                  AS "user_id!",
            users.username                            AS "username!",
            users.role                                AS "role!",
            password_credentials.password_hash        AS "password_hash!",
            password_credentials.must_change_password AS "must_change_password!: bool"
        FROM users
        JOIN password_credentials ON password_credentials.user_id = users.id
        WHERE users.username = ?
        "#,
        username
    )
    .fetch_optional(pool)
    .await
}

/// 取得使用者的密碼雜湊；沒有設定密碼時回傳 None。用於修改憑證時驗證目前的密碼。
pub async fn find_password_hash(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar!(
        "SELECT password_hash FROM password_credentials WHERE user_id = ?",
        user_id
    )
    .fetch_optional(pool)
    .await
}

/// 使用者是否已停用；使用者不存在時回傳 None
pub async fn is_disabled(pool: &SqlitePool, user_id: i64) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar!(
        r#"SELECT disabled AS "disabled!: bool" FROM users WHERE id = ?"#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

/// 是否已經有任何使用者
pub async fn has_any_user(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(r#"SELECT EXISTS (SELECT 1 FROM users) AS "exists!: bool""#)
        .fetch_one(pool)
        .await
}

/// 建立初始管理員：角色為 admin，密碼標記為必須修改。
///
/// 只在資料庫中「沒有任何使用者」時才會插入（寫在同一條 SQL 的條件裡），
/// 已有使用者時回傳 None，不建立任何東西。
/// 呼叫端在伺服器開始監聽之前呼叫，本來就沒有並行的問題；SQL 條件是額外的保險。
pub async fn create_initial_admin(
    pool: &SqlitePool,
    username: &str,
    password_hash: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query!(
        r#"
        INSERT INTO users (username, role)
        SELECT ?, 'admin'
        WHERE NOT EXISTS (SELECT 1 FROM users)
        "#,
        username
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() == 0 {
        // 已經有使用者；tx 在這裡被丟棄，自動 rollback
        return Ok(None);
    }
    let user_id = result.last_insert_rowid();

    sqlx::query!(
        r#"
        INSERT INTO password_credentials (user_id, password_hash, must_change_password)
        VALUES (?, ?, 1)
        "#,
        user_id,
        password_hash
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(user_id))
}

/// 尚未被認領的初始管理員
pub struct UnclaimedAdmin {
    pub id: i64,
    pub username: String,
}

/// 找出「尚未被認領」的初始管理員：整個系統只有一個使用者、它是管理員、
/// 而且密碼仍標記為必須修改。
///
/// 三個條件缺一不可：只看「必須修改」的話，管理員之後建立的家人帳號也帶有這個旗標，
/// 每次重啟都會被換掉密碼。
pub async fn find_unclaimed_initial_admin(
    pool: &SqlitePool,
) -> Result<Option<UnclaimedAdmin>, sqlx::Error> {
    sqlx::query_as!(
        UnclaimedAdmin,
        r#"
        SELECT users.id AS "id!", users.username AS "username!"
        FROM users
        JOIN password_credentials ON password_credentials.user_id = users.id
        WHERE users.role = 'admin'
          AND password_credentials.must_change_password = 1
          AND (SELECT COUNT(*) FROM users) = 1
        "#
    )
    .fetch_optional(pool)
    .await
}

/// 替換使用者的密碼，並登出該使用者的所有裝置。回傳是否有找到這個使用者的密碼。
///
/// 換了密碼卻保留舊的 session，持有舊 session 的人（可能就是密碼外洩的原因）仍能繼續使用，
/// 所以兩件事在同一個交易中完成。
/// 用於重新產生初始管理員的密碼，以及之後管理員重設他人密碼。
pub async fn replace_password(
    pool: &SqlitePool,
    user_id: i64,
    password_hash: &str,
    must_change: bool,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query!(
        r#"
        UPDATE password_credentials
        SET password_hash = ?, must_change_password = ?, updated_at = unixepoch()
        WHERE user_id = ?
        "#,
        password_hash,
        must_change,
        user_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if updated == 0 {
        return Ok(false);
    }

    sqlx::query!("DELETE FROM sessions WHERE user_id = ?", user_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(true)
}

/// 修改自己的憑證時可能的失敗
#[derive(Debug)]
pub enum UpdateCredentialsError {
    /// 新的帳號名稱已被使用
    UsernameTaken,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for UpdateCredentialsError {
    fn from(e: sqlx::Error) -> Self {
        UpdateCredentialsError::Db(e)
    }
}

/// 修改帳號名稱及／或密碼（使用者自己操作），在同一個交易中完成。
///
/// - 改密碼時：清除「必須修改」並刪除該使用者的所有 session（呼叫端再為目前的裝置建立新的）
/// - 只改帳號名稱時：session 不受影響（session 連結的是 user_id，不是名稱）
///
/// 呼叫前必須已驗證目前的密碼，新的名稱與密碼也必須已通過格式驗證。
pub async fn update_credentials(
    pool: &SqlitePool,
    user_id: i64,
    new_username: Option<&str>,
    new_password_hash: Option<&str>,
) -> Result<(), UpdateCredentialsError> {
    let mut tx = pool.begin().await?;

    if let Some(username) = new_username {
        sqlx::query!(
            "UPDATE users SET username = ? WHERE id = ?",
            username,
            user_id
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(|db| db.is_unique_violation())
            {
                UpdateCredentialsError::UsernameTaken
            } else {
                UpdateCredentialsError::Db(e)
            }
        })?;
    }

    if let Some(hash) = new_password_hash {
        sqlx::query!(
            r#"
            UPDATE password_credentials
            SET password_hash = ?, must_change_password = 0, updated_at = unixepoch()
            WHERE user_id = ?
            "#,
            hash,
            user_id
        )
        .execute(&mut *tx)
        .await?;

        sqlx::query!("DELETE FROM sessions WHERE user_id = ?", user_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

// ==================== 管理員 ====================

/// 使用者列表的一列（管理員用）。不含任何憑證
pub struct UserSummary {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub disabled: bool,
    pub must_change_password: bool,
    pub created_at: String,
}

/// 所有使用者，依帳號名稱排序。
/// 用 LEFT JOIN：將來只用 SSO 的使用者沒有密碼紀錄，此時「必須修改」視為 false。
pub async fn list_users(pool: &SqlitePool) -> Result<Vec<UserSummary>, sqlx::Error> {
    sqlx::query_as!(
        UserSummary,
        r#"
        SELECT
            users.id         AS "id!",
            users.username   AS "username!",
            users.role       AS "role!",
            users.disabled   AS "disabled!: bool",
            COALESCE(password_credentials.must_change_password, 0)
                             AS "must_change_password!: bool",
            users.created_at AS "created_at!"
        FROM users
        LEFT JOIN password_credentials ON password_credentials.user_id = users.id
        ORDER BY users.username
        "#
    )
    .fetch_all(pool)
    .await
}

/// 單一使用者的摘要；不存在時回傳 None
pub async fn find_user_summary(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<UserSummary>, sqlx::Error> {
    sqlx::query_as!(
        UserSummary,
        r#"
        SELECT
            users.id         AS "id!",
            users.username   AS "username!",
            users.role       AS "role!",
            users.disabled   AS "disabled!: bool",
            COALESCE(password_credentials.must_change_password, 0)
                             AS "must_change_password!: bool",
            users.created_at AS "created_at!"
        FROM users
        LEFT JOIN password_credentials ON password_credentials.user_id = users.id
        WHERE users.id = ?
        "#,
        user_id
    )
    .fetch_optional(pool)
    .await
}

/// 管理員修改使用者時可能的失敗
#[derive(Debug)]
pub enum AdminUpdateError {
    NotFound,
    /// 會讓系統沒有任何有效的管理員（由 trigger 擋下）
    LastAdmin,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AdminUpdateError {
    fn from(e: sqlx::Error) -> Self {
        AdminUpdateError::Db(e)
    }
}

/// 修改使用者的角色及／或停用狀態。`None` 代表不修改該欄位。
///
/// 停用時一併刪除該使用者的所有 session。停用的瞬間，查詢 session 時就會過濾掉他，
/// 但若不刪除，之後重新啟用時，那些尚未過期的舊 session 會全部復活：
/// 若停用的原因是裝置遺失，那台裝置也會跟著恢復登入。
pub async fn update_user(
    pool: &SqlitePool,
    user_id: i64,
    role: Option<&str>,
    disabled: Option<bool>,
) -> Result<(), AdminUpdateError> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query!(
        r#"
        UPDATE users
        SET role = COALESCE(?, role), disabled = COALESCE(?, disabled)
        WHERE id = ?
        "#,
        role,
        disabled,
        user_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if e.as_database_error()
            .is_some_and(|db| db.message() == LAST_ADMIN_MSG)
        {
            AdminUpdateError::LastAdmin
        } else {
            AdminUpdateError::Db(e)
        }
    })?
    .rows_affected();

    if updated == 0 {
        return Err(AdminUpdateError::NotFound);
    }

    if disabled == Some(true) {
        sqlx::query!("DELETE FROM sessions WHERE user_id = ?", user_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn role_of(pool: &SqlitePool, id: i64) -> String {
        sqlx::query_scalar::<_, String>("SELECT role FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
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

    async fn set_disabled(pool: &SqlitePool, id: i64, disabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET disabled = ? WHERE id = ?")
            .bind(disabled)
            .bind(id)
            .execute(pool)
            .await
            .map(|_| ())
    }

    async fn delete_user(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await
            .map(|_| ())
    }

    async fn session_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// 是否為「至少一個管理員」的 trigger 擋下的錯誤
    fn is_last_admin_error(result: &Result<(), sqlx::Error>) -> bool {
        match result {
            Err(e) => e
                .as_database_error()
                .is_some_and(|db| db.message() == LAST_ADMIN_MSG),
            Ok(()) => false,
        }
    }

    // ==================== 建立與查詢 ====================

    #[sqlx::test]
    async fn new_user_is_member_with_password(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "hash").await.unwrap();

        assert_eq!(role_of(&pool, id).await, "member");
        let login = find_password_login(&pool, "carol").await.unwrap().unwrap();
        assert_eq!(login.user_id, id);
        assert_eq!(login.role, "member");
        assert_eq!(login.password_hash, "hash");
        assert!(!login.must_change_password);
    }

    #[sqlx::test]
    async fn unknown_username_has_no_login(pool: SqlitePool) {
        assert!(
            find_password_login(&pool, "nobody")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    async fn user_without_password_cannot_use_password_login(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "hash").await.unwrap();
        // 模擬將來只用 SSO 的使用者：帳號存在，但沒有密碼
        sqlx::query("DELETE FROM password_credentials WHERE user_id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        assert!(find_password_login(&pool, "carol").await.unwrap().is_none());
        assert!(find_password_hash(&pool, id).await.unwrap().is_none());
    }

    #[sqlx::test]
    async fn duplicate_username_is_rejected_without_leftover_rows(pool: SqlitePool) {
        create_user(&pool, "carol", "hash").await.unwrap();

        let result = create_user(&pool, "carol", "other").await;

        assert!(
            result
                .unwrap_err()
                .as_database_error()
                .unwrap()
                .is_unique_violation()
        );
        // 交易已 rollback：只有第一個使用者的密碼
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM password_credentials")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    async fn deleting_user_removes_password(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "hash").await.unwrap();

        delete_user(&pool, id).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM password_credentials")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    async fn is_disabled_reports_state(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "hash").await.unwrap();

        assert_eq!(is_disabled(&pool, id).await.unwrap(), Some(false));
        set_disabled(&pool, id, true).await.unwrap();
        assert_eq!(is_disabled(&pool, id).await.unwrap(), Some(true));
        assert_eq!(is_disabled(&pool, 9999).await.unwrap(), None);
    }

    #[test]
    fn password_login_debug_hides_hash() {
        let login = PasswordLogin {
            user_id: 1,
            username: "carol".to_string(),
            role: "member".to_string(),
            password_hash: "$argon2id$secret".to_string(),
            must_change_password: false,
        };

        let output = format!("{login:?}");

        assert!(!output.contains("argon2"), "{output}");
        assert!(output.contains("carol"), "{output}");
    }

    // ==================== 修改憑證 ====================

    #[sqlx::test]
    async fn changing_password_clears_flag_and_sessions(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "old").await.unwrap();
        sqlx::query("UPDATE password_credentials SET must_change_password = 1")
            .execute(&pool)
            .await
            .unwrap();
        crate::db::session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        update_credentials(&pool, id, None, Some("new"))
            .await
            .unwrap();

        let login = find_password_login(&pool, "carol").await.unwrap().unwrap();
        assert_eq!(login.password_hash, "new");
        assert!(!login.must_change_password);
        assert_eq!(session_count(&pool).await, 0);
    }

    #[sqlx::test]
    async fn renaming_keeps_sessions(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "hash").await.unwrap();
        crate::db::session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        update_credentials(&pool, id, Some("caroline"), None)
            .await
            .unwrap();

        assert!(find_password_login(&pool, "carol").await.unwrap().is_none());
        assert!(
            find_password_login(&pool, "caroline")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(session_count(&pool).await, 1);
    }

    #[sqlx::test]
    async fn taken_username_rolls_back_everything(pool: SqlitePool) {
        create_user(&pool, "alice", "a").await.unwrap();
        let id = create_user(&pool, "carol", "old").await.unwrap();

        let result = update_credentials(&pool, id, Some("alice"), Some("new")).await;

        assert!(matches!(result, Err(UpdateCredentialsError::UsernameTaken)));
        // 名稱衝突時，同一個交易中的密碼也沒有被修改
        let login = find_password_login(&pool, "carol").await.unwrap().unwrap();
        assert_eq!(login.password_hash, "old");
    }

    // ==================== 至少一個有效的管理員 ====================

    #[sqlx::test]
    async fn last_admin_cannot_be_demoted(pool: SqlitePool) {
        let admin = create_user(&pool, "admin", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();

        // 若 migration 的 RAISE 訊息被改掉，這裡會失敗
        assert!(is_last_admin_error(&set_role(&pool, admin, "member").await));
        assert_eq!(role_of(&pool, admin).await, "admin");
    }

    #[sqlx::test]
    async fn last_admin_cannot_be_disabled(pool: SqlitePool) {
        let admin = create_user(&pool, "admin", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();

        assert!(is_last_admin_error(&set_disabled(&pool, admin, true).await));
    }

    #[sqlx::test]
    async fn last_admin_cannot_be_deleted(pool: SqlitePool) {
        let admin = create_user(&pool, "admin", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();

        assert!(is_last_admin_error(&delete_user(&pool, admin).await));
    }

    #[sqlx::test]
    async fn one_of_two_admins_can_be_demoted(pool: SqlitePool) {
        let a = create_user(&pool, "a", "x").await.unwrap();
        let b = create_user(&pool, "b", "x").await.unwrap();
        set_role(&pool, a, "admin").await.unwrap();
        set_role(&pool, b, "admin").await.unwrap();

        set_role(&pool, a, "member").await.unwrap();

        // 剩下 b 一個：不能再降級
        assert!(is_last_admin_error(&set_role(&pool, b, "member").await));
    }

    #[sqlx::test]
    async fn disabled_admin_does_not_count(pool: SqlitePool) {
        let a = create_user(&pool, "a", "x").await.unwrap();
        let b = create_user(&pool, "b", "x").await.unwrap();
        set_role(&pool, a, "admin").await.unwrap();
        set_role(&pool, b, "admin").await.unwrap();
        set_disabled(&pool, a, true).await.unwrap();

        // a 雖然仍是 admin，但已停用，不算有效的管理員：b 是最後一個
        assert!(is_last_admin_error(&set_role(&pool, b, "member").await));
    }

    #[sqlx::test]
    async fn members_are_not_affected(pool: SqlitePool) {
        let admin = create_user(&pool, "admin", "x").await.unwrap();
        let member = create_user(&pool, "carol", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();

        // 一般成員的停用、刪除不受 trigger 影響
        set_disabled(&pool, member, true).await.unwrap();
        delete_user(&pool, member).await.unwrap();
    }

    #[sqlx::test]
    async fn invalid_role_is_rejected(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "x").await.unwrap();

        // CHECK 約束擋下拼錯的角色名稱
        assert!(set_role(&pool, id, "Admin").await.is_err());
    }
    // ==================== 管理員 ====================

    #[sqlx::test]
    async fn temporary_password_user_must_change(pool: SqlitePool) {
        create_user_with_temporary_password(&pool, "mom", "temp")
            .await
            .unwrap();

        let login = find_password_login(&pool, "mom").await.unwrap().unwrap();

        assert!(login.must_change_password);
        assert_eq!(login.role, "member");
    }

    #[sqlx::test]
    async fn list_users_is_sorted_by_name(pool: SqlitePool) {
        create_user(&pool, "zed", "x").await.unwrap();
        create_user(&pool, "amy", "x").await.unwrap();

        let names: Vec<String> = list_users(&pool)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();

        assert_eq!(names, ["amy", "zed"]);
    }

    #[sqlx::test]
    async fn disabling_deletes_sessions_so_reenabling_does_not_revive_them(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "x").await.unwrap();
        crate::db::session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        update_user(&pool, id, None, Some(true)).await.unwrap();
        update_user(&pool, id, None, Some(false)).await.unwrap();

        // 重新啟用後，舊的 session 不會復活
        assert_eq!(session_count(&pool).await, 0);
        assert_eq!(is_disabled(&pool, id).await.unwrap(), Some(false));
    }

    #[sqlx::test]
    async fn changing_role_keeps_sessions(pool: SqlitePool) {
        let id = create_user(&pool, "carol", "x").await.unwrap();
        crate::db::session::create_session(&pool, id, &[1u8; 32], 4_000_000_000)
            .await
            .unwrap();

        update_user(&pool, id, Some("admin"), None).await.unwrap();

        assert_eq!(role_of(&pool, id).await, "admin");
        assert_eq!(session_count(&pool).await, 1);
    }

    #[sqlx::test]
    async fn update_reports_last_admin_and_missing_user(pool: SqlitePool) {
        let admin = create_user(&pool, "admin", "x").await.unwrap();
        set_role(&pool, admin, "admin").await.unwrap();

        let demote = update_user(&pool, admin, Some("member"), None).await;
        let missing = update_user(&pool, 9999, None, Some(true)).await;

        assert!(matches!(demote, Err(AdminUpdateError::LastAdmin)));
        assert!(matches!(missing, Err(AdminUpdateError::NotFound)));
    }
}
