pub mod files;
pub mod session;
pub mod uploads;
pub mod user;

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

/// 開啟（必要時建立）資料庫並執行 migration。伺服器啟動時使用。
///
/// 連線字串由呼叫端傳入，不在這裡讀取設定：設定的來源與驗證統一由 config 模組負責。
pub async fn init_pool(database_url: &str) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(database_url)
        .expect("invalid database URL")
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("failed to connect to the database");

    sqlx::migrate!()
        .run(&pool)
        .await
        .expect("failed to run migrations");

    println!("database connected; migrations applied");

    pool
}

/// 以唯讀模式開啟既有的資料庫，**不執行 migration**、不建立檔案。
///
/// 給匯出工具使用：逃生工具絕對不能修改任何資料。服務執行中也能安全地讀取。
/// `SqliteConnectOptions::new()` 預設不會在檔案不存在時建立它：
/// 路徑寫錯時會得到錯誤，而不是一個空的新資料庫。
pub async fn open_read_only(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
}
