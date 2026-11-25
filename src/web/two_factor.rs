//! 兩步驟驗證（2FA）的設定。登入時的第二步在 auth.rs（與登入的其他部分放在一起）。
//!
//! 自己的設定（Settings → Sign-in）：
//!   GET  /api/me/two-factor                  狀態
//!   POST /api/me/two-factor/setup            開始設定：需要目前的密碼，回傳密鑰與 QR code
//!   POST /api/me/two-factor/enable           輸入驗證碼確認，啟用並回傳備用碼
//!   POST /api/me/two-factor/disable          關閉：需要密碼與驗證碼（或備用碼）
//!   POST /api/me/two-factor/recovery-codes   換一組新的備用碼：需要密碼
//!
//! 管理員（Administration）：
//!   GET  /api/admin/two-factor               是否強制、哪些人已經開啟
//!   PUT  /api/admin/two-factor               打開或關閉強制
//!   POST /api/admin/users/{id}/reset-two-factor   關閉別人的 2FA（手機遺失時）
//!
//! 自己的設定使用 SessionUser：管理員打開強制之後，還沒設定的人只能使用這幾個 API，
//! 必須先完成設定。必須修改臨時密碼的人則要先改密碼。
//!
//! 開始設定與換備用碼要求目前的密碼：離開座位沒鎖螢幕時，路過的人無法替你開啟 2FA
//! （把你鎖在外面）或看到你的備用碼。

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    auth::{start_session, unix_now, verify_password, verify_second_factor},
    error::{AppError, Forbidden},
    extract::{AdminUser, SessionUser},
    state::{AppState, CookieSecure},
};
use crate::{
    auth::totp,
    db::{two_factor, user},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/me/two-factor", get(my_status))
        .route("/api/me/two-factor/setup", post(setup))
        .route("/api/me/two-factor/enable", post(enable))
        .route("/api/me/two-factor/disable", post(disable))
        .route(
            "/api/me/two-factor/recovery-codes",
            post(new_recovery_codes),
        )
        .route("/api/admin/two-factor", get(admin_status).put(set_required))
        .route("/api/admin/users/{id}/reset-two-factor", post(admin_reset))
}

// ==================== 自己的狀態 ====================

#[derive(Serialize)]
struct MyStatus {
    enabled: bool,
    recovery_codes_left: i64,
    /// 管理員要求開啟：開啟之後不能自己關閉
    required: bool,
    /// 只有能用密碼登入的帳號可以設定；只用 SSO 的帳號由身分提供者負責
    available: bool,
}

/// GET /api/me/two-factor
async fn my_status(
    me: SessionUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<MyStatus>, AppError> {
    let status = two_factor::status(&pool, me.id).await?;
    Ok(Json(MyStatus {
        enabled: status.enabled,
        recovery_codes_left: status.recovery_codes_left,
        required: two_factor::require_two_factor(&pool).await?,
        available: user::find_password_hash(&pool, me.id).await?.is_some(),
    }))
}

// ==================== 設定 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordRequest {
    current_password: String,
}

#[derive(Serialize)]
struct SetupResponse {
    /// 手動輸入用的密鑰（Base32），前端每 4 個字元分一組顯示
    secret: String,
    /// QR code 的內容（otpauth://…）
    uri: String,
    qr: QrResponse,
}

#[derive(Serialize)]
struct QrResponse {
    size: usize,
    /// size × size 個 0 與 1，由上而下、由左而右；1 是黑點
    modules: String,
}

/// POST /api/me/two-factor/setup：開始設定，回傳新的密鑰。
/// 這時還沒有啟用：輸入正確的驗證碼確認之後才生效
async fn setup(
    me: SessionUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<PasswordRequest>,
) -> Result<Json<SetupResponse>, AppError> {
    reject_temporary_password(&me)?;
    check_password(&pool, me.id, req.current_password).await?;

    let secret = totp::generate_secret();
    if !two_factor::begin_setup(&pool, me.id, &secret).await? {
        return Err(AppError::Conflict(
            "Two-factor authentication is already on".to_string(),
        ));
    }
    let uri = totp::otpauth_uri(&me.username, &secret);
    let qr = totp::qr_matrix(&uri);
    Ok(Json(SetupResponse {
        secret: totp::base32(&secret),
        uri,
        qr: QrResponse {
            size: qr.size,
            modules: qr.modules,
        },
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeRequest {
    code: String,
}

#[derive(Serialize)]
struct RecoveryCodesResponse {
    /// 只在這次回應中出現；伺服器只存它們的雜湊
    recovery_codes: Vec<String>,
}

/// POST /api/me/two-factor/enable：輸入驗證器 App 顯示的驗證碼，確認之後啟用。
///
/// 啟用時登出這個帳號的所有裝置（與修改密碼相同），目前的裝置立刻建立新的 session
async fn enable(
    me: SessionUser,
    State(pool): State<SqlitePool>,
    cookie_secure: CookieSecure,
    jar: CookieJar,
    Json(req): Json<CodeRequest>,
) -> Result<(CookieJar, Json<RecoveryCodesResponse>), AppError> {
    reject_temporary_password(&me)?;
    let pending = two_factor::find_totp(&pool, me.id)
        .await?
        .filter(|credential| !credential.enabled)
        .ok_or_else(|| AppError::BadRequest("Start the setup again".to_string()))?;

    let step = totp::verify(
        &pending.secret,
        &req.code,
        unix_now(),
        pending.last_used_step,
    )
    .ok_or_else(|| {
        AppError::BadRequest(
            "The code isn't correct. Check that you scanned the QR code, then try the current code"
                .to_string(),
        )
    })?;

    let codes = totp::generate_recovery_codes();
    let hashes: Vec<Vec<u8>> = codes
        .iter()
        .map(|code| totp::hash_recovery_code(code))
        .collect();
    if !two_factor::enable(&pool, me.id, step, &hashes).await? {
        return Err(AppError::BadRequest("Start the setup again".to_string()));
    }

    let jar = start_session(&pool, jar, me.id, cookie_secure).await?;
    Ok((
        jar,
        Json(RecoveryCodesResponse {
            recovery_codes: codes,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisableRequest {
    current_password: String,
    /// 驗證碼或備用碼：證明手機（或備用碼）仍在自己手上
    code: String,
}

/// POST /api/me/two-factor/disable：關閉 2FA。管理員要求開啟時不能關閉
async fn disable(
    me: SessionUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<DisableRequest>,
) -> Result<StatusCode, AppError> {
    if two_factor::require_two_factor(&pool).await? {
        return Err(AppError::BadRequest(
            "Your administrator requires two-factor authentication".to_string(),
        ));
    }
    if !two_factor::status(&pool, me.id).await?.enabled {
        return Err(AppError::BadRequest(
            "Two-factor authentication is not on".to_string(),
        ));
    }
    check_password(&pool, me.id, req.current_password).await?;
    if !verify_second_factor(&pool, me.id, &me.username, &req.code).await? {
        return Err(AppError::BadRequest("The code isn't correct".to_string()));
    }
    two_factor::disable(&pool, me.id, false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/me/two-factor/recovery-codes：換一組新的備用碼，舊的全部作廢
async fn new_recovery_codes(
    me: SessionUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<PasswordRequest>,
) -> Result<Json<RecoveryCodesResponse>, AppError> {
    if !two_factor::status(&pool, me.id).await?.enabled {
        return Err(AppError::BadRequest(
            "Two-factor authentication is not on".to_string(),
        ));
    }
    check_password(&pool, me.id, req.current_password).await?;
    let codes = totp::generate_recovery_codes();
    let hashes: Vec<Vec<u8>> = codes
        .iter()
        .map(|code| totp::hash_recovery_code(code))
        .collect();
    two_factor::replace_recovery_codes(&pool, me.id, &hashes).await?;
    Ok(Json(RecoveryCodesResponse {
        recovery_codes: codes,
    }))
}

/// 必須修改臨時密碼的人要先改密碼：用臨時密碼設定 2FA 沒有意義
fn reject_temporary_password(me: &SessionUser) -> Result<(), AppError> {
    if me.must_change_password {
        return Err(AppError::Forbidden(Forbidden::PasswordChangeRequired));
    }
    Ok(())
}

/// 目前的密碼。沒有密碼的帳號（只用 SSO）一律失敗
async fn check_password(pool: &SqlitePool, user_id: i64, password: String) -> Result<(), AppError> {
    let stored = user::find_password_hash(pool, user_id).await?;
    if stored.is_none() {
        return Err(AppError::BadRequest(
            "Accounts that sign in with single sign-on use the provider's two-factor authentication"
                .to_string(),
        ));
    }
    if !verify_password(password, stored).await? {
        return Err(AppError::Forbidden(Forbidden::WrongCurrentPassword));
    }
    Ok(())
}

// ==================== 管理員 ====================

#[derive(Serialize)]
struct AdminStatus {
    require_two_factor: bool,
    /// 已經開啟 2FA 的使用者，Users 列表依此顯示
    enabled_user_ids: Vec<i64>,
}

/// GET /api/admin/two-factor
async fn admin_status(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<AdminStatus>, AppError> {
    let mut ids: Vec<i64> = two_factor::enabled_user_ids(&pool)
        .await?
        .into_iter()
        .collect();
    ids.sort_unstable();
    Ok(Json(AdminStatus {
        require_two_factor: two_factor::require_two_factor(&pool).await?,
        enabled_user_ids: ids,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequireRequest {
    require_two_factor: bool,
}

/// PUT /api/admin/two-factor：打開或關閉強制。
/// 打開之後，還沒設定的人（包括管理員自己）下一個請求就會被要求先完成設定
async fn set_required(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<RequireRequest>,
) -> Result<Json<AdminStatus>, AppError> {
    two_factor::set_require_two_factor(&pool, req.require_two_factor).await?;
    println!(
        "2fa: requirement turned {} by user {}",
        if req.require_two_factor { "on" } else { "off" },
        admin.id
    );
    admin_status(admin, State(pool)).await
}

/// POST /api/admin/users/{id}/reset-two-factor：關閉別人的 2FA，同時登出那個人的所有裝置。
/// 不能用在自己身上：自己的 2FA 要在 Settings 中用密碼與驗證碼關閉
async fn admin_reset(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    if id == admin.id {
        return Err(AppError::BadRequest(
            "Turn off your own two-factor authentication in Settings".to_string(),
        ));
    }
    if !two_factor::disable(&pool, id, true).await? {
        return Err(AppError::NotFound(
            "That user doesn't have two-factor authentication".to_string(),
        ));
    }
    println!(
        "2fa: turned off for user {id} by administrator {}",
        admin.id
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::{
        auth::SESSION_COOKIE,
        test_support::{MIB, TestApp, empty_request, request},
    };
    use crate::{auth, auth::totp, db::user};
    use axum::{
        body::{Body, Bytes},
        http::{HeaderMap, Method, StatusCode, header},
    };
    use serde_json::Value;
    use sqlx::SqlitePool;

    const PASSWORD: &str = "dave_password123";

    async fn send_json(
        t: &TestApp,
        method: Method,
        uri: &str,
        cookies: &[&str],
        body: Value,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let mut builder =
            request(method, uri, None).header(header::CONTENT_TYPE, "application/json");
        if !cookies.is_empty() {
            builder = builder.header(header::COOKIE, cookies.join("; "));
        }
        t.send(builder.body(Body::from(body.to_string())).unwrap())
            .await
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    }

    fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .find(|v| v.starts_with(&format!("{name}=")) && !v.starts_with(&format!("{name}=;")))
            .and_then(|v| v.split(';').next())
            .map(str::to_string)
    }

    /// 有真實密碼的 dave，回傳 (id, 登入後的 session Cookie)
    async fn dave(t: &TestApp) -> (i64, String) {
        let hash = auth::hash_password(PASSWORD).unwrap();
        let id = user::create_user(&t.pool, "dave", &hash).await.unwrap();
        let (status, headers, _) = login(t, PASSWORD).await;
        assert_eq!(status, StatusCode::OK);
        (id, cookie(&headers, SESSION_COOKIE).unwrap())
    }

    async fn login(t: &TestApp, password: &str) -> (StatusCode, HeaderMap, Bytes) {
        send_json(
            t,
            Method::POST,
            "/api/login",
            &[],
            serde_json::json!({ "username": "dave", "password": password }),
        )
        .await
    }

    /// 目前時間的驗證碼
    fn current_code(secret_base32: &str) -> String {
        let secret = decode_base32(secret_base32);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        format!("{:06}", totp::code_at(&secret, totp::step_at(now)))
    }

    /// 下一個時間段的驗證碼（同一個時間段的不能用兩次；下一個在容許的誤差內）
    fn next_code(secret_base32: &str) -> String {
        let secret = decode_base32(secret_base32);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        format!("{:06}", totp::code_at(&secret, totp::step_at(now) + 1))
    }

    fn decode_base32(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut out = Vec::new();
        let (mut buffer, mut bits) = (0u32, 0);
        for c in text.bytes() {
            let value = ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
            buffer = (buffer << 5) | value;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
            }
        }
        out
    }

    /// 完整的設定流程，回傳 (密鑰, 備用碼, 新的 session Cookie)
    async fn turn_on(t: &TestApp, session: &str) -> (String, Vec<String>, String) {
        let (status, _, body) = send_json(
            t,
            Method::POST,
            "/api/me/two-factor/setup",
            &[session],
            serde_json::json!({ "current_password": PASSWORD }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let secret = json(&body)["secret"].as_str().unwrap().to_string();

        let (status, headers, body) = send_json(
            t,
            Method::POST,
            "/api/me/two-factor/enable",
            &[session],
            serde_json::json!({ "code": current_code(&secret) }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let codes: Vec<String> = json(&body)["recovery_codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect();
        (secret, codes, cookie(&headers, SESSION_COOKIE).unwrap())
    }

    /// 登入的第一步，回傳等待中的登入的 Cookie
    async fn first_step(t: &TestApp) -> String {
        let (status, headers, body) = login(t, PASSWORD).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body),
            serde_json::json!({ "two_factor_required": true })
        );
        // 密碼正確但還沒有登入
        assert!(cookie(&headers, SESSION_COOKIE).is_none());
        cookie(&headers, "manorcloud_2fa").unwrap()
    }

    async fn second_step(t: &TestApp, pending: &str, code: &str) -> (StatusCode, HeaderMap, Bytes) {
        send_json(
            t,
            Method::POST,
            "/api/login/two-factor",
            &[pending],
            serde_json::json!({ "code": code }),
        )
        .await
    }

    #[sqlx::test]
    async fn turning_on_then_logging_in_takes_two_steps(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (_, session) = dave(&t).await;

        let (secret, codes, new_session) = turn_on(&t, &session).await;

        assert_eq!(codes.len(), 10);
        // 啟用時其他裝置被登出，目前的裝置換了新的 session
        let (status, _, _) = t
            .send(empty_request(Method::GET, "/api/me", Some(&session)))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = t
            .send(empty_request(Method::GET, "/api/me", Some(&new_session)))
            .await;
        assert_eq!(status, StatusCode::OK);

        // 登入：密碼之後要驗證碼
        let pending = first_step(&t).await;
        let (status, headers, body) = second_step(&t, &pending, &next_code(&secret)).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(json(&body)["username"], "dave");
        assert!(cookie(&headers, SESSION_COOKIE).is_some());
    }

    #[sqlx::test]
    async fn the_password_alone_gets_nothing(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (_, session) = dave(&t).await;
        let (secret, _, _) = turn_on(&t, &session).await;
        let pending = first_step(&t).await;

        // 等待中的登入不是 session
        let (status, _, _) = t
            .send(empty_request(Method::GET, "/api/me", Some(&pending)))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // 沒有等待中的登入就送驗證碼
        let (status, _, _) = second_step(&t, "manorcloud_2fa=forged", &next_code(&secret)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // 錯的驗證碼可以再試；猜錯 5 次之後要從密碼重新開始
        for attempt in 1..=5 {
            let (status, _, _) = second_step(&t, &pending, "000000").await;
            let expected = if attempt < 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNAUTHORIZED
            };
            assert_eq!(status, expected, "{attempt}");
        }
        let (status, _, _) = second_step(&t, &pending, &next_code(&secret)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn codes_and_recovery_codes_work_only_once(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (_, session) = dave(&t).await;
        let (secret, codes, _) = turn_on(&t, &session).await;
        let code = next_code(&secret);

        let pending = first_step(&t).await;
        assert_eq!(second_step(&t, &pending, &code).await.0, StatusCode::OK);
        // 同一個驗證碼：第二次不行
        let pending = first_step(&t).await;
        assert_eq!(
            second_step(&t, &pending, &code).await.0,
            StatusCode::BAD_REQUEST
        );

        // 備用碼可以代替驗證碼，大小寫與連字號不影響；也只能用一次
        let typed = codes[0].to_uppercase().replace('-', "");
        assert_eq!(second_step(&t, &pending, &typed).await.0, StatusCode::OK);
        let pending = first_step(&t).await;
        assert_eq!(
            second_step(&t, &pending, &codes[0]).await.0,
            StatusCode::BAD_REQUEST
        );
        // 同一個等待中的登入用過之後就作廢
        assert_eq!(second_step(&t, &pending, &codes[1]).await.0, StatusCode::OK);
        assert_eq!(
            second_step(&t, &pending, &codes[2]).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[sqlx::test]
    async fn setup_needs_the_password_and_a_correct_code(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, session) = dave(&t).await;

        let (status, _, _) = send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/setup",
            &[&session],
            serde_json::json!({ "current_password": "wrong_password" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/setup",
            &[&session],
            serde_json::json!({ "current_password": PASSWORD }),
        )
        .await;
        let (status, _, _) = send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/enable",
            &[&session],
            serde_json::json!({ "code": "123456" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // 確認失敗：還沒有啟用，登入仍然只要密碼
        assert!(
            !crate::db::two_factor::status(&t.pool, id)
                .await
                .unwrap()
                .enabled
        );
        assert_eq!(login(&t, PASSWORD).await.0, StatusCode::OK);
    }

    #[sqlx::test]
    async fn turning_off_needs_the_password_and_a_code(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (_, session) = dave(&t).await;
        let (secret, _, session) = turn_on(&t, &session).await;

        let disable = |password: &str, code: String| serde_json::json!({ "current_password": password, "code": code });
        let (status, _, _) = send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/disable",
            &[&session],
            disable(PASSWORD, "000000".to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _, _) = send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/disable",
            &[&session],
            disable(PASSWORD, next_code(&secret)),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        // 關閉之後，登入只要密碼
        let (_, _, body) = login(&t, PASSWORD).await;
        assert_eq!(json(&body)["username"], "dave");
    }

    #[sqlx::test]
    async fn the_requirement_blocks_everything_until_setup_is_done(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (_, session) = dave(&t).await;
        crate::db::two_factor::set_require_two_factor(&t.pool, true)
            .await
            .unwrap();

        // 一般 API 被擋下，/api/me 告訴前端要先設定
        let (status, _, body) = t
            .send(empty_request(Method::GET, "/api/files", Some(&session)))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(json(&body)["code"], "two_factor_setup_required");
        let (_, _, body) = t
            .send(empty_request(Method::GET, "/api/me", Some(&session)))
            .await;
        assert_eq!(json(&body)["two_factor_setup_required"], true);

        // 完成設定之後恢復；強制的時候不能自己關閉
        let (secret, _, session) = turn_on(&t, &session).await;
        let (status, _, _) = t
            .send(empty_request(Method::GET, "/api/files", Some(&session)))
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = send_json(
            &t,
            Method::POST,
            "/api/me/two-factor/disable",
            &[&session],
            serde_json::json!({ "current_password": PASSWORD, "code": next_code(&secret) }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn an_administrator_can_turn_off_someone_elses_two_factor(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (dave_id, session) = dave(&t).await;
        let (_, _, dave_session) = turn_on(&t, &session).await;
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();

        let (_, _, body) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/two-factor",
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(
            json(&body)["enabled_user_ids"],
            serde_json::json!([dave_id])
        );

        let uri = format!("/api/admin/users/{dave_id}/reset-two-factor");
        let (status, _, _) = t
            .send(empty_request(Method::POST, &uri, Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        // dave 的所有裝置都被登出；之後登入只要密碼
        let (status, _, _) = t
            .send(empty_request(Method::GET, "/api/me", Some(&dave_session)))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json(&login(&t, PASSWORD).await.2)["username"], "dave");

        // 自己的不能用這個方式關閉；一般成員不能使用
        let own = format!("/api/admin/users/{}/reset-two-factor", t.user_id);
        let (status, _, _) = t
            .send(empty_request(Method::POST, &own, Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, dave_session) = (
            0,
            cookie(&login(&t, PASSWORD).await.1, SESSION_COOKIE).unwrap(),
        );
        let (status, _, _) = t
            .send(empty_request(Method::POST, &uri, Some(&dave_session)))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
