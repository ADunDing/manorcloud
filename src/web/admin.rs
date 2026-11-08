//! 管理員功能：使用者管理。只有有效的管理員可以使用（`AdminUser`）。
//!
//! 臨時密碼由伺服器產生，只在建立或重設的那一次回應中出現，伺服器不保存明文。
//! 使用者首次登入時被強制修改，管理員從此不會知道家人的正式密碼。

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::{StatusCode, header},
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    auth::{hash_password, is_unique_violation, validate_username},
    error::AppError,
    extract::AdminUser,
    state::AppState,
};
use crate::{
    bootstrap::generate_password,
    db::user::{self, AdminUpdateError, UserSummary},
};

const USER_NOT_FOUND: &str = "找不到使用者";

/// 含有臨時密碼的回應：不得被瀏覽器或任何中間快取保存
const NO_STORE: [(header::HeaderName, &str); 1] = [(header::CACHE_CONTROL, "no-store")];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/users", get(list_users).post(create_user))
        .route("/api/admin/users/{id}", patch(update_user))
        .route("/api/admin/users/{id}/reset-password", post(reset_password))
}

// ==================== 型別 ====================

/// 使用者的對外格式（管理員用）。不含任何憑證
#[derive(Serialize)]
struct UserItem {
    id: i64,
    username: String,
    role: String,
    disabled: bool,
    must_change_password: bool,
    created_at: String,
}

impl From<UserSummary> for UserItem {
    fn from(u: UserSummary) -> Self {
        UserItem {
            id: u.id,
            username: u.username,
            role: u.role,
            disabled: u.disabled,
            must_change_password: u.must_change_password,
            created_at: u.created_at,
        }
    }
}

/// 角色。用 enum 而不是字串：拼錯的角色在解析 JSON 時就被拒絕（422），不會送到資料庫
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Admin,
    Member,
}

impl Role {
    /// 資料庫中的值，對應 users.role 的 CHECK 約束
    fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }
}

// ==================== 列出 ====================

/// GET /api/admin/users：所有使用者
async fn list_users(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<UserItem>>, AppError> {
    let users = user::list_users(&pool).await?;
    Ok(Json(users.into_iter().map(UserItem::from).collect()))
}

// ==================== 建立 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUserRequest {
    username: String,
}

#[derive(Serialize)]
struct CreatedUser {
    user: UserItem,
    /// 只在這次回應中出現一次
    temporary_password: String,
}

/// POST /api/admin/users：建立一般成員帳號，回傳臨時密碼
async fn create_user(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<CreateUserRequest>,
) -> Result<
    (
        StatusCode,
        [(header::HeaderName, &'static str); 1],
        Json<CreatedUser>,
    ),
    AppError,
> {
    validate_username(&req.username)?;

    // 由伺服器產生，不讓管理員自己輸入：避免弱密碼或每個家人都一樣的密碼
    let password = generate_password();
    let hash = hash_password(password.clone()).await?;

    let id = user::create_user_with_temporary_password(&pool, &req.username, &hash)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                AppError::Conflict("帳號已被使用".to_string())
            } else {
                AppError::internal(e)
            }
        })?;

    let summary = user::find_user_summary(&pool, id)
        .await?
        .ok_or_else(|| AppError::internal("剛建立的使用者不存在"))?;

    Ok((
        StatusCode::CREATED,
        NO_STORE,
        Json(CreatedUser {
            user: summary.into(),
            temporary_password: password,
        }),
    ))
}

// ==================== 修改角色與停用 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateUserRequest {
    role: Option<Role>,
    disabled: Option<bool>,
}

/// PATCH /api/admin/users/{id}：修改角色及／或停用狀態。
///
/// 不能停用自己：會立刻把自己登出，而且很可能是誤按。
/// 降級自己則允許（把管理員交接給別人的正常方式），最後一位有效的管理員仍由 trigger 保護。
async fn update_user(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<UserItem>, AppError> {
    if req.role.is_none() && req.disabled.is_none() {
        return Err(AppError::BadRequest("沒有要修改的內容".to_string()));
    }
    if id == admin.id && req.disabled == Some(true) {
        return Err(AppError::BadRequest("不能停用自己的帳號".to_string()));
    }

    user::update_user(&pool, id, req.role.map(Role::as_str), req.disabled)
        .await
        .map_err(|e| match e {
            AdminUpdateError::NotFound => user_not_found(),
            AdminUpdateError::LastAdmin => {
                AppError::Conflict("至少需要保留一位有效的管理員".to_string())
            }
            AdminUpdateError::Db(e) => AppError::internal(e),
        })?;

    let summary = user::find_user_summary(&pool, id)
        .await?
        .ok_or_else(user_not_found)?;
    Ok(Json(summary.into()))
}

// ==================== 重設密碼 ====================

#[derive(Serialize)]
struct ResetPasswordResponse {
    /// 只在這次回應中出現一次
    temporary_password: String,
}

/// POST /api/admin/users/{id}/reset-password：重設為新的臨時密碼。
///
/// 同時登出該使用者的所有裝置（replace_password 在同一個交易中刪除 session），
/// 使用者下次登入時必須修改密碼。
/// 不能用來重設自己的密碼：應該從「帳號設定」修改，那裡會要求目前的密碼。
async fn reset_password(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<ResetPasswordResponse>,
    ),
    AppError,
> {
    if id == admin.id {
        return Err(AppError::BadRequest(
            "請從「帳號設定」修改自己的密碼".to_string(),
        ));
    }

    let password = generate_password();
    let hash = hash_password(password.clone()).await?;

    if !user::replace_password(&pool, id, &hash, true).await? {
        return Err(user_not_found());
    }

    Ok((
        NO_STORE,
        Json(ResetPasswordResponse {
            temporary_password: password,
        }),
    ))
}

fn user_not_found() -> AppError {
    AppError::NotFound(USER_NOT_FOUND.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::{
        auth::SESSION_COOKIE,
        test_support::{MIB, TestApp, empty_request, request},
    };
    use crate::{
        auth::session::{generate_token, hash_token},
        db::{session, user},
    };
    use axum::{
        body::{Body, Bytes},
        http::{HeaderMap, Method, StatusCode, header},
    };
    use serde_json::Value;
    use sqlx::SqlitePool;

    // ==================== 測試輔助 ====================
    //
    // 建立帳號與重設密碼會執行真正的 Argon2，debug 模式下較慢，屬於正常現象。

    /// 讓 TestApp 內建的 carol 成為管理員
    async fn make_admin(t: &TestApp) {
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    /// 為指定使用者建立一個 session，回傳可以放進 Cookie header 的值
    async fn session_for(t: &TestApp, user_id: i64) -> String {
        let token = generate_token();
        session::create_session(&t.pool, user_id, &hash_token(&token), 4_000_000_000)
            .await
            .unwrap();
        format!("{SESSION_COOKIE}={token}")
    }

    async fn send_json(
        t: &TestApp,
        method: Method,
        uri: &str,
        cookie: &str,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let req = request(method, uri, Some(cookie))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        t.send(req).await
    }

    async fn login(t: &TestApp, username: &str, password: &str) -> (StatusCode, Value) {
        let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
        let req = request(Method::POST, "/api/login", None)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _, body) = t.send(req).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn me_status(t: &TestApp, cookie: &str) -> StatusCode {
        t.send(empty_request(Method::GET, "/api/me", Some(cookie)))
            .await
            .0
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    fn user_uri(id: i64) -> String {
        format!("/api/admin/users/{id}")
    }

    // ==================== 權限 ====================

    #[sqlx::test]
    async fn members_cannot_use_admin_api(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/users",
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json(&body)["code"], "admin_required");
    }

    #[sqlx::test]
    async fn list_users_hides_credentials(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        t.other_user("alice").await;

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/users",
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body).as_array().unwrap().len(), 2);
        assert!(!String::from_utf8_lossy(&body).contains("password_hash"));
    }

    // ==================== 建立 ====================

    #[sqlx::test]
    async fn created_user_logs_in_with_temporary_password(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;

        let (status, headers, body) = send_json(
            &t,
            Method::POST,
            "/api/admin/users",
            &t.cookie,
            r#"{"username":"mom"}"#,
        )
        .await;

        assert_eq!(status, StatusCode::CREATED);
        // 含有臨時密碼的回應不得被快取
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        let body = json(&body);
        assert_eq!(body["user"]["username"], "mom");
        assert_eq!(body["user"]["role"], "member");
        let temporary = body["temporary_password"].as_str().unwrap();
        assert_eq!(temporary.len(), 22);

        // 用臨時密碼登入：必須先修改
        let (status, body) = login(&t, "mom", temporary).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["must_change_password"], true);
    }

    #[sqlx::test]
    async fn create_user_is_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;

        let cases = [
            (r#"{"username":"Mom"}"#, StatusCode::BAD_REQUEST),
            // carol 是 TestApp 建立的使用者
            (r#"{"username":"carol"}"#, StatusCode::CONFLICT),
            (
                r#"{"username":"mom","password":"x"}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];

        for (body, expected) in cases {
            let (status, _, _) =
                send_json(&t, Method::POST, "/api/admin/users", &t.cookie, body).await;
            assert_eq!(status, expected, "{body}");
        }
    }

    // ==================== 修改角色與停用 ====================

    #[sqlx::test]
    async fn disabling_logs_out_and_reenabling_does_not_revive_session(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = t.other_user("alice").await;
        let alice_cookie = session_for(&t, alice).await;
        assert_eq!(me_status(&t, &alice_cookie).await, StatusCode::OK);

        let (status, _, body) = send_json(
            &t,
            Method::PATCH,
            &user_uri(alice),
            &t.cookie,
            r#"{"disabled":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["disabled"], true);
        assert_eq!(me_status(&t, &alice_cookie).await, StatusCode::UNAUTHORIZED);

        send_json(
            &t,
            Method::PATCH,
            &user_uri(alice),
            &t.cookie,
            r#"{"disabled":false}"#,
        )
        .await;

        // 重新啟用後，停用前的 session 不會復活
        assert_eq!(me_status(&t, &alice_cookie).await, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn promoting_takes_effect_immediately(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = t.other_user("alice").await;
        let alice_cookie = session_for(&t, alice).await;

        send_json(
            &t,
            Method::PATCH,
            &user_uri(alice),
            &t.cookie,
            r#"{"role":"admin"}"#,
        )
        .await;

        // 角色每個請求都重新讀取：alice 用同一個 session 就能使用管理員 API
        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/users",
                Some(&alice_cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test]
    async fn self_protection_rules(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let me = user_uri(t.user_id);

        // 不能停用自己
        let (status, _, _) =
            send_json(&t, Method::PATCH, &me, &t.cookie, r#"{"disabled":true}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // 唯一的管理員不能降級自己（trigger 擋下）
        let (status, _, _) =
            send_json(&t, Method::PATCH, &me, &t.cookie, r#"{"role":"member"}"#).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn update_is_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = t.other_user("alice").await;

        let cases = [
            (user_uri(alice), "{}", StatusCode::BAD_REQUEST),
            (
                user_uri(alice),
                r#"{"role":"root"}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                user_uri(999_999),
                r#"{"disabled":true}"#,
                StatusCode::NOT_FOUND,
            ),
        ];

        for (uri, body, expected) in cases {
            let (status, _, _) = send_json(&t, Method::PATCH, &uri, &t.cookie, body).await;
            assert_eq!(status, expected, "{uri} {body}");
        }
    }

    // ==================== 重設密碼 ====================

    #[sqlx::test]
    async fn reset_password_logs_out_and_requires_change(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = user::create_user(&t.pool, "alice", "x").await.unwrap();
        let alice_cookie = session_for(&t, alice).await;

        let uri = format!("{}/reset-password", user_uri(alice));
        let (status, headers, body) = t
            .send(empty_request(Method::POST, &uri, Some(&t.cookie)))
            .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        let temporary = json(&body)["temporary_password"]
            .as_str()
            .unwrap()
            .to_string();

        // 所有裝置都被登出
        assert_eq!(me_status(&t, &alice_cookie).await, StatusCode::UNAUTHORIZED);
        // 新的臨時密碼可以登入，而且必須修改
        let (status, body) = login(&t, "alice", &temporary).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["must_change_password"], true);
    }

    #[sqlx::test]
    async fn reset_password_rules(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;

        // 不能重設自己的密碼
        let own = format!("{}/reset-password", user_uri(t.user_id));
        let (status, _, _) = t
            .send(empty_request(Method::POST, &own, Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // 不存在的使用者
        let missing = format!("{}/reset-password", user_uri(999_999));
        let (status, _, _) = t
            .send(empty_request(Method::POST, &missing, Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
