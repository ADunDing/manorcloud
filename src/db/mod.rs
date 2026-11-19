pub mod session;
pub mod sso;
pub mod storage;
pub mod transfers;
pub mod trash;
pub mod two_factor;
pub mod uploads;
pub mod user;

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
