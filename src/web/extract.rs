//! 身分驗證的 extractor。
//!
//! 兩個層級：
//! - `CurrentUser`（預設）：已登入、未停用、**而且不需要修改密碼**。所有一般 API 都用它。
//! - `SessionUser`：已登入、未停用，允許「必須修改密碼」的狀態。
//!   只用於 `/api/me` 與 `/api/me/credentials`，否則使用者會卡在無法修改密碼的狀態。
//!
//! 預設是嚴格的那一個：新增的 API 照慣例寫 `CurrentUser` 就自動受到保護，
//! 要允許「尚未修改密碼」必須刻意改用 `SessionUser`。

use axum::{
    extract::{FromRef, FromRequestParts},
    http::request::Parts,
};
use axum_extra::extract::cookie::CookieJar;
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    auth::SESSION_COOKIE,
    error::{AppError, Forbidden},
};
use crate::{auth, db::session};

/// 未登入時統一的訊息：不區分是沒帶 Cookie 還是 session 失效
const NOT_LOGGED_IN: &str = "尚未登入或登入已過期";

/// 已登入且未停用的使用者，包含「必須修改密碼」的狀態。
///
/// **只用於 `/api/me` 與 `/api/me/credentials`**。其他 API 一律使用 `CurrentUser`。
///
/// 每個請求都從資料庫重新讀取。被停用的使用者，其 session 在查詢中即被過濾，
/// 所以停用會立即生效，不必等 session 過期。
#[derive(Debug, Clone, Serialize)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
}

/// 目前登入的使用者：已登入、未停用，而且不需要修改密碼。
///
/// 作為 extractor 使用：handler 參數寫上 `CurrentUser`，未登入的請求回 401，
/// 必須修改密碼的請求回 403，都在進入 handler 之前就被擋下。
#[derive(Debug, Clone, Serialize)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub role: String,
}

/// 對任何「能從中取出 SqlitePool」的 state 都可使用
impl<S> FromRequestParts<S> for SessionUser
where
    SqlitePool: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // 1. 呼叫另一個 extractor 取得 Cookie；CookieJar 不會失敗，所以 Ok 是唯一可能
        let Ok(jar) = CookieJar::from_request_parts(parts, state).await;

        // 2. 沒帶 session Cookie → 401
        let Some(cookie) = jar.get(SESSION_COOKIE) else {
            return Err(unauthorized());
        };

        // 3. 雜湊後查資料庫
        let token_hash = auth::session::hash_token(cookie.value());
        let pool = SqlitePool::from_ref(state);

        // 4. 資料庫出錯 → `?` 自動轉成 500；查無有效 session（含已停用）→ 401
        let info = session::find_user_by_token_hash(&pool, &token_hash)
            .await?
            .ok_or_else(unauthorized)?;

        Ok(SessionUser {
            id: info.user_id,
            username: info.username,
            role: info.role,
            must_change_password: info.must_change_password,
        })
    }
}

/// 建立在 SessionUser 之上，多一道「不需要修改密碼」的檢查
impl<S> FromRequestParts<S> for CurrentUser
where
    SqlitePool: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = SessionUser::from_request_parts(parts, state).await?;

        if user.must_change_password {
            // 403 而不是 401：使用者已經登入，只是要先完成一件事，
            // 前端依代碼 password_change_required 顯示修改畫面
            return Err(AppError::Forbidden(Forbidden::PasswordChangeRequired));
        }

        Ok(CurrentUser {
            id: user.id,
            username: user.username,
            role: user.role,
        })
    }
}

/// 目前登入的管理員：已登入、未停用、不需要修改密碼，而且角色是 admin。
///
/// 建立在 CurrentUser 之上，多一道角色檢查，不是管理員就回 403（admin_required）。
/// 角色每個請求都從資料庫重新讀取，降級後立即生效。
#[derive(Debug, Clone)]
pub struct AdminUser {
    pub id: i64,
}

impl<S> FromRequestParts<S> for AdminUser
where
    SqlitePool: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = CurrentUser::from_request_parts(parts, state).await?;

        if user.role != "admin" {
            return Err(AppError::Forbidden(Forbidden::AdminRequired));
        }

        Ok(AdminUser { id: user.id })
    }
}

fn unauthorized() -> AppError {
    AppError::Unauthorized(NOT_LOGGED_IN.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user as db_user;
    use axum::http::{Request, header};

    /// 建立一個請求的 Parts，可選擇是否帶 Cookie
    fn request_parts(cookie: Option<&str>) -> Parts {
        let mut builder = Request::builder().uri("/");
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        builder.body(()).unwrap().into_parts().0
    }

    /// 建立使用者與有效 session，回傳原始 token
    async fn login_as_carol(pool: &SqlitePool) -> String {
        let user_id = db_user::create_user(pool, "carol", "fake_hash")
            .await
            .unwrap();
        let token = auth::session::generate_token();
        let hash = auth::session::hash_token(&token);
        // 遠在未來的過期時間（約 2096 年）
        session::create_session(pool, user_id, &hash, 4_000_000_000)
            .await
            .unwrap();
        token
    }

    fn cookie_for(token: &str) -> String {
        format!("{SESSION_COOKIE}={token}")
    }

    async fn require_password_change(pool: &SqlitePool) {
        sqlx::query("UPDATE password_credentials SET must_change_password = 1")
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn valid_session_returns_user(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        let mut parts = request_parts(Some(&cookie_for(&token)));

        let user = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap();

        assert_eq!(user.username, "carol");
        assert_eq!(user.role, "member");
    }

    #[sqlx::test]
    async fn missing_cookie_is_unauthorized(pool: SqlitePool) {
        let mut parts = request_parts(None);

        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[sqlx::test]
    async fn unknown_token_is_unauthorized(pool: SqlitePool) {
        let mut parts = request_parts(Some(&cookie_for("not-a-real-token")));

        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[sqlx::test]
    async fn database_error_is_internal_not_unauthorized(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        let mut parts = request_parts(Some(&cookie_for(&token)));

        // 模擬資料庫故障：關閉連線池後，任何查詢都會失敗
        pool.close().await;

        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();

        assert!(matches!(err, AppError::Internal));
    }

    #[sqlx::test]
    async fn disabled_user_is_unauthorized(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        sqlx::query("UPDATE users SET disabled = 1 WHERE username = 'carol'")
            .execute(&pool)
            .await
            .unwrap();
        let mut parts = request_parts(Some(&cookie_for(&token)));

        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();

        // 已登入的 session 在停用後立即失效
        assert!(matches!(err, AppError::Unauthorized(_)));
    }

    #[sqlx::test]
    async fn must_change_password_blocks_current_user(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        require_password_change(&pool).await;
        let mut parts = request_parts(Some(&cookie_for(&token)));

        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();

        // 403 而不是 401：已登入，只是必須先修改密碼
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    #[sqlx::test]
    async fn must_change_password_is_allowed_for_session_user(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        require_password_change(&pool).await;
        let mut parts = request_parts(Some(&cookie_for(&token)));

        let user = SessionUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap();

        assert!(user.must_change_password);
    }
}
