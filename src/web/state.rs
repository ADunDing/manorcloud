use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::{header, request::Parts};

use super::extract::TrustedProxies;
use super::upload_progress::UploadProgress;
use sqlx::SqlitePool;

use crate::auth::limiter::{self, LoginLimiter};

/// 單一檔案的上傳大小上限（位元組）。
///
/// 管理員在管理頁面設定（存在資料庫的 server_settings，啟動時載入）；沒有設定時是預設值。
/// 所有請求共用同一個值，修改後立即生效。
#[derive(Clone, Debug)]
pub struct UploadLimit {
    /// 管理頁面沒有設定時的值
    default: u64,
    current: Arc<AtomicU64>,
}

impl UploadLimit {
    pub fn new(default: u64) -> Self {
        UploadLimit {
            default,
            current: Arc::new(AtomicU64::new(default)),
        }
    }

    /// 目前的上限
    pub fn get(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// 管理頁面沒有設定時的值
    pub fn default_value(&self) -> u64 {
        self.default
    }

    /// 管理頁面設定的值；None 回到預設值
    pub fn set(&self, custom: Option<u64>) {
        self.current
            .store(custom.unwrap_or(self.default), Ordering::Relaxed);
    }
}

/// 是否開放 WebDAV（/dav/）。管理員在管理頁面開關，立即生效
#[derive(Clone, Debug, Default)]
pub struct WebDavSwitch(Arc<AtomicBool>);

impl WebDavSwitch {
    pub fn enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, enabled: bool) {
        self.0.store(enabled, Ordering::Relaxed);
    }
}

/// 每個使用者最多幾個未完成（可續傳）的上傳：放棄的上傳不能無限累積佔用磁碟。
/// 管理員在管理頁面修改，立即生效
#[derive(Clone, Debug)]
pub struct PendingUploadLimit(Arc<AtomicU32>);

/// 管理頁面沒有設定時的值
pub const DEFAULT_PENDING_UPLOAD_LIMIT: u32 = 5;

impl Default for PendingUploadLimit {
    fn default() -> Self {
        PendingUploadLimit(Arc::new(AtomicU32::new(DEFAULT_PENDING_UPLOAD_LIMIT)))
    }
}

impl PendingUploadLimit {
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// 管理頁面設定的值；None 回到預設值
    pub fn set(&self, custom: Option<u32>) {
        self.0.store(
            custom.unwrap_or(DEFAULT_PENDING_UPLOAD_LIMIT),
            Ordering::Relaxed,
        );
    }
}

/// ManorCloud 自己提供 HTTPS（設定檔有 MANORCLOUD_TLS_CERT 與 MANORCLOUD_TLS_KEY）：
/// 所有連線都是加密的
#[derive(Clone, Copy, Debug, Default)]
pub struct ServesTls(pub bool);

/// Session Cookie 是否加上 `Secure`（只在 HTTPS 中送出），每個請求自己判斷，不需要設定：
///
/// 1. ManorCloud 自己提供 HTTPS
/// 2. 瀏覽器送來的 `Origin` 是 `https://`。登入、登出、修改密碼都是 POST，瀏覽器一定會帶上它
/// 3. 信任的反向代理說使用者連到它用的是 HTTPS（`X-Forwarded-Proto: https`）
///
/// 都不是時不加：http 連線中，瀏覽器會丟掉 Secure 的 Cookie，使用者就登不進去。
/// `Origin` 可以被非瀏覽器的程式偽造，但那只影響它自己拿到的 Cookie，不會影響別人
#[derive(Clone, Copy, Debug)]
pub struct CookieSecure(pub bool);

impl<S> FromRequestParts<S> for CookieSecure
where
    ServesTls: FromRef<S>,
    TrustedProxies: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if ServesTls::from_ref(state).0 {
            return Ok(CookieSecure(true));
        }
        if let Some(origin) = parts.headers.get(header::ORIGIN) {
            return Ok(CookieSecure(origin.as_bytes().starts_with(b"https://")));
        }
        let peer = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .ok()
            .map(|ConnectInfo(addr)| addr.ip());
        let from_proxy = peer.is_some_and(|ip| TrustedProxies::from_ref(state).trusts(ip));
        // 經過好幾層代理時，第一個值是使用者連到最外層代理的方式
        let https = parts
            .headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"));
        Ok(CookieSecure(from_proxy && https))
    }
}

/// 對外連線用的 HTTP 用戶端（目前只有 SSO：向身分提供者讀取設定、交換 token）。
///
/// 整個程式共用一個：它內部有連線池，clone 只是複製一個參照。
#[derive(Clone, Debug)]
pub struct HttpClient(pub reqwest::Client);

impl HttpClient {
    fn new() -> Self {
        let client = reqwest::Client::builder()
            // 不跟隨跳轉：身分提供者的網址由管理員設定，跟隨跳轉會讓伺服器被導向任意的網址
            // （openidconnect 的文件也建議這樣做）
            .redirect(reqwest::redirect::Policy::none())
            // 身分提供者沒有回應時，登入要在合理的時間內失敗，而不是讓使用者一直等
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("ManorCloud/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to create the HTTP client");
        HttpClient(client)
    }
}

/// 登入次數限制的計數，所有請求共用同一份。
///
/// 鎖只在計數的瞬間持有，從不跨過 await。
#[derive(Clone, Default)]
pub struct LoginLimits(Arc<Mutex<LoginLimiter>>);

impl LoginLimits {
    /// 取得計數器。
    ///
    /// 即使之前有請求在持有鎖時 panic（鎖「中毒」），計數仍然繼續運作：
    /// 登入限制是安全功能，不能因為一次 panic 就失效，或讓之後所有登入都變成 500。
    pub fn lock(&self) -> MutexGuard<'_, LoginLimiter> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 背景定期清除已經過期的記錄。
    ///
    /// 記錄滿了時 LoginLimiter 本來就會先清除過期的再記錄新的，記憶體上限已有保證；
    /// 這裡只是讓記憶體不會在一次大量嘗試之後長時間停在高點。
    /// 間隔取計數視窗的長度：記錄至少要過一個視窗才會過期，更頻繁沒有意義。
    pub fn spawn_pruning(&self) {
        let limits = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(limiter::WINDOW);
            loop {
                ticker.tick().await;
                limits.lock().prune(Instant::now());
            }
        });
    }
}

/// 整個應用程式共用的狀態。
///
/// axum 每次提取 `State` 都會 clone，所以每個欄位都必須便宜可 clone：
/// `SqlitePool` 內部本來就是共享的，`data_dir` 用 `Arc<Path>` 避免每次複製路徑字串，
/// 管理頁面可以修改的設定（上傳上限、反向代理等）內部是 `Arc`，所有請求看到同一份。
///
/// `FromRef` 讓 handler 可以只提取需要的欄位，例如 `State<SqlitePool>` 或
/// `State<UploadLimit>`，不必拿整個 `AppState`。
#[derive(Clone, FromRef)]
pub struct AppState {
    pub pool: SqlitePool,
    pub data_dir: Arc<Path>,
    pub upload_limit: UploadLimit,
    pub pending_upload_limit: PendingUploadLimit,
    pub serves_tls: ServesTls,
    pub login_limits: LoginLimits,
    pub http: HttpClient,
    pub upload_progress: UploadProgress,
    pub trusted_proxies: TrustedProxies,
    pub webdav: WebDavSwitch,
}

impl AppState {
    /// 其他設定使用預設值；正式環境啟動時再從資料庫載入（server_settings::load_settings）。
    /// 簽名維持三個參數，既有的測試都不需要修改。
    pub fn new(pool: SqlitePool, data_dir: PathBuf, max_upload_bytes: u64) -> Self {
        AppState {
            pool,
            data_dir: Arc::from(data_dir),
            upload_limit: UploadLimit::new(max_upload_bytes),
            pending_upload_limit: PendingUploadLimit::default(),
            serves_tls: ServesTls(false),
            login_limits: LoginLimits::default(),
            http: HttpClient::new(),
            upload_progress: UploadProgress::default(),
            trusted_proxies: TrustedProxies::default(),
            webdav: WebDavSwitch::default(),
        }
    }

    pub fn with_tls(mut self, serves_tls: bool) -> Self {
        self.serves_tls = ServesTls(serves_tls);
        self
    }

    pub fn with_max_pending_uploads(self, limit: u32) -> Self {
        self.pending_upload_limit.set(Some(limit));
        self
    }
}
