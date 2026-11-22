use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::extract::FromRef;

use super::extract::TrustedProxies;
use super::upload_progress::UploadProgress;
use sqlx::SqlitePool;

use crate::auth::limiter::{self, LoginLimiter};

/// 單一檔案的上傳大小上限（位元組）。
///
/// 預設是設定檔的 MANORCLOUD_MAX_UPLOAD_BYTES；管理員可以在管理頁面改成其他值
/// （存在資料庫的 server_settings，啟動時載入）。所有請求共用同一個值，修改後立即生效。
#[derive(Clone, Debug)]
pub struct UploadLimit {
    /// 設定檔的值
    config: u64,
    current: Arc<AtomicU64>,
}

impl UploadLimit {
    pub fn new(config: u64) -> Self {
        UploadLimit {
            config,
            current: Arc::new(AtomicU64::new(config)),
        }
    }

    /// 目前的上限
    pub fn get(&self) -> u64 {
        self.current.load(Ordering::Relaxed)
    }

    /// 設定檔的值（管理頁面沒有另外設定時使用）
    pub fn config(&self) -> u64 {
        self.config
    }

    /// 管理頁面設定的值；None 回到設定檔的值
    pub fn set(&self, custom: Option<u64>) {
        self.current
            .store(custom.unwrap_or(self.config), Ordering::Relaxed);
    }
}

/// 每個使用者最多幾個未完成（可續傳）的上傳
#[derive(Clone, Copy, Debug)]
pub struct PendingUploadLimit(pub u32);

/// 未設定時的預設值，與 config 的預設相同
const DEFAULT_PENDING_UPLOAD_LIMIT: u32 = 5;

/// Session Cookie 是否加上 `Secure`（只在 HTTPS 中送出）。
/// 同樣用專屬型別：`State<bool>` 看不出是哪一個設定。
#[derive(Clone, Copy, Debug)]
pub struct CookieSecure(pub bool);

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
/// 設定值是 Copy 的小型值，`LoginLimits`、`HttpClient` 與 `UploadProgress` 內部是 `Arc`。
///
/// `FromRef` 讓 handler 可以只提取需要的欄位，例如 `State<SqlitePool>` 或
/// `State<UploadLimit>`，不必拿整個 `AppState`。
#[derive(Clone, FromRef)]
pub struct AppState {
    pub pool: SqlitePool,
    pub data_dir: Arc<Path>,
    pub upload_limit: UploadLimit,
    pub pending_upload_limit: PendingUploadLimit,
    pub cookie_secure: CookieSecure,
    pub login_limits: LoginLimits,
    pub http: HttpClient,
    pub upload_progress: UploadProgress,
    pub trusted_proxies: TrustedProxies,
}

impl AppState {
    /// 其他設定使用預設值，正式環境透過 `with_*` 設定。
    /// 簽名維持三個參數，既有的測試都不需要修改。
    pub fn new(pool: SqlitePool, data_dir: PathBuf, max_upload_bytes: u64) -> Self {
        AppState {
            pool,
            data_dir: Arc::from(data_dir),
            upload_limit: UploadLimit::new(max_upload_bytes),
            pending_upload_limit: PendingUploadLimit(DEFAULT_PENDING_UPLOAD_LIMIT),
            cookie_secure: CookieSecure(false),
            login_limits: LoginLimits::default(),
            http: HttpClient::new(),
            upload_progress: UploadProgress::default(),
            trusted_proxies: TrustedProxies::default(),
        }
    }

    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.cookie_secure = CookieSecure(secure);
        self
    }

    pub fn with_trusted_proxies(mut self, proxies: TrustedProxies) -> Self {
        self.trusted_proxies = proxies;
        self
    }

    pub fn with_max_pending_uploads(mut self, limit: u32) -> Self {
        self.pending_upload_limit = PendingUploadLimit(limit);
        self
    }
}
