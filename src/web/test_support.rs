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
    db::{files as files_db, session, user},
    storage::{self, TempBlob},
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
        storage::init_dirs(dir.path()).unwrap();
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

    /// 送出請求，回傳狀態碼、header 與完整內容
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
    pub async fn make_folder(&self, owner: i64, parent: Option<i64>, name: &str) -> i64 {
        files_db::create_folder(&self.pool, owner, parent, name)
            .await
            .unwrap()
            .id
    }

    /// 直接透過儲存層與資料層建立檔案，回傳 (id, storage_key)
    pub async fn store_file(
        &self,
        owner: i64,
        parent: Option<i64>,
        name: &str,
        content: &[u8],
    ) -> (i64, String) {
        let mut blob = TempBlob::create(self.dir.path(), MIB).await.unwrap();
        blob.write_chunk(content).await.unwrap();
        let key = storage::generate_storage_key();
        blob.persist(&key).await.unwrap();

        let size = i64::try_from(content.len()).unwrap();
        let entry = files_db::create_file(&self.pool, owner, parent, name, size, None, &key)
            .await
            .unwrap();
        (entry.id, key)
    }

    /// blob 是否存在於磁碟上
    pub fn blob_exists(&self, key: &str) -> bool {
        storage::blob_path(self.dir.path(), key).unwrap().exists()
    }

    pub fn blob_count(&self) -> usize {
        count_files(&self.dir.path().join("blobs"))
    }

    pub fn tmp_count(&self) -> usize {
        count_files(&self.dir.path().join("tmp"))
    }

    pub async fn row_count(&self) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files")
            .fetch_one(&self.pool)
            .await
            .unwrap()
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
