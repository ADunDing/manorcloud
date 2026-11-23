pub mod session;
pub mod settings;
pub mod shares;
pub mod sso;
pub mod storage;
pub mod transfers;
pub mod trash;
pub mod two_factor;
pub mod uploads;
pub mod user;

use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// 等待其他連線釋放寫入鎖的時間上限。硬碟很慢時（例如 HDD 上的 ZFS），
/// 另一個寫入可能要好幾秒才結束；超過才回報「database is locked」
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// 開啟（必要時建立）資料庫並執行 migration。伺服器啟動時使用。
///
/// 連線字串由呼叫端傳入，不在這裡讀取設定：設定的來源與驗證統一由 config 模組負責。
///
/// 使用 WAL 模式：讀取不會被寫入擋住（例如搬移檔案時照樣能瀏覽），每次寫入也只需要
/// 一次 fsync，而不是預設模式的好幾次。synchronous 維持 FULL：寫入完成就一定已經落盤，
/// 斷電也不會遺失剛完成的上傳或設定。
pub async fn init_pool(database_url: &str) -> SqlitePool {
    let options = SqliteConnectOptions::from_str(database_url)
        .expect("invalid database URL")
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(BUSY_TIMEOUT);

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
