use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::extract::FromRef;
use sqlx::SqlitePool;

use crate::auth::limiter::{self, LoginLimiter};

/// 單一檔案的上傳大小上限（位元組）。
///
/// 用專屬型別包起來而不是直接用 u64：`FromRef` 依「型別」提取欄位，
/// 若之後再有其他 u64 欄位會互相衝突；`State<UploadLimit>` 的語意也比 `State<u64>` 清楚。
#[derive(Clone, Copy, Debug)]
pub struct UploadLimit(pub u64);

/// 每個使用者最多幾個未完成（可續傳）的上傳
#[derive(Clone, Copy, Debug)]
pub struct PendingUploadLimit(pub u32);

/// 未設定時的預設值，與 config 的預設相同
const DEFAULT_PENDING_UPLOAD_LIMIT: u32 = 5;

/// Session Cookie 是否加上 `Secure`（只在 HTTPS 中送出）。
/// 同樣用專屬型別：`State<bool>` 看不出是哪一個設定。
#[derive(Clone, Copy, Debug)]
pub struct CookieSecure(pub bool);

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
/// 設定值是 Copy 的小型值，`LoginLimits` 內部是 `Arc`。
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
}

impl AppState {
    /// 其他設定使用預設值，正式環境透過 `with_*` 設定。
    /// 簽名維持三個參數，既有的測試都不需要修改。
    pub fn new(pool: SqlitePool, data_dir: PathBuf, max_upload_bytes: u64) -> Self {
        AppState {
            pool,
            data_dir: Arc::from(data_dir),
            upload_limit: UploadLimit(max_upload_bytes),
            pending_upload_limit: PendingUploadLimit(DEFAULT_PENDING_UPLOAD_LIMIT),
            cookie_secure: CookieSecure(false),
            login_limits: LoginLimits::default(),
        }
    }

    pub fn with_secure_cookies(mut self, secure: bool) -> Self {
        self.cookie_secure = CookieSecure(secure);
        self
    }

    pub fn with_max_pending_uploads(mut self, limit: u32) -> Self {
        self.pending_upload_limit = PendingUploadLimit(limit);
        self
    }
}
