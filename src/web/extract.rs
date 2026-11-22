//! 身分驗證的 extractor。
//!
//! 兩個層級：
//! - `CurrentUser`（預設）：已登入、未停用，**而且不需要先完成任何事**
//!   （修改臨時密碼、依管理員的要求設定 2FA）。所有一般 API 都用它。
//! - `SessionUser`：已登入、未停用，允許「必須先完成一件事」的狀態。
//!   只用於 `/api/me`、`/api/me/credentials` 與 `/api/me/two-factor/*`，
//!   否則使用者會卡在無法完成那件事的狀態。
//!
//! 預設是嚴格的那一個：新增的 API 照慣例寫 `CurrentUser` 就自動受到保護，
//! 要允許這些狀態必須刻意改用 `SessionUser`。
//!
//! 檔案的 API 用 `InSpace`：目前的使用者，加上網址參數 `?space=` 指定的空間。
//!
//! `ClientIp`：使用者的 IP（登入次數限制用），放在反向代理後面時取自 X-Forwarded-For。

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, FromRef, FromRequestParts},
    http::{HeaderMap, request::Parts},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    auth::SESSION_COOKIE,
    error::{AppError, Forbidden},
};
use crate::{
    auth,
    config::IpRange,
    db::session,
    space::{self, Space, SpaceError, SpaceKey},
};

/// 未登入時統一的訊息：不區分是沒帶 Cookie 還是 session 失效
const NOT_LOGGED_IN: &str = "You're not logged in, or your session has expired";

/// 已登入且未停用的使用者，包含「必須修改密碼」與「必須設定 2FA」的狀態。
///
/// **只用於 `/api/me`、`/api/me/credentials` 與 `/api/me/two-factor/*`**。
/// 其他 API 一律使用 `CurrentUser`。
///
/// 每個請求都從資料庫重新讀取。被停用的使用者，其 session 在查詢中即被過濾，
/// 所以停用會立即生效，不必等 session 過期。
#[derive(Debug, Clone, Serialize)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    /// 管理員要求開啟 2FA，而這個帳號還沒有設定
    pub two_factor_setup_required: bool,
}

/// 目前登入的使用者：已登入、未停用，而且不需要先完成任何事。
///
/// 作為 extractor 使用：handler 參數寫上 `CurrentUser`，未登入的請求回 401，
/// 必須修改密碼或設定 2FA 的請求回 403，都在進入 handler 之前就被擋下。
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
            two_factor_setup_required: info.two_factor_setup_required,
        })
    }
}

/// 建立在 SessionUser 之上，多一道「不需要先完成任何事」的檢查
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

        if user.two_factor_setup_required {
            // 管理員要求開啟 2FA：先完成設定，前端依代碼 two_factor_setup_required 顯示設定畫面。
            // 排在修改密碼之後：臨時密碼要先改掉，才能設定 2FA
            return Err(AppError::Forbidden(Forbidden::TwoFactorSetupRequired));
        }

        Ok(CurrentUser {
            id: user.id,
            username: user.username,
            role: user.role,
        })
    }
}

/// 目前登入的管理員：已登入、未停用、不需要先完成任何事，而且角色是 admin。
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

/// 檔案的 API 要操作誰的檔案。
///
/// 檔案的 API：目前的使用者，以及網址參數 `space=` 指定的空間（見 `crate::space`）。
/// 沒有權限、空間不存在都在進入 handler 之前就被擋下
#[derive(Debug, Clone)]
pub struct InSpace {
    pub user: CurrentUser,
    pub space: Space,
}

impl<S> FromRequestParts<S> for InSpace
where
    SqlitePool: FromRef<S>,
    Arc<Path>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = CurrentUser::from_request_parts(parts, state).await?;
        let text = parts.uri.query().and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("space="))
                .map(str::to_string)
        });
        let key = SpaceKey::parse(text.as_deref()).ok_or_else(space_not_found)?;
        let pool = SqlitePool::from_ref(state);
        let data_dir = Arc::<Path>::from_ref(state);
        let space = resolve_space(&pool, &data_dir, &user, key).await?;
        Ok(InSpace { user, space })
    }
}

/// 找出空間並確認權限（分段上傳完成時，用上傳紀錄中的空間再找一次）
pub async fn resolve_space(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &CurrentUser,
    key: SpaceKey,
) -> Result<Space, AppError> {
    space::resolve(pool, data_dir, user.id, user.role == "admin", key)
        .await
        .map_err(|e| match e {
            SpaceError::NotFound => space_not_found(),
            SpaceError::AdminOnly => AppError::Forbidden(Forbidden::AdminRequired),
            SpaceError::Unavailable(message) => AppError::Unavailable(message),
            SpaceError::Db(e) => AppError::internal(e),
        })
}

fn space_not_found() -> AppError {
    AppError::NotFound("Not found".to_string())
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

    #[sqlx::test]
    async fn required_two_factor_blocks_current_user_but_not_session_user(pool: SqlitePool) {
        let token = login_as_carol(&pool).await;
        crate::db::two_factor::set_require_two_factor(&pool, true)
            .await
            .unwrap();

        let mut parts = request_parts(Some(&cookie_for(&token)));
        let err = CurrentUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AppError::Forbidden(Forbidden::TwoFactorSetupRequired)
        ));

        // /api/me 與 2FA 的設定仍然可以使用
        let mut parts = request_parts(Some(&cookie_for(&token)));
        let user = SessionUser::from_request_parts(&mut parts, &pool)
            .await
            .unwrap();
        assert!(user.two_factor_setup_required);
    }
}

// ==================== 使用者的 IP ====================

/// 放在前面的反向代理的位址（MANORCLOUD_TRUSTED_PROXIES）
#[derive(Clone, Default)]
pub struct TrustedProxies(Arc<[IpRange]>);

impl TrustedProxies {
    pub fn new(ranges: Vec<IpRange>) -> Self {
        TrustedProxies(ranges.into())
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|range| range.contains(ip))
    }

    /// 使用者真正的 IP。
    ///
    /// 連線不是來自信任的代理時，直接用連線的位址：任何人都能自己寫 X-Forwarded-For，
    /// 不能讓它決定登入次數算在誰頭上。
    ///
    /// 來自信任的代理時，X-Forwarded-For 是「使用者, 代理1, 代理2…」，每經過一層代理就在後面加一個。
    /// 從右往左看，跳過信任的代理，第一個不信任的就是使用者：最左邊的可能是使用者自己偽造的。
    /// 內容無法解析時當作沒有這個標頭，用連線的位址（代理本身）。
    fn client(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = peer.to_canonical();
        if !self.trusts(peer) {
            return peer;
        }
        let mut hops = Vec::new();
        for value in headers.get_all("x-forwarded-for") {
            let Ok(value) = value.to_str() else {
                return peer;
            };
            for hop in value.split(',') {
                match hop.trim().parse::<IpAddr>() {
                    Ok(ip) => hops.push(ip.to_canonical()),
                    Err(_) => return peer,
                }
            }
        }
        hops.iter()
            .rev()
            .find(|&&ip| !self.trusts(ip))
            .or(hops.first())
            .copied()
            .unwrap_or(peer)
    }
}

/// 使用者的 IP，見 TrustedProxies::client
pub struct ClientIp(pub IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    TrustedProxies: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ConnectInfo(peer) = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map_err(AppError::internal)?;
        let proxies = TrustedProxies::from_ref(state);
        Ok(ClientIp(proxies.client(peer.ip(), &parts.headers)))
    }
}

#[cfg(test)]
mod client_ip_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn proxies(list: &[&str]) -> TrustedProxies {
        TrustedProxies::new(list.iter().map(|r| IpRange::parse(r).unwrap()).collect())
    }

    fn forwarded(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append("x-forwarded-for", HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn without_trusted_proxies_the_header_is_ignored() {
        let none = TrustedProxies::default();

        // 任何人都能自己寫這個標頭
        let client = none.client(ip("203.0.113.9"), &forwarded(&["10.0.0.1"]));

        assert_eq!(client, ip("203.0.113.9"));
    }

    #[test]
    fn behind_a_trusted_proxy_the_forwarded_address_is_used() {
        let nginx = proxies(&["127.0.0.1"]);

        let client = nginx.client(ip("127.0.0.1"), &forwarded(&["203.0.113.9"]));

        assert_eq!(client, ip("203.0.113.9"));
    }

    #[test]
    fn a_forged_leftmost_address_is_skipped() {
        let nginx = proxies(&["127.0.0.1"]);

        // 使用者自己送了「1.1.1.1」，nginx 在後面加上真正的位址
        let client = nginx.client(ip("127.0.0.1"), &forwarded(&["1.1.1.1, 203.0.113.9"]));

        assert_eq!(client, ip("203.0.113.9"));
    }

    #[test]
    fn several_trusted_proxies_are_skipped() {
        let chain = proxies(&["127.0.0.1", "172.18.0.0/16"]);

        // 使用者 → Caddy（172.18.0.5）→ nginx（127.0.0.1）→ ManorCloud
        let client = chain.client(
            ip("::ffff:127.0.0.1"),
            &forwarded(&["203.0.113.9", "172.18.0.5"]),
        );

        assert_eq!(client, ip("203.0.113.9"));
    }

    #[test]
    fn a_missing_or_malformed_header_means_the_proxy_itself() {
        let nginx = proxies(&["127.0.0.1"]);

        assert_eq!(
            nginx.client(ip("127.0.0.1"), &HeaderMap::new()),
            ip("127.0.0.1")
        );
        assert_eq!(
            nginx.client(ip("127.0.0.1"), &forwarded(&["203.0.113.9, unknown"])),
            ip("127.0.0.1")
        );
    }
}
