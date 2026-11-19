//! web 層測試共用的輔助工具。
//!
//! 部分輔助函式目前只有一個模組使用，其他模組之後會陸續改用，
//! 所以整個模組允許未使用的程式碼。

#![allow(dead_code)]

use axum::extract::connect_info::MockConnectInfo;
use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Method, Request, StatusCode, header, request::Builder},
};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use super::{AppState, app, auth::SESSION_COOKIE};
use crate::{
    auth::session::{generate_token, hash_token},
    db::{session, user},
};

pub const MIB: u64 = 1024 * 1024;

/// 測試環境：一個已登入的使用者、一個臨時資料目錄、一個組裝好的 app
pub struct TestApp {
    pub app: Router,
    pub pool: SqlitePool,
    pub dir: TempDir,
    pub cookie: String,
    pub user_id: i64,
}

impl TestApp {
    /// 建立名為 carol 的已登入使用者，上傳上限為 `limit`
    pub async fn new(pool: SqlitePool, limit: u64) -> Self {
        let user_id = user::create_user(&pool, "carol", "x").await.unwrap();
        let token = generate_token();
        session::create_session(&pool, user_id, &hash_token(&token), 4_000_000_000)
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(pool.clone(), dir.path().to_path_buf(), limit);

        TestApp {
            // 測試沒有真正的 TCP 連線，由這裡提供一個固定的來源位址給需要 IP 的 handler
            app: app(state).layer(MockConnectInfo(SocketAddr::from((
                [192, 168, 1, 10],
                50000,
            )))),
            pool,
            dir,
            cookie: format!("{SESSION_COOKIE}={token}"),
            user_id,
        }
    }

    /// 資料目錄（預設的儲存位置）
    pub fn data_dir(&self) -> std::path::PathBuf {
        self.dir.path().to_path_buf()
    }

    /// 使用者在資料目錄中的資料夾（`{資料目錄}/{帳號名稱}`），不存在時建立
    /// 一般使用者的 My files（不存在時建立）
    pub fn home(&self, username: &str) -> std::path::PathBuf {
        let dir = self.dir.path().join("users").join(username);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把目前的使用者（carol）改成管理員
    pub async fn make_admin(&self) {
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(self.user_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    pub async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body)
    }

    /// 建立另一個使用者，回傳 id
    pub async fn other_user(&self, username: &str) -> i64 {
        user::create_user(&self.pool, username, "x").await.unwrap()
    }

    /// 直接透過資料層建立資料夾，回傳 id
    /// 送出 JSON，回傳狀態碼與解析過的 JSON（沒有內容時是 Null）
    pub async fn json(
        &self,
        method: Method,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let builder = request(method, uri, Some(&self.cookie));
        let req = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let (status, _, bytes) = self.send(req).await;
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    /// 一般上傳（multipart）
    pub async fn upload(&self, query: &str, content: &[u8]) -> (StatusCode, serde_json::Value) {
        let boundary = "BOUNDARY";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let req = request(
            Method::POST,
            &format!("/api/files?{query}"),
            Some(&self.cookie),
        )
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
        let (status, _, bytes) = self.send(req).await;
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }
}

/// 建立請求。固定帶上 Host，讓跨站測試的 403 是因為 Origin 不符，而不是缺少 Host。
pub fn request(method: Method, uri: &str, cookie: Option<&str>) -> Builder {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost:3000");
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    builder
}

/// 沒有內容的請求
pub fn empty_request(method: Method, uri: &str, cookie: Option<&str>) -> Request<Body> {
    request(method, uri, cookie).body(Body::empty()).unwrap()
}

/// 遞迴計算目錄下的一般檔案數量（目錄不存在視為 0）
pub fn count_files(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() { count_files(&path) } else { 1 }
        })
        .sum()
}
