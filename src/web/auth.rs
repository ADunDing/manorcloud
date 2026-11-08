//! 登入、登出，以及使用者修改自己的帳號名稱與密碼。
//!
//! 沒有公開的註冊：帳號由管理員建立，第一個管理員在伺服器啟動時自動建立。

use std::net::{IpAddr, SocketAddr};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use super::{
    error::{AppError, Forbidden},
    extract::SessionUser,
    state::{AppState, CookieSecure, LoginLimits},
};
use crate::{
    auth::{self, limiter::Outcome},
    db::{
        session,
        user::{self, UpdateCredentialsError},
    },
};

const USERNAME_MIN: usize = 3;
const USERNAME_MAX: usize = 32;
const PASSWORD_MIN: usize = 8;
const PASSWORD_MAX: usize = 128;

/// Session Cookie 的名稱（身分驗證 extractor 也會用到）
pub const SESSION_COOKIE: &str = "manorcloud_session";
/// Session Cookie 的路徑；設定與清除時必須一致，否則瀏覽器視為不同 Cookie
const COOKIE_PATH: &str = "/";
/// Session 有效天數
const SESSION_DAYS: i64 = 30;

/// 登入失敗時統一的訊息：不透露是帳號不存在、沒有密碼，還是密碼錯誤
const LOGIN_FAILED: &str = "帳號或密碼錯誤";

/// 被鎖定時的錯誤代碼，前端依此顯示「請 X 分鐘後再試」
const TOO_MANY_ATTEMPTS: &str = "too_many_attempts";

/// 同時最多幾個 Argon2 運算。每個約使用 19 MiB 記憶體並佔滿一個 CPU 核心：
/// 不限制的話，大量同時的請求可以耗盡樹莓派的記憶體。超過的請求排隊等待。
/// 登入、修改密碼、管理員建立帳號都經過下方的 verify_password 與 hash_password。
static ARGON2_PERMITS: Semaphore = Semaphore::const_new(2);

// ==================== 登入的錯誤 ====================

/// 登入的錯誤：一般的 AppError，或因失敗次數過多被暫時鎖定。
///
/// 429 需要額外的 Retry-After 標頭與剩餘秒數，所以不放進所有 handler 共用的 AppError。
enum LoginError {
    App(AppError),
    Locked(Duration),
}

/// 讓 handler 中回傳 AppError 的 `?` 與 `.into()` 不必修改
impl From<AppError> for LoginError {
    fn from(e: AppError) -> Self {
        LoginError::App(e)
    }
}

/// `?` 只做一次轉換，不會串接 sqlx::Error → AppError → LoginError，
/// 所以資料庫錯誤需要直接轉換（交給 AppError 處理：記錄細節、對外回傳通用訊息）
impl From<sqlx::Error> for LoginError {
    fn from(e: sqlx::Error) -> Self {
        LoginError::App(e.into())
    }
}

#[derive(Serialize)]
struct LockedBody {
    error: String,
    code: &'static str,
    /// 還要等幾秒，與 Retry-After 標頭相同；放在內容中，前端不必另外讀標頭
    retry_after: u64,
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        match self {
            LoginError::App(e) => e.into_response(),
            LoginError::Locked(wait) => {
                let seconds = retry_after_secs(wait);
                let body = LockedBody {
                    error: format!("登入失敗次數過多，請 {} 分鐘後再試", seconds.div_ceil(60)),
                    code: TOO_MANY_ATTEMPTS,
                    retry_after: seconds,
                };
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, seconds.to_string())],
                    Json(body),
                )
                    .into_response()
            }
        }
    }
}

/// 剩餘時間換成整數秒，無條件進位，至少 1 秒：
/// 0.3 秒時回答「0 秒後再試」，用戶端立刻重試只會再被擋一次
fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

// ==================== 登入名額 ====================

/// 一次登入嘗試佔用的名額（RAII）。
///
/// 建立時佔用 IP 與帳號各一個名額，Drop 時一定歸還並記錄結果：
/// 不論是正常結束、提早 return，還是使用者斷線導致 future 被丟棄。
/// 與 TempBlob 在 Drop 中刪除暫存檔是同一個模式。
///
/// 沒有標記結果就被丟棄時算一次失敗：否則攻擊者可以送出請求、在回應前斷線，
/// 讓猜測不被計數。正常使用者登入到一半關掉分頁，多算一次失敗也無傷大雅。
struct LoginAttempt {
    limits: LoginLimits,
    ip: IpAddr,
    username: String,
    outcome: Outcome,
}

impl LoginAttempt {
    /// 佔用名額；被鎖定或名額已滿時回傳還要等多久
    fn begin(limits: &LoginLimits, ip: IpAddr, username: &str) -> Result<Self, Duration> {
        // 先存進 let：鎖在這一行結束時就釋放
        let allowed = limits.lock().begin(ip, username, Instant::now());
        allowed.map(|()| LoginAttempt {
            limits: limits.clone(),
            ip,
            username: username.to_string(),
            outcome: Outcome::Failure,
        })
    }

    /// 密碼正確：之後不論帳號停用或建立 session 失敗，都不算猜錯
    fn password_correct(&mut self) {
        self.outcome = Outcome::Neither;
    }

    /// 登入成功：清除帳號的失敗計數
    fn succeeded(&mut self) {
        self.outcome = Outcome::Success;
    }
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        let lockouts =
            self.limits
                .lock()
                .end(self.ip, &self.username, self.outcome, Instant::now());
        log_lockouts(self.ip, &self.username, &lockouts);
    }
}

/// 新的鎖定在日誌中記一行，讓管理員發現有人在猜密碼
fn log_lockouts(ip: IpAddr, username: &str, lockouts: &auth::limiter::NewLockouts) {
    let minutes = auth::limiter::LOCKOUT.as_secs() / 60;
    if lockouts.ip {
        eprintln!("login: too many failed attempts from {ip}; blocked for {minutes} minutes");
    }
    if lockouts.user {
        // 帳號名稱來自請求：用 {:?} 印出，換行等控制字元會被跳脫，無法偽造日誌的內容
        let name: String = username.chars().take(64).collect();
        eprintln!(
            "login: too many failed attempts for account {name:?}; blocked for {minutes} minutes"
        );
    }
}

// ==================== 假雜湊與路由 ====================

/// 帳號不存在時用來「陪跑」Argon2 的假雜湊，讓回應時間與密碼錯誤時一致。
/// 由 `warm_up` 在伺服器啟動時預先產生。
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    auth::hash_password("manorcloud-dummy-password").expect("failed to generate the dummy hash")
});

/// 伺服器啟動時呼叫：預先產生假雜湊，避免第一個請求因初始化而變慢、洩漏時間差
pub async fn warm_up() {
    tokio::task::spawn_blocking(|| LazyLock::force(&DUMMY_HASH))
        .await
        .expect("failed to warm up the dummy hash");
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/me/credentials", post(update_credentials))
}

// ==================== 共用回應 ====================

/// 登入、/api/me、修改憑證的回應。前端依 must_change_password 決定是否顯示修改畫面，
/// 依 role 決定是否顯示管理入口（真正的權限檢查仍在後端）
#[derive(Serialize)]
struct UserResponse {
    id: i64,
    username: String,
    role: String,
    must_change_password: bool,
}

// ==================== 建立 session ====================

/// 建立 session 的唯一入口：檢查帳號狀態、建立 session、把 Cookie 加進 jar。
///
/// 呼叫前必須已經驗證過身分（密碼；將來還有 SSO）。停用的檢查放在這裡而不是各個登入方式裡，
/// 將來新增的登入方式呼叫同一個函式，就不可能漏掉。
///
/// 檢查與建立之間若帳號剛好被停用，建立出來的 session 也無法使用：
/// 查詢 session 時會過濾掉停用的使用者。
pub(super) async fn start_session(
    pool: &SqlitePool,
    jar: CookieJar,
    user_id: i64,
    CookieSecure(secure): CookieSecure,
) -> Result<CookieJar, AppError> {
    match user::is_disabled(pool, user_id).await? {
        // 驗證身分之後帳號被刪除了
        None => return Err(AppError::Unauthorized(LOGIN_FAILED.to_string())),
        // 只有通過身分驗證的人才會看到這個訊息
        Some(true) => return Err(AppError::Forbidden(Forbidden::AccountDisabled)),
        Some(false) => {}
    }

    // 資料庫只存 token 的雜湊，原始 token 只交給瀏覽器
    let token = auth::session::generate_token();
    let token_hash = auth::session::hash_token(&token);
    let expires_at = unix_now() + SESSION_DAYS * 24 * 60 * 60;
    session::create_session(pool, user_id, &token_hash, expires_at).await?;

    let cookie = Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .same_site(SameSite::Lax)
        // 只在設定為 HTTPS 時開啟：http 連線中，瀏覽器會丟掉 Secure 的 Cookie
        .secure(secure)
        .path(COOKIE_PATH)
        .max_age(time::Duration::days(SESSION_DAYS));

    Ok(jar.add(cookie))
}

// ==================== 登入 ====================

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// POST /api/login：驗證帳密，成功後建立 session 並設定 Cookie。
///
/// 同一個 IP 或同一個帳號失敗太多次時暫時鎖定，回傳 429 與 Retry-After（見 auth::limiter）。
async fn login(
    State(pool): State<SqlitePool>,
    State(cookie_secure): State<CookieSecure>,
    State(limits): State<LoginLimits>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> Result<(CookieJar, Json<UserResponse>), LoginError> {
    // 0. 佔用名額。被鎖定、或名額已被進行中的嘗試佔滿時直接拒絕：
    //    不查資料庫，也不花 Argon2 的運算。
    //    從這裡起，任何提早 return 都會在 attempt 被丟棄時記為一次失敗
    let mut attempt = match LoginAttempt::begin(&limits, peer.ip(), &req.username) {
        Ok(attempt) => attempt,
        Err(wait) => return Err(LoginError::Locked(wait)),
    };

    // 1. 密碼過長直接拒絕，避免把超大輸入交給 Argon2。這也是一次猜測，照樣計數
    if req.password.chars().count() > PASSWORD_MAX {
        return Err(AppError::Unauthorized(LOGIN_FAILED.to_string()).into());
    }

    // 2. 查詢密碼登入的資料。帳號不存在、或帳號沒有設定密碼，都是 None，
    //    兩者都走「陪跑」分支，回應時間與訊息完全相同
    let found = user::find_password_login(&pool, &req.username).await?;
    let stored_hash = found.as_ref().map(|login| login.password_hash.clone());

    // 3. 驗證密碼。帳號不存在、密碼錯誤、帳號停用但密碼錯誤：三者無法區分，都計數
    let password_ok = verify_password(req.password, stored_hash).await?;
    let Some(login) = found.filter(|_| password_ok) else {
        return Err(AppError::Unauthorized(LOGIN_FAILED.to_string()).into());
    };
    attempt.password_correct();

    // 4. 建立 session。停用的檢查在 start_session 裡：
    //    停用帳號的正確密碼不算猜錯，也沒有登入成功，所以不計數、也不清除計數
    let jar = start_session(&pool, jar, login.user_id, cookie_secure).await?;

    // 5. 登入成功：attempt 在函式結束時被丟棄，清除這個帳號的失敗計數。
    //    IP 的計數刻意不清除，理由見 LoginLimiter::end
    attempt.succeeded();

    Ok((
        jar,
        Json(UserResponse {
            id: login.user_id,
            username: login.username,
            role: login.role,
            must_change_password: login.must_change_password,
        }),
    ))
}

// ==================== 登出 ====================

/// POST /api/logout：刪除 session 並清除 Cookie。
/// 冪等操作：沒帶 Cookie 或 session 已不存在時同樣回傳成功。
/// 不使用 extractor：必須修改密碼的使用者也要能登出。
async fn logout(
    State(pool): State<SqlitePool>,
    State(CookieSecure(secure)): State<CookieSecure>,
    jar: CookieJar,
) -> Result<(StatusCode, CookieJar), AppError> {
    // 1. 有帶 Cookie 才需要刪資料庫；刪不到東西也不算錯誤
    if let Some(cookie) = jar.get(SESSION_COOKIE) {
        let token_hash = auth::session::hash_token(cookie.value());
        session::delete_session(&pool, &token_hash).await?;
    }

    // 2. 清除 Cookie：名稱與 Path 必須與設定時相同；Secure 也保持一致，
    //    避免瀏覽器把清除指令當成另一個 Cookie
    let jar = jar.remove(
        Cookie::build(SESSION_COOKIE)
            .path(COOKIE_PATH)
            .secure(secure),
    );

    Ok((StatusCode::NO_CONTENT, jar))
}

// ==================== 目前使用者 ====================

/// GET /api/me：回傳目前登入的使用者。
///
/// 使用 SessionUser：必須修改密碼的使用者也要能呼叫，
/// 前端重新整理頁面時才知道要顯示修改畫面。
async fn me(me: SessionUser) -> Json<UserResponse> {
    Json(UserResponse {
        id: me.id,
        username: me.username,
        role: me.role,
        must_change_password: me.must_change_password,
    })
}

// ==================== 修改自己的憑證 ====================

/// 打錯欄位名稱時直接回錯誤，而不是被默默忽略
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialsRequest {
    current_password: String,
    new_username: Option<String>,
    new_password: Option<String>,
}

/// POST /api/me/credentials：修改自己的帳號名稱及／或密碼。
///
/// 一律要求目前的密碼（包括首次修改）：離開座位沒鎖螢幕時，路過的人只拿到一個已登入的分頁，
/// 不知道密碼就無法改掉密碼或帳號名稱，把主人鎖在外面。
///
/// 改了密碼時，所有裝置都會被登出，目前的裝置立刻建立新的 session，保持登入。
async fn update_credentials(
    me: SessionUser,
    State(pool): State<SqlitePool>,
    State(cookie_secure): State<CookieSecure>,
    jar: CookieJar,
    Json(req): Json<CredentialsRequest>,
) -> Result<(CookieJar, Json<UserResponse>), AppError> {
    // 1. 便宜的檢查先做，不合法的請求不必跑 Argon2
    if req.new_username.is_none() && req.new_password.is_none() {
        return Err(AppError::BadRequest("沒有要修改的內容".to_string()));
    }
    // 只改名稱就清除旗標的話，log 中的臨時密碼會繼續有效
    if me.must_change_password && req.new_password.is_none() {
        return Err(AppError::BadRequest("必須設定新的密碼".to_string()));
    }
    if let Some(username) = &req.new_username {
        validate_username(username)?;
    }
    if let Some(password) = &req.new_password {
        validate_password(password)?;
        // 請求中本來就有兩者的明文，直接比較字串，不需要額外的 Argon2
        if *password == req.current_password {
            return Err(AppError::BadRequest(
                "新密碼不能與目前的密碼相同".to_string(),
            ));
        }
    }
    if req.current_password.chars().count() > PASSWORD_MAX {
        return Err(AppError::Forbidden(Forbidden::WrongCurrentPassword));
    }

    // 2. 驗證目前的密碼。沒有設定密碼的帳號（將來只用 SSO）一律失敗
    let stored_hash = user::find_password_hash(&pool, me.id).await?;
    if !verify_password(req.current_password, stored_hash).await? {
        // 403 而不是 401：前端不會把它誤判為「登入已過期」
        return Err(AppError::Forbidden(Forbidden::WrongCurrentPassword));
    }

    // 3. 雜湊新密碼
    let new_hash = match req.new_password {
        Some(password) => Some(hash_password(password).await?),
        None => None,
    };

    // 4. 在同一個交易中更新；改了密碼時會刪除這個使用者的所有 session
    user::update_credentials(
        &pool,
        me.id,
        req.new_username.as_deref(),
        new_hash.as_deref(),
    )
    .await
    .map_err(|e| match e {
        UpdateCredentialsError::UsernameTaken => AppError::Conflict("帳號已被使用".to_string()),
        UpdateCredentialsError::Db(e) => AppError::internal(e),
    })?;

    // 5. 改了密碼：舊的 session 都已刪除，為目前的裝置建立新的，保持登入
    let jar = if new_hash.is_some() {
        start_session(&pool, jar, me.id, cookie_secure).await?
    } else {
        jar
    };

    Ok((
        jar,
        Json(UserResponse {
            id: me.id,
            username: req.new_username.unwrap_or(me.username),
            role: me.role,
            // 必須修改時一定有新密碼（步驟 1 已檢查），完成後旗標已清除
            must_change_password: me.must_change_password && new_hash.is_none(),
        }),
    ))
}

// ==================== 輔助函式 ====================

/// 驗證密碼。沒有可比對的雜湊時，以假雜湊「陪跑」一次並回傳 false，讓各種失敗耗時相同。
/// Argon2 是重 CPU 運算，放到專用執行緒；同時進行的數量受 ARGON2_PERMITS 限制。
async fn verify_password(password: String, stored_hash: Option<String>) -> Result<bool, AppError> {
    let permit = ARGON2_PERMITS.acquire().await.map_err(AppError::internal)?;
    tokio::task::spawn_blocking(move || {
        // 名額跟著運算走：運算結束時才歸還，即使 handler 的 future 已經被丟棄
        let _permit = permit;
        match stored_hash {
            Some(hash) => auth::verify_password(&password, &hash),
            None => {
                auth::verify_password(&password, &DUMMY_HASH);
                false
            }
        }
    })
    .await
    .map_err(AppError::internal)
}

/// 雜湊新密碼，放到專用執行緒；同時進行的數量受 ARGON2_PERMITS 限制
pub(super) async fn hash_password(password: String) -> Result<String, AppError> {
    let permit = ARGON2_PERMITS.acquire().await.map_err(AppError::internal)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        auth::hash_password(&password)
    })
    .await
    .map_err(AppError::internal)?
    .map_err(AppError::internal)
}

/// 目前的 Unix 秒數；系統時間異常時回傳 0（產生的 session 會立即過期，屬安全的失敗）
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 判斷資料庫錯誤是否為 UNIQUE 約束衝突
pub(super) fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|db_err| db_err.is_unique_violation())
}

/// 帳號規則：3～32 字元，只允許小寫英文、數字、底線、連字號
pub(super) fn validate_username(username: &str) -> Result<(), AppError> {
    let allowed = username
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');

    if !allowed {
        return Err(AppError::BadRequest(
            "帳號只能包含小寫英文、數字、底線與連字號".to_string(),
        ));
    }

    // 已確認全為 ASCII，所以 bytes 長度就等於字元數
    if !(USERNAME_MIN..=USERNAME_MAX).contains(&username.len()) {
        return Err(AppError::BadRequest(format!(
            "帳號長度需為 {USERNAME_MIN}～{USERNAME_MAX} 個字元"
        )));
    }

    Ok(())
}

/// 密碼規則：8～128 個字元（以字元計算，中文也算一個字）
fn validate_password(password: &str) -> Result<(), AppError> {
    let len = password.chars().count();

    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&len) {
        return Err(AppError::BadRequest(format!(
            "密碼長度需為 {PASSWORD_MIN}～{PASSWORD_MAX} 個字元"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request, request};
    use super::*;
    use auth::limiter::{LOCKOUT, MAX_IP_FAILURES};
    use axum::{
        body::{Body, Bytes},
        http::{HeaderMap, Method},
    };
    use serde_json::Value;
    use tower::ServiceExt;

    // ==================== 驗證規則 ====================

    #[test]
    fn valid_usernames() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("bob_01").is_ok());
        assert!(validate_username("a-b").is_ok());
    }

    #[test]
    fn invalid_usernames() {
        assert!(validate_username("ab").is_err());
        assert!(validate_username(&"a".repeat(33)).is_err());
        assert!(validate_username("Alice").is_err());
        assert!(validate_username("bob smith").is_err());
        assert!(validate_username("小明明").is_err());
        assert!(validate_username("").is_err());
    }

    #[test]
    fn valid_passwords() {
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password(&"a".repeat(128)).is_ok());
        assert!(validate_password("我的密碼很安全喔").is_ok());
    }

    #[test]
    fn invalid_passwords() {
        assert!(validate_password("1234567").is_err());
        assert!(validate_password(&"a".repeat(129)).is_err());
    }

    #[test]
    fn unknown_error_is_not_unique_violation() {
        assert!(!is_unique_violation(&sqlx::Error::RowNotFound));
    }

    #[test]
    fn dummy_hash_never_matches() {
        assert!(!auth::verify_password("password123", &DUMMY_HASH));
    }

    #[test]
    fn retry_after_rounds_up_and_is_at_least_one_second() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
        assert_eq!(retry_after_secs(Duration::from_secs(60)), 60);
        assert_eq!(retry_after_secs(Duration::from_millis(60_001)), 61);
    }

    // ==================== 測試輔助 ====================
    //
    // 這些測試會執行真正的 Argon2，debug 模式下較慢，屬於正常現象。

    const DAVE_PASSWORD: &str = "dave_password123";
    const NEW_PASSWORD: &str = "a_brand_new_password";

    /// 建立一個有真實密碼雜湊的使用者 dave（TestApp 內建的 carol 用的是假雜湊，無法登入）
    async fn create_dave(t: &TestApp) -> i64 {
        let hash = auth::hash_password(DAVE_PASSWORD).unwrap();
        user::create_user(&t.pool, "dave", &hash).await.unwrap()
    }

    async fn require_change(t: &TestApp, id: i64) {
        sqlx::query("UPDATE password_credentials SET must_change_password = 1 WHERE user_id = ?")
            .bind(id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    async fn disable(t: &TestApp, id: i64) {
        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?")
            .bind(id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    fn json_request(uri: &str, cookie: Option<&str>, body: &str) -> axum::http::Request<Body> {
        request(Method::POST, uri, cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn post_json(
        t: &TestApp,
        uri: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        t.send(json_request(uri, cookie, body)).await
    }

    async fn login(t: &TestApp, username: &str, password: &str) -> (StatusCode, HeaderMap, Bytes) {
        let body = format!(r#"{{"username":"{username}","password":"{password}"}}"#);
        post_json(t, "/api/login", None, &body).await
    }

    /// 從回應的 Set-Cookie 取出可以放進 Cookie header 的 session 值
    fn session_cookie(headers: &HeaderMap) -> String {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .find(|v| v.starts_with(SESSION_COOKIE))
            .and_then(|v| v.split(';').next())
            .unwrap()
            .to_string()
    }

    async fn login_cookie(t: &TestApp, username: &str, password: &str) -> String {
        let (status, headers, _) = login(t, username, password).await;
        assert_eq!(status, StatusCode::OK);
        session_cookie(&headers)
    }

    async fn status_of(t: &TestApp, uri: &str, cookie: &str) -> StatusCode {
        t.send(empty_request(Method::GET, uri, Some(cookie)))
            .await
            .0
    }

    async fn change(t: &TestApp, cookie: &str, body: &str) -> (StatusCode, HeaderMap, Bytes) {
        post_json(t, "/api/me/credentials", Some(cookie), body).await
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    // ==================== 登入 ====================

    #[sqlx::test]
    async fn active_user_can_log_in(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        create_dave(&t).await;

        let (status, _, body) = login(&t, "dave", DAVE_PASSWORD).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["must_change_password"], false);
        assert_eq!(json(&body)["role"], "member");
    }

    #[sqlx::test]
    async fn disabled_user_with_correct_password_is_told(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        disable(&t, dave).await;

        let (status, _, body) = login(&t, "dave", DAVE_PASSWORD).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&body).contains("停用"));
        // 沒有建立任何 session：只有 TestApp 為 carol 建立的那一筆
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert_eq!(sessions, 1);
    }

    #[sqlx::test]
    async fn disabled_user_with_wrong_password_looks_like_any_failure(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        disable(&t, dave).await;

        let (status_a, _, body_a) = login(&t, "dave", "wrong_password").await;
        let (status_b, _, body_b) = login(&t, "nobody", "wrong_password").await;

        // 密碼錯誤時，看不出帳號是否存在、是否被停用
        assert_eq!(status_a, StatusCode::UNAUTHORIZED);
        assert_eq!((status_a, body_a), (status_b, body_b));
    }

    // ==================== 強制修改密碼 ====================

    #[sqlx::test]
    async fn must_change_user_can_only_reach_me(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        require_change(&t, dave).await;

        let (status, headers, body) = login(&t, "dave", DAVE_PASSWORD).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["must_change_password"], true);
        let cookie = session_cookie(&headers);

        // 一般 API 被擋下
        assert_eq!(
            status_of(&t, "/api/files", &cookie).await,
            StatusCode::FORBIDDEN
        );
        // /api/me 可以使用，並告訴前端要先修改
        let (status, _, body) = t
            .send(empty_request(Method::GET, "/api/me", Some(&cookie)))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["must_change_password"], true);
    }

    // ==================== 修改憑證：驗證 ====================

    #[sqlx::test]
    async fn credentials_requests_are_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        require_change(&t, dave).await;
        let cookie = login_cookie(&t, "dave", DAVE_PASSWORD).await;

        let cases = [
            // 目前的密碼錯誤
            (
                format!(r#"{{"current_password":"wrong","new_password":"{NEW_PASSWORD}"}}"#),
                StatusCode::FORBIDDEN,
            ),
            // 沒有要修改的內容
            (
                format!(r#"{{"current_password":"{DAVE_PASSWORD}"}}"#),
                StatusCode::BAD_REQUEST,
            ),
            // 必須修改密碼時，只改名稱不行
            (
                format!(r#"{{"current_password":"{DAVE_PASSWORD}","new_username":"david"}}"#),
                StatusCode::BAD_REQUEST,
            ),
            // 新舊密碼相同
            (
                format!(
                    r#"{{"current_password":"{DAVE_PASSWORD}","new_password":"{DAVE_PASSWORD}"}}"#
                ),
                StatusCode::BAD_REQUEST,
            ),
            // 新密碼太短
            (
                format!(r#"{{"current_password":"{DAVE_PASSWORD}","new_password":"short"}}"#),
                StatusCode::BAD_REQUEST,
            ),
            // 打錯欄位名稱
            (
                format!(
                    r#"{{"current_password":"{DAVE_PASSWORD}","newPassword":"{NEW_PASSWORD}"}}"#
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];

        for (body, expected) in cases {
            let (status, _, _) = change(&t, &cookie, &body).await;
            assert_eq!(status, expected, "{body}");
        }

        // 全部失敗之後，仍然處於必須修改的狀態
        assert_eq!(
            status_of(&t, "/api/files", &cookie).await,
            StatusCode::FORBIDDEN
        );
    }

    #[sqlx::test]
    async fn taken_username_is_409(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        create_dave(&t).await;
        let cookie = login_cookie(&t, "dave", DAVE_PASSWORD).await;

        // carol 是 TestApp 建立的使用者
        let body = format!(r#"{{"current_password":"{DAVE_PASSWORD}","new_username":"carol"}}"#);
        let (status, _, _) = change(&t, &cookie, &body).await;

        assert_eq!(status, StatusCode::CONFLICT);
    }

    // ==================== 修改憑證：成功 ====================

    #[sqlx::test]
    async fn changing_password_completes_claim_and_logs_out_other_devices(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        require_change(&t, dave).await;
        // 兩個裝置都登入了
        let this_device = login_cookie(&t, "dave", DAVE_PASSWORD).await;
        let other_device = login_cookie(&t, "dave", DAVE_PASSWORD).await;

        let body = format!(
            r#"{{"current_password":"{DAVE_PASSWORD}","new_username":"david","new_password":"{NEW_PASSWORD}"}}"#
        );
        let (status, headers, body) = change(&t, &this_device, &body).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["must_change_password"], false);
        assert_eq!(json(&body)["username"], "david");

        // 目前的裝置拿到新的 Cookie，一般 API 恢復可用
        let new_cookie = session_cookie(&headers);
        assert_eq!(
            status_of(&t, "/api/files", &new_cookie).await,
            StatusCode::OK
        );
        // 舊的 Cookie（兩個裝置的）都已失效
        assert_eq!(
            status_of(&t, "/api/me", &this_device).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_of(&t, "/api/me", &other_device).await,
            StatusCode::UNAUTHORIZED
        );
        // 新名稱與新密碼可以登入；舊的都不行
        assert_eq!(login(&t, "david", NEW_PASSWORD).await.0, StatusCode::OK);
        assert_eq!(
            login(&t, "david", DAVE_PASSWORD).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            login(&t, "dave", NEW_PASSWORD).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[sqlx::test]
    async fn renaming_only_keeps_current_session(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        create_dave(&t).await;
        let cookie = login_cookie(&t, "dave", DAVE_PASSWORD).await;

        let body = format!(r#"{{"current_password":"{DAVE_PASSWORD}","new_username":"david"}}"#);
        let (status, headers, _) = change(&t, &cookie, &body).await;

        assert_eq!(status, StatusCode::OK);
        // 沒有改密碼：不發新的 Cookie，原本的 session 繼續有效，而且顯示新名稱
        assert!(headers.get(header::SET_COOKIE).is_none());
        let (status, _, body) = t
            .send(empty_request(Method::GET, "/api/me", Some(&cookie)))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["username"], "david");
    }

    // ==================== 登入次數限制 ====================

    #[sqlx::test]
    async fn too_many_failures_lock_the_login(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        create_dave(&t).await;

        // 測試中所有請求都來自同一個 IP：先達到 IP 的上限
        for _ in 0..MAX_IP_FAILURES {
            let (status, _, _) = login(&t, "dave", "wrong_password").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        // 鎖定之後，連正確的密碼也被拒絕：檢查發生在驗證密碼之前
        let (status, headers, body) = login(&t, "dave", DAVE_PASSWORD).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        let retry_after: u64 = headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=LOCKOUT.as_secs()).contains(&retry_after));
        assert_eq!(json(&body)["code"], "too_many_attempts");
        assert_eq!(json(&body)["retry_after"], retry_after);
    }

    #[sqlx::test]
    async fn concurrent_guesses_cannot_exceed_the_limit(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        create_dave(&t).await;
        const ATTEMPTS: usize = 20;

        // 同時送出 20 個錯誤的密碼
        let tasks: Vec<_> = (0..ATTEMPTS)
            .map(|_| {
                let app = t.app.clone();
                let req = json_request(
                    "/api/login",
                    None,
                    r#"{"username":"dave","password":"wrong_password"}"#,
                );
                tokio::spawn(async move { app.oneshot(req).await.unwrap().status() })
            })
            .collect();

        let mut statuses = Vec::new();
        for task in tasks {
            statuses.push(task.await.unwrap());
        }

        // 實際被驗證（401）的不超過上限，其餘都在驗證之前被拒絕（429）
        let verified = statuses
            .iter()
            .filter(|s| **s == StatusCode::UNAUTHORIZED)
            .count();
        let rejected = statuses
            .iter()
            .filter(|s| **s == StatusCode::TOO_MANY_REQUESTS)
            .count();
        assert!(
            verified <= MAX_IP_FAILURES as usize,
            "{verified} guesses were verified"
        );
        assert_eq!(verified + rejected, ATTEMPTS);
    }

    #[sqlx::test]
    async fn disabled_account_with_correct_password_is_not_a_failure(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = create_dave(&t).await;
        disable(&t, dave).await;

        // 密碼正確但帳號停用，重複很多次：都是 403，而不是逐漸累積到 429
        for _ in 0..=MAX_IP_FAILURES {
            let (status, _, _) = login(&t, "dave", DAVE_PASSWORD).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
    }
}
