//! CSRF 防護：拒絕瀏覽器從其他來源發出的「會改資料」請求。

use axum::{extract::Request, http::header, middleware::Next, response::Response};

use super::error::{AppError, Forbidden};

/// Middleware：檢查 `Origin` 是否與 `Host` 相符，不符則回 403。
pub async fn check_origin(request: Request, next: Next) -> Result<Response, AppError> {
    verify_origin(&request)?;
    Ok(next.run(request).await)
}

/// 實際的檢查邏輯。只借用 request，借用結束後才交給 `next.run`。
fn verify_origin(request: &Request) -> Result<(), AppError> {
    // 1. GET／HEAD／OPTIONS 等安全方法不改資料，不需要檢查
    if request.method().is_safe() {
        return Ok(());
    }

    // 2. 沒有 Origin → 放行。
    //    現代瀏覽器對跨來源請求與同來源的 POST 一律附上 Origin，網頁無法省略它；
    //    缺少 Origin 代表是 curl、App 等非瀏覽器客戶端，它們無法被用來發動 CSRF。
    //    殘餘風險（極舊瀏覽器不送 Origin）由 SameSite=Lax 的 Cookie 兜底。
    let Some(origin) = request.headers().get(header::ORIGIN) else {
        return Ok(());
    };

    // 3. header 值可能不是合法 UTF-8；瀏覽器不會送出這種值，視為可疑直接拒絕
    let origin = origin.to_str().map_err(|_| forbidden())?;

    // 4. 取得 Host。HTTP/1.1 必定有 Host header；HTTP/2 則放在 URI 的 authority。
    //    兩者都沒有本身就不正常，在無法驗證的情況下選擇拒絕
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()))
        .ok_or_else(forbidden)?;

    // 5. 比對
    if is_same_origin(origin, host) {
        Ok(())
    } else {
        // 記錄實際值，部署在反向代理後若設定錯誤，可以從這裡看出原因
        eprintln!(
            "拒絕跨來源請求：{} {}，Origin={origin}，Host={host}",
            request.method(),
            request.uri().path()
        );
        Err(forbidden())
    }
}

/// 判斷 `Origin`（如 `http://pi:3000`）是否與 `Host`（如 `pi:3000`）指向同一個來源。
///
/// - 忽略 scheme：`Host` 本身不含 scheme，放在 HTTPS 反向代理後面時兩者 scheme 也會不同。
/// - 主機名稱不分大小寫。
/// - `Origin: null` 與任何格式不符的值一律回傳 false。
fn is_same_origin(origin: &str, host: &str) -> bool {
    // 去掉 scheme；沒有 "://" 的值（包含 "null"）都不是合法的 Origin
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };

    if scheme.is_empty() || authority.is_empty() || host.is_empty() {
        return false;
    }

    // 合法的 Origin 只有 scheme://host[:port]，不會有路徑、查詢、片段或帳密
    if authority.contains(['/', '?', '#', '@']) {
        return false;
    }

    authority.eq_ignore_ascii_case(host)
}

fn forbidden() -> AppError {
    AppError::Forbidden(Forbidden::CrossOrigin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_origin_and_host() {
        assert!(is_same_origin("http://localhost:3000", "localhost:3000"));
        assert!(is_same_origin(
            "http://192.168.1.10:3000",
            "192.168.1.10:3000"
        ));
    }

    #[test]
    fn scheme_is_ignored() {
        // 放在 HTTPS 反向代理後面的情況
        assert!(is_same_origin("https://pi.lan", "pi.lan"));
    }

    #[test]
    fn different_host() {
        assert!(!is_same_origin("http://evil.example", "localhost:3000"));
        assert!(!is_same_origin(
            "http://192.168.1.11:3000",
            "192.168.1.10:3000"
        ));
    }

    #[test]
    fn different_port() {
        // 同主機不同埠：SameSite 擋不住，這裡必須擋
        assert!(!is_same_origin("http://localhost:8080", "localhost:3000"));
        assert!(!is_same_origin("http://localhost", "localhost:3000"));
    }

    #[test]
    fn null_origin() {
        assert!(!is_same_origin("null", "localhost:3000"));
    }

    #[test]
    fn malformed_input() {
        assert!(!is_same_origin("", "localhost:3000"));
        assert!(!is_same_origin("localhost:3000", "localhost:3000")); // 沒有 scheme
        assert!(!is_same_origin("://localhost:3000", "localhost:3000")); // 空 scheme
        assert!(!is_same_origin("http://", "")); // 都是空的
        assert!(!is_same_origin("http://localhost:3000", "")); // 空 Host
    }

    #[test]
    fn case_insensitive_host() {
        assert!(is_same_origin("http://LocalHost:3000", "localhost:3000"));
        assert!(is_same_origin("http://pi.lan", "PI.LAN"));
    }

    #[test]
    fn ipv6() {
        assert!(is_same_origin("http://[::1]:3000", "[::1]:3000"));
        assert!(!is_same_origin("http://[::1]:3000", "[::2]:3000"));
    }

    #[test]
    fn weird_suffixes_rejected() {
        assert!(!is_same_origin("http://localhost:3000/", "localhost:3000"));
        assert!(!is_same_origin(
            "http://localhost:3000/path",
            "localhost:3000"
        ));
        assert!(!is_same_origin(
            "http://localhost:3000?x=1",
            "localhost:3000"
        ));
        assert!(!is_same_origin(
            "http://user@localhost:3000",
            "localhost:3000"
        ));
    }
}
