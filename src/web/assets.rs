//! 前端靜態檔案。
//!
//! 只打包 `frontend/public/`：它的上一層有 `node_modules/`、`package.json` 與 TypeScript 原始碼，
//! 這些絕不能被打包或對外提供。`public/js/` 是 `tsc` 的輸出，release 建置前必須先執行
//! `npm --prefix frontend run build`。
//!
//! 開發時（debug）rust-embed 在執行時從磁碟讀取，搭配 `npm run watch` 自動重新編譯，
//! 改了 TypeScript 重新整理瀏覽器即可；release 時檔案編譯進執行檔，部署只需要一個檔案。
//!
//! 前端路由使用網址的 `#` 部分（例如 `/#/folder/12`），不會送到伺服器，
//! 所以這裡不需要「找不到就回 index.html」，打錯的路徑會誠實地得到 404。

use axum::{
    Router,
    extract::{Path, Request},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::Embed;

use super::state::AppState;

#[derive(Embed)]
#[folder = "frontend/public/"]
struct Asset;

/// 頁面的內容安全政策：只允許本站的資源。
/// 沒有 CDN、沒有行內腳本與樣式，才能使用這份最嚴格的規則。
const CONTENT_SECURITY_POLICY: &str = concat!(
    "default-src 'self'; ",
    "script-src 'self'; ",
    "style-src 'self'; ",
    "img-src 'self' blob:; ",
    "connect-src 'self'; ",
    "object-src 'none'; ",
    "base-uri 'none'; ",
    "frame-ancestors 'none'; ",
    "form-action 'self'"
);

/// 前端靜態檔案的路由。所有回應（包含 404、405）都會經過 `security_headers`。
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(root_handler))
        .route("/{*path}", get(asset_handler))
        .layer(middleware::from_fn(security_headers))
}

async fn root_handler() -> Response {
    serve_asset("index.html")
}

async fn asset_handler(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

/// 由 rust-embed 取得檔案。路徑已被 axum 做過百分比解碼，可能包含 `..`；
/// release 時只有固定的檔案清單，debug 時依賴 rust-embed 的路徑檢查，
/// `path_traversal_is_rejected` 測試在 debug 模式下驗證這一點。
fn serve_asset(path: &str) -> Response {
    match Asset::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();

            let mut response = content.data.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref()).expect("mime type is ascii"),
            );
            response
        }
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}

/// 為所有靜態檔案的回應加上安全 header。
///
/// 寫成 middleware 而不是寫在 `serve_asset` 裡：404 與 405 也會被涵蓋，
/// 新增任何靜態路由也不可能漏掉。
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    // 禁止猜測類型；也讓瀏覽器拒絕執行 MIME 不是 JavaScript 的腳本
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // 不能用 no-referrer：依 Fetch 規範，那會讓同源的 POST 等請求帶上 `Origin: null`，
    // 被 CSRF 檢查擋下。same-origin 讓同源請求照常帶 Origin，跨站時不透露網址。
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    // 其他網站用 window.open 開啟本站時，無法透過 window.opener 操作本站的頁面
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    // 每次都向伺服器確認，更新程式後不會卡在舊版的 JS
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));

    response
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request};
    use axum::http::{HeaderMap, Method, StatusCode, header};
    use sqlx::SqlitePool;

    fn header_str(headers: &HeaderMap, name: header::HeaderName) -> &str {
        headers.get(name).unwrap().to_str().unwrap()
    }

    fn assert_page_headers(headers: &HeaderMap) {
        let csp = header_str(headers, header::CONTENT_SECURITY_POLICY);
        assert!(csp.contains("script-src 'self'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert_eq!(
            header_str(headers, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(header_str(headers, header::REFERRER_POLICY), "same-origin");
    }

    #[sqlx::test]
    async fn index_has_security_headers(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, headers, _) = t.send(empty_request(Method::GET, "/", None)).await;

        assert_eq!(status, StatusCode::OK);
        assert!(header_str(&headers, header::CONTENT_TYPE).starts_with("text/html"));
        assert_page_headers(&headers);
    }

    /// 也守住「忘記先編譯前端」：public/js/main.js 是 tsc 的輸出，
    /// 沒有先執行 `npm run build` 時這個測試會失敗，而不是默默出貨一個沒有前端的版本。
    #[sqlx::test]
    async fn javascript_is_served_with_javascript_mime(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, headers, _) = t
            .send(empty_request(Method::GET, "/js/main.js", None))
            .await;

        assert_eq!(
            status,
            StatusCode::OK,
            "找不到 /js/main.js：請先在 frontend/ 執行 npm run build"
        );
        // nosniff 之下，MIME 不是 JavaScript 的腳本會被瀏覽器拒絕執行
        assert!(header_str(&headers, header::CONTENT_TYPE).contains("javascript"));
    }

    #[sqlx::test]
    async fn not_found_also_has_security_headers(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, headers, _) = t.send(empty_request(Method::GET, "/nope.js", None)).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_page_headers(&headers);
    }

    /// debug 模式下 rust-embed 從磁碟讀取，這裡驗證它不會跳出 frontend/public/。
    /// 上一層就有真實存在的 package.json、tsconfig.json 與 TypeScript 原始碼，
    /// 如果這個測試失敗，代表開發環境存在路徑穿越，請立刻停下來處理。
    #[sqlx::test]
    async fn path_traversal_is_rejected(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        for uri in [
            "/%2e%2e/package.json",
            "/%2e%2e/tsconfig.json",
            "/%2e%2e/src/main.ts",
            "/..%2fpackage.json",
            "/%2e%2e%2fpackage.json",
            "/js/%2e%2e/%2e%2e/package.json",
            "/%2e%2e/%2e%2e/Cargo.toml",
        ] {
            let (status, _, body) = t.send(empty_request(Method::GET, uri, None)).await;
            let body = String::from_utf8_lossy(&body);

            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert!(!body.contains("manorcloud"), "{uri} 讀到了不該讀的檔案");
        }
    }

    #[sqlx::test]
    async fn api_routes_are_not_affected(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, headers, _) = t.send(empty_request(Method::GET, "/api/me", None)).await;

        // 頁面的 CSP 只套在靜態檔案上，API 回應不受影響
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_none());
    }
}
