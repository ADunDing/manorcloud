//! SSO（OpenID Connect）的 API。
//!
//! 一般登入：
//!   1. POST /api/auth/sso/start：建立登入流程，Cookie 中放入 state，回傳身分提供者的登入網址
//!   2. 瀏覽器前往那個網址，使用者登入身分提供者
//!   3. GET  /api/auth/sso/callback：身分提供者把瀏覽器帶回來；核對、驗證之後建立 session
//!
//! 連結既有帳號：同樣的流程，只是第 1 步是 POST /api/me/sso/link（必須已登入），
//! 回來時把身分連結到這個帳號，而不是登入。
//!
//! 開始登入用 POST 而不是 GET：受 Origin 檢查保護，別的網站無法替使用者開始一個流程。
//! 回呼是身分提供者發起的跨網站跳轉，只能是 GET；它的保護是 state：
//! 網址中的 state 必須與這個瀏覽器的 Cookie 相同，而且只能使用一次。
//! 所以 Cookie 必須是 SameSite=Lax：Strict 的 Cookie 在跨網站跳轉時不會被帶上。
//!
//! 回呼的結果用跳轉表達（不是 JSON）：成功回到 My files 或 Settings，
//! 失敗時網址帶著 ?sso_error=原因，前端依原因顯示說明。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::Redirect,
    routing::{get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use openidconnect::url::{Host, Url};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    auth::{SESSION_COOKIE, start_session, validate_username},
    error::{AppError, Forbidden},
    extract::{AdminUser, CurrentUser},
    state::{AppState, CookieSecure, HttpClient},
};
use crate::{
    auth::{
        self,
        oidc::{self, Identity, ProviderConfig},
    },
    db::{
        session,
        sso::{self, CreateUserError, Flow, LinkError, SsoSettings, UnlinkOutcome},
    },
};

/// 登入流程的 Cookie：只送往 /api/auth/sso 底下（回呼網址在這裡）
const FLOW_COOKIE: &str = "manorcloud_sso_flow";
const FLOW_COOKIE_PATH: &str = "/api/auth/sso";
/// 登入流程的有效時間：在身分提供者那邊停留太久就要重新開始
const FLOW_MINUTES: i64 = 10;
const CALLBACK_PATH: &str = "/api/auth/sso/callback";

const DISPLAY_NAME_MAX: usize = 40;
const CLIENT_ID_MAX: usize = 255;
const URL_MAX: usize = 1024;
const SECRET_MAX: usize = 1024;
/// 自動建立帳號時，名稱衝突最多試到「名稱-99」
const MAX_NAME_ATTEMPTS: u32 = 99;

const NOT_CONFIGURED: &str = "Single sign-on is not set up";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/auth/sso", get(public_info))
        .route("/api/auth/sso/start", post(start_login))
        .route(CALLBACK_PATH, get(callback))
        .route("/api/me/sso", get(my_status).delete(unlink))
        .route("/api/me/sso/link", post(start_link))
        .route(
            "/api/admin/sso",
            get(admin_settings).put(save_admin_settings),
        )
        .route("/api/admin/sso/test", post(test_connection))
}

// ==================== 登入畫面需要的資訊 ====================

#[derive(Serialize)]
struct PublicInfo {
    enabled: bool,
    /// 按鈕文字：Sign in with {display_name}
    display_name: Option<String>,
}

/// GET /api/auth/sso：登入畫面要不要顯示「Sign in with …」。不需要登入
async fn public_info(State(pool): State<SqlitePool>) -> Result<Json<PublicInfo>, AppError> {
    let settings = sso::load_settings(&pool).await?.filter(|s| s.enabled);
    Ok(Json(PublicInfo {
        enabled: settings.is_some(),
        display_name: settings.map(|s| s.display_name),
    }))
}

// ==================== 開始登入、開始連結 ====================

#[derive(Serialize)]
struct StartResponse {
    /// 身分提供者的登入網址：前端把瀏覽器送過去
    url: String,
}

/// POST /api/auth/sso/start：開始用 SSO 登入
async fn start_login(
    State(pool): State<SqlitePool>,
    State(HttpClient(http)): State<HttpClient>,
    State(cookie_secure): State<CookieSecure>,
    jar: CookieJar,
) -> Result<(CookieJar, Json<StartResponse>), AppError> {
    begin_flow(&pool, &http, cookie_secure, jar, None).await
}

/// POST /api/me/sso/link：開始把 SSO 身分連結到目前的帳號
async fn start_link(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(HttpClient(http)): State<HttpClient>,
    State(cookie_secure): State<CookieSecure>,
    jar: CookieJar,
) -> Result<(CookieJar, Json<StartResponse>), AppError> {
    if sso::sign_in_methods(&pool, user.id).await?.sso_linked {
        return Err(AppError::Conflict(
            "Your account is already linked".to_string(),
        ));
    }
    begin_flow(&pool, &http, cookie_secure, jar, Some(user.id)).await
}

async fn begin_flow(
    pool: &SqlitePool,
    http: &reqwest::Client,
    CookieSecure(secure): CookieSecure,
    jar: CookieJar,
    link_user_id: Option<i64>,
) -> Result<(CookieJar, Json<StartResponse>), AppError> {
    let settings = enabled_settings(pool)
        .await?
        .ok_or_else(|| AppError::NotFound(NOT_CONFIGURED.to_string()))?;
    let redirect_uri = redirect_uri(&settings.public_url);

    let begin = oidc::begin(http, &provider_config(&settings, &redirect_uri))
        .await
        .map_err(|e| {
            eprintln!("sso: cannot start a sign-in: {e}");
            AppError::BadGateway("Couldn't reach the sign-in provider. Try again later".to_string())
        })?;

    // 資料庫只存 state 的雜湊，原始值只在 Cookie 與網址中
    let now = unix_now();
    let flow = Flow {
        nonce: begin.nonce,
        pkce_verifier: begin.pkce_verifier,
        link_user_id,
    };
    let state_hash = auth::session::hash_token(&begin.state);
    if !sso::create_flow(pool, &state_hash, &flow, now, now + FLOW_MINUTES * 60).await? {
        return Err(AppError::Unavailable(
            "Too many sign-ins are in progress. Try again in a few minutes".to_string(),
        ));
    }

    let cookie = Cookie::build((FLOW_COOKIE, begin.state))
        .http_only(true)
        // Lax：身分提供者跳轉回來是跨網站的 GET，Strict 的 Cookie 不會被帶上
        .same_site(SameSite::Lax)
        .secure(secure)
        .path(FLOW_COOKIE_PATH)
        .max_age(time::Duration::minutes(FLOW_MINUTES));

    Ok((jar.add(cookie), Json(StartResponse { url: begin.url })))
}

// ==================== 回呼 ====================

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// 身分提供者回報的錯誤，例如使用者按了取消（access_denied）
    error: Option<String>,
}

/// 失敗的原因：前端依代碼顯示說明
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// 流程不存在、已過期、已使用過，或不是這個瀏覽器開始的
    Expired,
    /// 使用者在身分提供者按了取消
    Cancelled,
    /// 其他失敗（細節只記在 log）
    Failed,
    /// 這個身分沒有對應的帳號，而設定是不自動建立
    NoAccount,
    Disabled,
    NotConfigured,
    /// 連結：這個身分已經連結到另一個帳號
    IdentityTaken,
    /// 連結：這個帳號已經連結了
    AlreadyLinked,
    /// 連結：開始連結之後登出了，或換成了別的帳號
    SessionChanged,
}

impl Failure {
    fn code(self) -> &'static str {
        match self {
            Failure::Expired => "expired",
            Failure::Cancelled => "cancelled",
            Failure::Failed => "failed",
            Failure::NoAccount => "no_account",
            Failure::Disabled => "disabled",
            Failure::NotConfigured => "not_configured",
            Failure::IdentityTaken => "identity_taken",
            Failure::AlreadyLinked => "already_linked",
            Failure::SessionChanged => "session_changed",
        }
    }
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        eprintln!("sso: database error: {e}");
        Failure::Failed
    }
}

enum Done {
    LoggedIn,
    Linked,
}

/// GET /api/auth/sso/callback：身分提供者把瀏覽器帶回來
async fn callback(
    State(pool): State<SqlitePool>,
    State(HttpClient(http)): State<HttpClient>,
    State(cookie_secure): State<CookieSecure>,
    jar: CookieJar,
    Query(query): Query<CallbackQuery>,
) -> (CookieJar, Redirect) {
    // 流程的 Cookie 不論結果如何都清除：每個流程只能用一次
    let flow_state = jar
        .get(FLOW_COOKIE)
        .map(|cookie| cookie.value().to_string());
    let jar = jar.remove(
        Cookie::build(FLOW_COOKIE)
            .path(FLOW_COOKIE_PATH)
            .secure(cookie_secure.0),
    );

    let mut linking = false;
    let result = complete(
        &pool,
        &http,
        cookie_secure,
        jar.clone(),
        flow_state,
        query,
        &mut linking,
    )
    .await;

    match result {
        Ok((jar, Done::LoggedIn)) => (jar, Redirect::to("/#/")),
        Ok((jar, Done::Linked)) => (jar, Redirect::to("/?sso=linked#/settings/sign-in")),
        Err(failure) => {
            let page = if linking { "#/settings/sign-in" } else { "#/" };
            (
                jar,
                Redirect::to(&format!("/?sso_error={}{page}", failure.code())),
            )
        }
    }
}

async fn complete(
    pool: &SqlitePool,
    http: &reqwest::Client,
    cookie_secure: CookieSecure,
    jar: CookieJar,
    flow_state: Option<String>,
    query: CallbackQuery,
    linking: &mut bool,
) -> Result<(CookieJar, Done), Failure> {
    // 1. 網址中的 state 必須與這個瀏覽器的 Cookie 相同：
    //    別人無法把自己的登入結果（自己的授權碼與 state）塞給你，讓你登入他的帳號
    let (Some(state), Some(flow_state)) = (query.state, flow_state) else {
        return Err(Failure::Expired);
    };
    if state != flow_state {
        return Err(Failure::Expired);
    }

    // 2. 取出並刪除流程：同一個回呼網址被重送也只會成功一次
    let flow = sso::take_flow(pool, &auth::session::hash_token(&state), unix_now())
        .await?
        .ok_or(Failure::Expired)?;
    *linking = flow.link_user_id.is_some();

    if let Some(error) = query.error {
        if error == "access_denied" {
            return Err(Failure::Cancelled);
        }
        // 身分提供者送來的文字：用 {:?} 印出，換行等控制字元會被跳脫，無法偽造日誌的內容
        let error: String = error.chars().take(100).collect();
        eprintln!("sso: the provider reported an error: {error:?}");
        return Err(Failure::Failed);
    }
    let code = query.code.ok_or(Failure::Failed)?;

    // 3. 換 token、驗證 ID token
    let settings = enabled_settings(pool)
        .await?
        .ok_or(Failure::NotConfigured)?;
    let redirect_uri = redirect_uri(&settings.public_url);
    let identity = oidc::finish(
        http,
        &provider_config(&settings, &redirect_uri),
        &code,
        &flow.nonce,
        &flow.pkce_verifier,
    )
    .await
    .map_err(|e| {
        eprintln!("sso: sign-in failed: {e}");
        Failure::Failed
    })?;

    // 4. 連結，或登入
    match flow.link_user_id {
        Some(user_id) => {
            // 這個瀏覽器必須仍然登入著開始連結的那個帳號
            if session_user_id(pool, &jar).await? != Some(user_id) {
                return Err(Failure::SessionChanged);
            }
            sso::link_identity(pool, user_id, &identity.issuer, &identity.subject)
                .await
                .map_err(|e| match e {
                    LinkError::IdentityTaken => Failure::IdentityTaken,
                    LinkError::AlreadyLinked => Failure::AlreadyLinked,
                    LinkError::Db(e) => e.into(),
                })?;
            Ok((jar, Done::Linked))
        }
        None => {
            let user_id = match sso::find_user_by_identity(
                pool,
                &identity.issuer,
                &identity.subject,
            )
            .await?
            {
                Some(id) => id,
                None if settings.auto_create => create_account(pool, &identity).await?,
                None => return Err(Failure::NoAccount),
            };
            // 與密碼登入相同的入口：停用的檢查在 start_session 裡
            let jar = start_session(pool, jar, user_id, cookie_secure)
                .await
                .map_err(|e| match e {
                    AppError::Forbidden(Forbidden::AccountDisabled) => Failure::Disabled,
                    _ => Failure::Failed,
                })?;
            Ok((jar, Done::LoggedIn))
        }
    }
}

/// 第一次用 SSO 登入：建立一般成員的帳號。名稱取自身分提供者建議的名稱，被使用時加上編號
async fn create_account(pool: &SqlitePool, identity: &Identity) -> Result<i64, Failure> {
    let base = username_base(identity);
    for n in 1..=MAX_NAME_ATTEMPTS {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        if validate_username(&candidate).is_err() {
            continue;
        }
        match sso::create_user_with_identity(pool, &candidate, &identity.issuer, &identity.subject)
            .await
        {
            Ok(id) => {
                println!("sso: created the account {candidate:?} on first sign-in");
                return Ok(id);
            }
            Err(CreateUserError::UsernameTaken) => continue,
            // 同時的另一個登入剛用這個身分建立了帳號：使用它
            Err(CreateUserError::IdentityTaken) => {
                return sso::find_user_by_identity(pool, &identity.issuer, &identity.subject)
                    .await?
                    .ok_or(Failure::Failed);
            }
            Err(CreateUserError::Db(e)) => return Err(e.into()),
        }
    }
    eprintln!("sso: no free username for {base:?}");
    Err(Failure::Failed)
}

/// 帳號名稱的基礎：身分提供者建議的名稱，或 email 的 @ 之前，轉成 ManorCloud 的規則
/// （小寫英文、數字、底線、連字號，3～32 字元）。都沒有、或轉換後太短時是 user
fn username_base(identity: &Identity) -> String {
    let source = identity
        .preferred_username
        .as_deref()
        .or_else(|| {
            identity
                .email
                .as_deref()
                .and_then(|email| email.split('@').next())
        })
        .unwrap_or("");
    let converted: String = source
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // 保留空間給「-99」
    let trimmed: String = converted.trim_matches('-').chars().take(28).collect();
    let trimmed = trimmed.trim_end_matches('-').to_string();
    if trimmed.len() < 3 {
        "user".to_string()
    } else {
        trimmed
    }
}

/// 這個瀏覽器目前登入的帳號
async fn session_user_id(pool: &SqlitePool, jar: &CookieJar) -> Result<Option<i64>, sqlx::Error> {
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    let token_hash = auth::session::hash_token(cookie.value());
    Ok(session::find_user_by_token_hash(pool, &token_hash)
        .await?
        .map(|info| info.user_id))
}

// ==================== 自己的連結狀態 ====================

#[derive(Serialize)]
struct MySso {
    /// SSO 是否已經設定並啟用
    enabled: bool,
    display_name: Option<String>,
    linked: bool,
    /// 沒有密碼的帳號不能取消連結，也不能修改密碼
    has_password: bool,
}

/// GET /api/me/sso：Settings → Sign-in 與 Account 需要的資訊
async fn my_status(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<MySso>, AppError> {
    let settings = enabled_settings(&pool).await?;
    let methods = sso::sign_in_methods(&pool, user.id).await?;
    Ok(Json(MySso {
        enabled: settings.is_some(),
        display_name: settings.map(|s| s.display_name),
        linked: methods.sso_linked,
        has_password: methods.has_password,
    }))
}

/// DELETE /api/me/sso：取消連結。沒有密碼的帳號不能取消，否則再也無法登入
async fn unlink(user: CurrentUser, State(pool): State<SqlitePool>) -> Result<StatusCode, AppError> {
    match sso::unlink_identity(&pool, user.id).await? {
        UnlinkOutcome::Unlinked => Ok(StatusCode::NO_CONTENT),
        UnlinkOutcome::NotLinked => {
            Err(AppError::NotFound("Your account isn't linked".to_string()))
        }
        UnlinkOutcome::NoPassword => Err(AppError::BadRequest(
            "Your account has no password, so you would no longer be able to log in".to_string(),
        )),
    }
}

// ==================== 管理員的設定 ====================

#[derive(Serialize)]
struct AdminSettings {
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    /// client secret 本身永遠不回傳，只告訴前端是否已經設定
    has_secret: bool,
    public_url: String,
    auto_create: bool,
    /// 要貼到身分提供者設定中的網址；ManorCloud 的網址還沒設定時為 None
    redirect_uri: Option<String>,
}

impl From<Option<SsoSettings>> for AdminSettings {
    fn from(settings: Option<SsoSettings>) -> Self {
        let settings = settings.unwrap_or(SsoSettings {
            enabled: false,
            display_name: String::new(),
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            public_url: String::new(),
            auto_create: false,
        });
        AdminSettings {
            redirect_uri: (!settings.public_url.is_empty())
                .then(|| redirect_uri(&settings.public_url)),
            enabled: settings.enabled,
            display_name: settings.display_name,
            issuer_url: settings.issuer_url,
            client_id: settings.client_id,
            has_secret: !settings.client_secret.is_empty(),
            public_url: settings.public_url,
            auto_create: settings.auto_create,
        }
    }
}

/// GET /api/admin/sso
async fn admin_settings(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<AdminSettings>, AppError> {
    Ok(Json(sso::load_settings(&pool).await?.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    enabled: bool,
    display_name: String,
    issuer_url: String,
    client_id: String,
    /// 沒有這個欄位或是空的：沿用已儲存的 client secret
    #[serde(default)]
    client_secret: Option<String>,
    public_url: String,
    auto_create: bool,
}

/// PUT /api/admin/sso：儲存設定。啟用時每一項都必須填寫
async fn save_admin_settings(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<SaveRequest>,
) -> Result<Json<AdminSettings>, AppError> {
    let existing = sso::load_settings(&pool).await?;
    let secret = match req.client_secret.as_deref().map(str::trim) {
        Some(secret) if !secret.is_empty() => secret.to_string(),
        _ => existing.map(|s| s.client_secret).unwrap_or_default(),
    };

    let settings = SsoSettings {
        enabled: req.enabled,
        display_name: req.display_name.trim().to_string(),
        issuer_url: req.issuer_url.trim().to_string(),
        client_id: req.client_id.trim().to_string(),
        client_secret: secret,
        public_url: req.public_url.trim().to_string(),
        auto_create: req.auto_create,
    };
    let settings = validate_settings(settings)?;
    sso::save_settings(&pool, &settings).await?;
    Ok(Json(Some(settings).into()))
}

/// 驗證並整理設定。未啟用時允許空白（可以先存一半）；有填的值一律檢查格式
fn validate_settings(mut settings: SsoSettings) -> Result<SsoSettings, AppError> {
    let bad = |message: &str| Err(AppError::BadRequest(message.to_string()));

    if settings.display_name.chars().count() > DISPLAY_NAME_MAX {
        return bad("The button label is too long");
    }
    if settings.client_id.len() > CLIENT_ID_MAX || settings.client_secret.len() > SECRET_MAX {
        return bad("The client ID or secret is too long");
    }
    if !settings.issuer_url.is_empty() {
        validate_issuer(&settings.issuer_url)?;
    }
    if !settings.public_url.is_empty() {
        settings.public_url = normalize_public_url(&settings.public_url)?;
    }

    if settings.enabled {
        let missing = [
            (settings.display_name.is_empty(), "button label"),
            (settings.issuer_url.is_empty(), "issuer URL"),
            (settings.client_id.is_empty(), "client ID"),
            (settings.client_secret.is_empty(), "client secret"),
            (settings.public_url.is_empty(), "ManorCloud address"),
        ]
        .into_iter()
        .filter(|(empty, _)| *empty)
        .map(|(_, name)| name)
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(AppError::BadRequest(format!(
                "To turn on single sign-on, fill in: {}",
                missing.join(", ")
            )));
        }
    }
    Ok(settings)
}

/// 身分提供者的網址必須是 https。例外是本機（測試用的身分提供者）
fn validate_issuer(issuer: &str) -> Result<(), AppError> {
    let bad = || {
        AppError::BadRequest(
            "The issuer URL must be an https:// address, for example https://auth.example.com/application/o/manorcloud/"
                .to_string(),
        )
    };
    if issuer.len() > URL_MAX {
        return Err(bad());
    }
    let url = Url::parse(issuer).map_err(|_| bad())?;
    let secure = url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url));
    if !secure || url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err(bad());
    }
    Ok(())
}

/// ManorCloud 對外的網址：只能有通訊協定、主機與埠號，整理成沒有結尾斜線的形式
fn normalize_public_url(public_url: &str) -> Result<String, AppError> {
    let bad = || {
        AppError::BadRequest(
            "The ManorCloud address must look like https://cloud.example.com, without a path"
                .to_string(),
        )
    };
    if public_url.len() > URL_MAX {
        return Err(bad());
    }
    let url = Url::parse(public_url).map_err(|_| bad())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
    {
        return Err(bad());
    }
    Ok(url.origin().ascii_serialization())
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestRequest {
    issuer_url: String,
}

/// POST /api/admin/sso/test：試著讀取身分提供者的設定。
/// 只有管理員能用，所以失敗時回傳詳細的原因（例如憑證不被信任），方便排除問題
async fn test_connection(
    _admin: AdminUser,
    State(HttpClient(http)): State<HttpClient>,
    Json(req): Json<TestRequest>,
) -> Result<StatusCode, AppError> {
    let issuer = req.issuer_url.trim();
    validate_issuer(issuer)?;
    oidc::check(&http, issuer)
        .await
        .map_err(|e| AppError::BadGateway(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ==================== 輔助函式 ====================

async fn enabled_settings(pool: &SqlitePool) -> Result<Option<SsoSettings>, sqlx::Error> {
    Ok(sso::load_settings(pool).await?.filter(|s| s.enabled))
}

fn redirect_uri(public_url: &str) -> String {
    format!("{public_url}{CALLBACK_PATH}")
}

fn provider_config<'a>(settings: &'a SsoSettings, redirect_uri: &'a str) -> ProviderConfig<'a> {
    ProviderConfig {
        issuer_url: &settings.issuer_url,
        client_id: &settings.client_id,
        client_secret: &settings.client_secret,
        redirect_uri,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    //! 用一個在測試中啟動的假身分提供者，跑完整的登入流程。
    //! 它用測試專用的 RSA 金鑰簽發 ID token；每個測試可以改變它下一次簽發的內容，
    //! 模擬各種正常與攻擊的情況。

    use super::super::test_support::{MIB, TestApp, empty_request, request};
    use super::*;
    use axum::{
        body::Body,
        http::{HeaderMap, Method, header},
    };
    use openidconnect::{
        AccessToken, Audience, AuthUrl, EmptyAdditionalClaims, EmptyExtraTokenFields,
        EndUserUsername, IdToken, IdTokenClaims, IssuerUrl, JsonWebKeyId, JsonWebKeySet,
        JsonWebKeySetUrl, Nonce, PrivateSigningKey, ResponseTypes, StandardClaims,
        SubjectIdentifier, TokenUrl,
        core::{
            CoreIdTokenFields, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
            CoreRsaPrivateSigningKey, CoreSubjectIdentifierType, CoreTokenResponse, CoreTokenType,
        },
    };
    use serde_json::Value;
    use std::sync::{Arc, Mutex};

    /// 測試專用的金鑰（PKCS#1），只用於簽發假的 ID token。
    /// FORGED_KEY 是另一把無關的金鑰：用它簽的 token 必須被拒絕
    const TEST_KEY: &str = include_str!("sso_test_key.pem");
    const FORGED_KEY: &str = include_str!("sso_forged_key.pem");
    const CLIENT_ID: &str = "manorcloud";

    /// 假身分提供者下一次簽發的 ID token 內容
    #[derive(Clone)]
    struct NextToken {
        subject: String,
        username: Option<String>,
        nonce: Option<String>,
        audience: String,
        /// 用另一把金鑰簽名（偽造的 token）
        forged: bool,
    }

    /// 修改下一次簽發的 ID token，模擬一種攻擊
    type Tamper = fn(&mut NextToken);

    #[derive(Clone)]
    struct Provider {
        issuer: String,
        next: Arc<Mutex<NextToken>>,
    }

    impl Provider {
        async fn start() -> Provider {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let issuer = format!("http://{}", listener.local_addr().unwrap());
            let provider = Provider {
                issuer,
                next: Arc::new(Mutex::new(NextToken {
                    subject: "sub-mom".to_string(),
                    username: Some("Mom".to_string()),
                    nonce: None,
                    audience: CLIENT_ID.to_string(),
                    forged: false,
                })),
            };
            let app = Router::new()
                .route("/.well-known/openid-configuration", get(metadata))
                .route("/jwks", get(jwks))
                .route("/token", post(token))
                .with_state(provider.clone());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            provider
        }

        fn set(&self, change: impl FnOnce(&mut NextToken)) {
            change(&mut self.next.lock().unwrap());
        }

        fn settings(&self, auto_create: bool) -> SsoSettings {
            SsoSettings {
                enabled: true,
                display_name: "Authentik".to_string(),
                issuer_url: self.issuer.clone(),
                client_id: CLIENT_ID.to_string(),
                client_secret: "secret".to_string(),
                public_url: "http://localhost:3000".to_string(),
                auto_create,
            }
        }
    }

    fn signing_key(forged: bool) -> CoreRsaPrivateSigningKey {
        let pem = if forged { FORGED_KEY } else { TEST_KEY };
        CoreRsaPrivateSigningKey::from_pem(pem, Some(JsonWebKeyId::new("test".to_string())))
            .unwrap()
    }

    async fn metadata(State(p): State<Provider>) -> Json<Value> {
        let metadata = CoreProviderMetadata::new(
            IssuerUrl::new(p.issuer.clone()).unwrap(),
            AuthUrl::new(format!("{}/authorize", p.issuer)).unwrap(),
            JsonWebKeySetUrl::new(format!("{}/jwks", p.issuer)).unwrap(),
            vec![ResponseTypes::new(vec![CoreResponseType::Code])],
            vec![CoreSubjectIdentifierType::Public],
            vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            Default::default(),
        )
        .set_token_endpoint(Some(TokenUrl::new(format!("{}/token", p.issuer)).unwrap()));
        Json(serde_json::to_value(metadata).unwrap())
    }

    async fn jwks() -> Json<Value> {
        let set = JsonWebKeySet::new(vec![signing_key(false).as_verification_key()]);
        Json(serde_json::to_value(set).unwrap())
    }

    async fn token(State(p): State<Provider>) -> Json<Value> {
        let next = p.next.lock().unwrap().clone();
        let now = chrono::Utc::now();
        let claims = IdTokenClaims::new(
            IssuerUrl::new(p.issuer.clone()).unwrap(),
            vec![Audience::new(next.audience)],
            now + chrono::Duration::seconds(300),
            now,
            StandardClaims::new(SubjectIdentifier::new(next.subject))
                .set_preferred_username(next.username.map(EndUserUsername::new)),
            EmptyAdditionalClaims {},
        )
        .set_nonce(next.nonce.map(Nonce::new));
        let id_token = IdToken::new(
            claims,
            &signing_key(next.forged),
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            None,
            None,
        )
        .unwrap();
        let response = CoreTokenResponse::new(
            AccessToken::new("access".to_string()),
            CoreTokenType::Bearer,
            CoreIdTokenFields::new(Some(id_token), EmptyExtraTokenFields {}),
        );
        Json(serde_json::to_value(response).unwrap())
    }

    // ---------- ManorCloud 這一邊 ----------

    /// 開始一個流程，回傳（流程的 Cookie、state、nonce）
    async fn begin(t: &TestApp, uri: &str, cookie: Option<&str>) -> (String, String, String) {
        let (status, headers, body) = t.send(empty_request(Method::POST, uri, cookie)).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let url: Value = serde_json::from_slice(&body).unwrap();
        let url = Url::parse(url["url"].as_str().unwrap()).unwrap();
        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.to_string())
                .unwrap()
        };
        // 送到身分提供者的必要參數：PKCE、state、nonce、我們的 Redirect URI
        assert_eq!(param("code_challenge_method"), "S256");
        assert_eq!(
            param("redirect_uri"),
            "http://localhost:3000/api/auth/sso/callback"
        );
        (
            cookie_value(&headers, FLOW_COOKIE),
            param("state"),
            param("nonce"),
        )
    }

    /// 送出回呼，回傳跳轉的目的地與回應的 header
    async fn callback(t: &TestApp, cookies: &[&str], query: &str) -> (String, HeaderMap) {
        let mut builder = request(Method::GET, &format!("{CALLBACK_PATH}?{query}"), None);
        if !cookies.is_empty() {
            builder = builder.header(header::COOKIE, cookies.join("; "));
        }
        let (status, headers, _) = t.send(builder.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = headers[header::LOCATION].to_str().unwrap().to_string();
        (location, headers)
    }

    fn cookie_value(headers: &HeaderMap, name: &str) -> String {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .find(|v| v.starts_with(&format!("{name}=")) && !v.starts_with(&format!("{name}=;")))
            .and_then(|v| v.split(';').next())
            .unwrap_or_else(|| panic!("no {name} cookie"))
            .to_string()
    }

    async fn me(t: &TestApp, session: &str) -> Value {
        let (status, _, body) = t
            .send(empty_request(Method::GET, "/api/me", Some(session)))
            .await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    /// 完整的一次 SSO 登入，回傳跳轉的目的地與 header
    async fn sign_in(t: &TestApp, provider: &Provider) -> (String, HeaderMap) {
        let (flow, state, nonce) = begin(t, "/api/auth/sso/start", None).await;
        provider.set(|next| next.nonce = Some(nonce));
        callback(t, &[&flow], &format!("code=abc&state={state}")).await
    }

    #[sqlx::test]
    async fn first_sign_in_creates_an_account_when_allowed(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;

        let (location, headers) = sign_in(&t, &provider).await;

        assert_eq!(location, "/#/");
        let session = cookie_value(&headers, SESSION_COOKIE);
        let user = me(&t, &session).await;
        // 名稱取自身分提供者建議的 Mom，轉成小寫
        assert_eq!(user["username"], "mom");
        assert_eq!(user["role"], "member");
        assert_eq!(user["must_change_password"], false);

        // 第二次用同一個身分登入：同一個帳號，不會再建立
        let (_, headers) = sign_in(&t, &provider).await;
        let again = me(&t, &cookie_value(&headers, SESSION_COOKIE)).await;
        assert_eq!(again["id"], user["id"]);
    }

    #[sqlx::test]
    async fn unknown_people_are_refused_when_not_allowed(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(false))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;

        let (location, headers) = sign_in(&t, &provider).await;

        assert_eq!(location, "/?sso_error=no_account#/");
        assert!(headers.get_all(header::SET_COOKIE).iter().all(|v| {
            !v.to_str()
                .unwrap()
                .starts_with(&format!("{SESSION_COOKIE}="))
        }));
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert_eq!(users, 1);
    }

    #[sqlx::test]
    async fn the_flow_must_belong_to_this_browser_and_is_single_use(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;
        let (flow, state, nonce) = begin(&t, "/api/auth/sso/start", None).await;
        provider.set(|next| next.nonce = Some(nonce));

        // 沒有這個瀏覽器的 Cookie（攻擊者把自己的回呼網址塞給受害者）
        let (without_cookie, _) = callback(&t, &[], &format!("code=abc&state={state}")).await;
        // state 與 Cookie 不符
        let (wrong_state, _) = callback(&t, &[&flow], "code=abc&state=forged").await;
        assert_eq!(without_cookie, "/?sso_error=expired#/");
        assert_eq!(wrong_state, "/?sso_error=expired#/");

        // 正確的一次可以登入；同一個回呼網址再送一次就不行
        let query = format!("code=abc&state={state}");
        let (first, _) = callback(&t, &[&flow], &query).await;
        let (replay, _) = callback(&t, &[&flow], &query).await;
        assert_eq!(first, "/#/");
        assert_eq!(replay, "/?sso_error=expired#/");
    }

    #[sqlx::test]
    async fn invalid_id_tokens_are_rejected(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;

        let cases: [(&str, Tamper); 3] = [
            // 舊的 ID token 被重放：nonce 不是這次的
            ("replayed nonce", |next| {
                next.nonce = Some("old-nonce".to_string())
            }),
            // 發給別的應用程式的 ID token
            ("other audience", |next| {
                next.audience = "another-app".to_string()
            }),
            // 不是身分提供者簽的
            ("forged signature", |next| next.forged = true),
        ];
        for (name, change) in cases {
            let (flow, state, nonce) = begin(&t, "/api/auth/sso/start", None).await;
            provider.set(|next| {
                *next = NextToken {
                    subject: "sub-mom".to_string(),
                    username: Some("Mom".to_string()),
                    nonce: Some(nonce),
                    audience: CLIENT_ID.to_string(),
                    forged: false,
                };
                change(next);
            });

            let (location, _) = callback(&t, &[&flow], &format!("code=abc&state={state}")).await;

            assert_eq!(location, "/?sso_error=failed#/", "{name}");
        }
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert_eq!(users, 1);
    }

    #[sqlx::test]
    async fn cancelling_at_the_provider_is_reported(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;
        let (flow, state, _) = begin(&t, "/api/auth/sso/start", None).await;

        let (location, _) =
            callback(&t, &[&flow], &format!("error=access_denied&state={state}")).await;

        assert_eq!(location, "/?sso_error=cancelled#/");
    }

    #[sqlx::test]
    async fn disabled_accounts_cannot_sign_in(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;
        let (_, headers) = sign_in(&t, &provider).await;
        let id = me(&t, &cookie_value(&headers, SESSION_COOKIE)).await["id"]
            .as_i64()
            .unwrap();
        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?")
            .bind(id)
            .execute(&t.pool)
            .await
            .unwrap();

        let (location, _) = sign_in(&t, &provider).await;

        assert_eq!(location, "/?sso_error=disabled#/");
    }

    #[sqlx::test]
    async fn an_existing_account_can_link_and_then_sign_in(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(false))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;

        // carol（TestApp 已登入的使用者）開始連結
        let (flow, state, nonce) = begin(&t, "/api/me/sso/link", Some(&t.cookie)).await;
        provider.set(|next| {
            next.nonce = Some(nonce);
            next.subject = "sub-carol".to_string();
        });
        let (location, _) =
            callback(&t, &[&flow, &t.cookie], &format!("code=abc&state={state}")).await;
        assert_eq!(location, "/?sso=linked#/settings/sign-in");

        // 之後用 SSO 登入就是 carol，即使設定是不自動建立帳號
        let (location, headers) = sign_in(&t, &provider).await;
        assert_eq!(location, "/#/");
        assert_eq!(
            me(&t, &cookie_value(&headers, SESSION_COOKIE)).await["username"],
            "carol"
        );

        let (status, _, body) = t
            .send(empty_request(Method::GET, "/api/me/sso", Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::OK);
        let status: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status["linked"], true);
        assert_eq!(status["has_password"], true);
    }

    #[sqlx::test]
    async fn linking_requires_the_same_session_and_a_free_identity(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;
        // mom 用 SSO 登入過：sub-mom 已經屬於她
        sign_in(&t, &provider).await;

        // 回來時已經沒有登入
        let (flow, state, nonce) = begin(&t, "/api/me/sso/link", Some(&t.cookie)).await;
        provider.set(|next| next.nonce = Some(nonce));
        let (logged_out, _) = callback(&t, &[&flow], &format!("code=abc&state={state}")).await;
        assert_eq!(logged_out, "/?sso_error=session_changed#/settings/sign-in");

        // carol 想連結的其實是 mom 的身分
        let (flow, state, nonce) = begin(&t, "/api/me/sso/link", Some(&t.cookie)).await;
        provider.set(|next| next.nonce = Some(nonce));
        let (taken, _) =
            callback(&t, &[&flow, &t.cookie], &format!("code=abc&state={state}")).await;
        assert_eq!(taken, "/?sso_error=identity_taken#/settings/sign-in");
    }

    #[sqlx::test]
    async fn accounts_without_a_password_cannot_unlink(pool: SqlitePool) {
        let provider = Provider::start().await;
        sso::save_settings(&pool, &provider.settings(true))
            .await
            .unwrap();
        let t = TestApp::new(pool, MIB).await;
        let (_, headers) = sign_in(&t, &provider).await;
        let mom = cookie_value(&headers, SESSION_COOKIE);

        let (status, _, _) = t
            .send(empty_request(Method::DELETE, "/api/me/sso", Some(&mom)))
            .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        // mom 不能用密碼登入（沒有密碼），也不能改密碼
        let body = r#"{"current_password":"anything","new_password":"a_new_password"}"#;
        let req = request(Method::POST, "/api/me/credentials", Some(&mom))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(t.send(req).await.0, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn the_login_page_only_learns_whether_sso_is_on(pool: SqlitePool) {
        let provider = Provider::start().await;
        let t = TestApp::new(pool, MIB).await;

        let (_, _, body) = t
            .send(empty_request(Method::GET, "/api/auth/sso", None))
            .await;
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["enabled"],
            false
        );
        let (status, _, _) = t
            .send(empty_request(Method::POST, "/api/auth/sso/start", None))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        sso::save_settings(&t.pool, &provider.settings(false))
            .await
            .unwrap();
        let (_, _, body) = t
            .send(empty_request(Method::GET, "/api/auth/sso", None))
            .await;
        let info: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            info,
            serde_json::json!({ "enabled": true, "display_name": "Authentik" })
        );
    }

    // ---------- 管理員的設定 ----------

    async fn make_admin(t: &TestApp) {
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    async fn put_settings(t: &TestApp, body: Value) -> (StatusCode, Value) {
        let req = request(Method::PUT, "/api/admin/sso", Some(&t.cookie))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, _, body) = t.send(req).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn valid_body() -> Value {
        serde_json::json!({
            "enabled": true,
            "display_name": "Authentik",
            "issuer_url": "https://auth.example.com/application/o/manorcloud/",
            "client_id": "manorcloud",
            "client_secret": "s3cret",
            "public_url": "https://cloud.example.com/",
            "auto_create": true,
        })
    }

    #[sqlx::test]
    async fn only_admins_can_see_or_change_the_settings(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/sso",
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            put_settings(&t, valid_body()).await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[sqlx::test]
    async fn settings_are_saved_without_ever_returning_the_secret(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;

        let (status, saved) = put_settings(&t, valid_body()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["has_secret"], true);
        assert!(!saved.to_string().contains("s3cret"));
        // 網址整理成沒有結尾斜線；Redirect URI 由它組成
        assert_eq!(saved["public_url"], "https://cloud.example.com");
        assert_eq!(
            saved["redirect_uri"],
            "https://cloud.example.com/api/auth/sso/callback"
        );

        // 再存一次但沒有給 secret：沿用原本的
        let mut body = valid_body();
        body.as_object_mut().unwrap().remove("client_secret");
        body["display_name"] = "Family login".into();
        let (status, saved) = put_settings(&t, body).await;
        assert_eq!(status, StatusCode::OK);
        let stored = sso::load_settings(&t.pool).await.unwrap().unwrap();
        assert_eq!(stored.client_secret, "s3cret");
        assert_eq!(saved["display_name"], "Family login");
    }

    #[sqlx::test]
    async fn settings_are_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;

        let cases: [(&str, Value); 5] = [
            // 身分提供者必須是 https（本機除外）
            ("issuer_url", "http://auth.example.com/".into()),
            ("issuer_url", "not a url".into()),
            // ManorCloud 的網址不能有路徑
            ("public_url", "https://cloud.example.com/files".into()),
            // 啟用時每一項都要填
            ("client_id", "".into()),
            ("display_name", "x".repeat(41).into()),
        ];
        for (field, value) in cases {
            let mut body = valid_body();
            body[field] = value.clone();
            assert_eq!(
                put_settings(&t, body).await.0,
                StatusCode::BAD_REQUEST,
                "{field} = {value}"
            );
        }

        // 未啟用時可以先存一半
        let half = serde_json::json!({
            "enabled": false, "display_name": "", "issuer_url": "", "client_id": "",
            "public_url": "", "auto_create": false,
        });
        assert_eq!(put_settings(&t, half).await.0, StatusCode::OK);
    }

    #[test]
    fn usernames_from_the_provider_follow_our_rules() {
        let identity = |username: Option<&str>, email: Option<&str>| Identity {
            issuer: "i".to_string(),
            subject: "s".to_string(),
            preferred_username: username.map(str::to_string),
            email: email.map(str::to_string),
        };
        assert_eq!(username_base(&identity(Some("Mom"), None)), "mom");
        assert_eq!(
            username_base(&identity(Some("Mary Jane"), None)),
            "mary-jane"
        );
        assert_eq!(
            username_base(&identity(None, Some("dad.smith@example.com"))),
            "dad-smith"
        );
        // 中文、太短、沒有：一律 user
        assert_eq!(username_base(&identity(Some("媽媽"), None)), "user");
        assert_eq!(username_base(&identity(Some("ab"), None)), "user");
        assert_eq!(username_base(&identity(None, None)), "user");
        let long = username_base(&identity(Some(&"a".repeat(50)), None));
        assert!(validate_username(&format!("{long}-99")).is_ok());
    }
}
