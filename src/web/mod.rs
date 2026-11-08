mod admin;
mod assets;
mod auth;
mod csrf;
mod download;
mod error;
mod extract;
mod files;
mod items;
mod range;
mod register;
mod shutdown;
mod state;
mod uploads;

#[cfg(test)]
mod test_support;

pub use state::AppState;

use axum::{Router, middleware};
use state::UploadLimit;
use std::net::SocketAddr;

/// 收到停止訊號後，最多等待進行中的請求多久。
/// 必須小於服務檔的 TimeoutStopSec，否則 systemd 會先用 SIGKILL 結束程式
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

pub async fn run(state: AppState, addr: SocketAddr) {
    // 開始接收請求前，先完成需要預熱的初始化
    auth::warm_up().await;

    // 先組裝 app：設定不合法（例如上傳上限超出平台範圍）會在開始監聽之前就停止
    let app = app(state);

    // 無法監聽（埠號被佔用、權限不足）是環境問題，不是程式的 bug：
    // 印出一行清楚的訊息後離開，而不是 panic
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot listen on {addr} (is the port already in use?): {e}");
            std::process::exit(1);
        }
    };
    println!("listening on http://{addr}");

    // 在通知就緒之前註冊訊號處理：之後收到的 SIGTERM 一定會被優雅地處理
    let stop = shutdown::shutdown_signal();

    // 已開始監聽：通知 systemd 服務就緒（Type=notify）
    crate::systemd::notify_ready();

    match shutdown::serve_until(listener, app, stop, SHUTDOWN_GRACE).await {
        Ok(shutdown::Shutdown::Graceful) => println!("stopped: all requests finished"),
        Ok(shutdown::Shutdown::TimedOut) => eprintln!(
            "stopped: requests still running after {} seconds were interrupted",
            SHUTDOWN_GRACE.as_secs()
        ),
        Err(e) => {
            eprintln!("server error: {e}");
            std::process::exit(1);
        }
    }
}

/// 組裝完整的應用程式。獨立出來讓測試可以直接使用。
fn app(state: AppState) -> Router {
    let upload_limit = state.upload_limit;

    Router::new()
        .merge(assets::router())
        .merge(api_router(upload_limit))
        .with_state(state)
}

/// 所有 /api 路由的唯一入口。
///
/// 新的 API 模組只要在這裡 merge 進來，就會自動受到 CSRF 保護。
/// `.layer` 必須維持在最後一行：它只會套用到在它之前加入的路由。
fn api_router(upload_limit: UploadLimit) -> Router<AppState> {
    Router::new()
        .merge(auth::router())
        .merge(admin::router())
        .merge(files::router(upload_limit))
        .merge(download::router())
        .merge(items::router())
        .merge(uploads::router())
        .layer(middleware::from_fn(csrf::check_origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode, header},
    };
    use sqlx::SqlitePool;
    use tempfile::TempDir;
    use tower::ServiceExt;

    /// 測試用的上傳上限：1 MiB
    const TEST_UPLOAD_LIMIT: u64 = 1024 * 1024;

    /// 建立測試用的 state，資料目錄指向一個臨時目錄。
    /// 回傳的 TempDir 必須在測試期間保持存活：它被丟棄時會刪除整個目錄。
    fn test_state(pool: SqlitePool) -> (AppState, TempDir) {
        let dir = tempfile::tempdir().expect("failed to create a temporary directory");
        let state = AppState::new(pool, dir.path().to_path_buf(), TEST_UPLOAD_LIMIT);
        (state, dir)
    }

    /// 建立一個假請求，Host 固定為 localhost:3000，可選擇是否帶 Origin
    fn request(method: Method, uri: &str, origin: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost:3000");
        if let Some(o) = origin {
            builder = builder.header(header::ORIGIN, o);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn status_of(pool: SqlitePool, req: Request<Body>) -> StatusCode {
        let (state, _dir) = test_state(pool);
        app(state).oneshot(req).await.unwrap().status()
    }

    #[sqlx::test]
    async fn cross_site_post_is_forbidden(pool: SqlitePool) {
        let req = request(Method::POST, "/api/logout", Some("http://evil.example"));
        assert_eq!(status_of(pool, req).await, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn cross_site_upload_is_forbidden(pool: SqlitePool) {
        // 上傳路由也在 api_router 內，自動受到 Origin 檢查保護
        let req = request(Method::POST, "/api/files", Some("http://evil.example"));
        assert_eq!(status_of(pool, req).await, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn same_origin_post_is_allowed(pool: SqlitePool) {
        let req = request(Method::POST, "/api/logout", Some("http://localhost:3000"));
        assert_eq!(status_of(pool, req).await, StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    async fn post_without_origin_is_allowed(pool: SqlitePool) {
        let req = request(Method::POST, "/api/logout", None);
        assert_eq!(status_of(pool, req).await, StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    async fn safe_method_is_not_checked(pool: SqlitePool) {
        // 跨站的 GET 不會被 403 擋下，而是正常走到 extractor 回 401
        let req = request(Method::GET, "/api/me", Some("http://evil.example"));
        assert_eq!(status_of(pool, req).await, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn static_routes_are_not_layered(pool: SqlitePool) {
        // 如果 CSRF middleware 也套到靜態路由，這裡會先被攔成 403；
        // 實際得到 405，證明請求直接到了靜態路由，沒經過 CSRF 檢查
        let req = request(Method::POST, "/", Some("http://evil.example"));
        assert_eq!(status_of(pool, req).await, StatusCode::METHOD_NOT_ALLOWED);
    }
}
