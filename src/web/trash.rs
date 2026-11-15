//! 垃圾桶：列出、還原、永久刪除、清空。
//!
//! 放進垃圾桶是 DELETE /api/files/{id}（見 items.rs）。這裡的永久刪除與 items.rs 的刪除流程相同：
//! 資料庫先刪並 commit，之後才刪 blob，失敗的留給垃圾回收。

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    error::AppError, extract::CurrentUser, files::FileResponse, items::remove_blobs,
    state::AppState,
};
use crate::db::files::{self as files_db, RestoreError};

/// 「不在垃圾桶中」與「屬於別人」對外一律使用這個訊息
const NOT_IN_TRASH: &str = "Item not found in the trash";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/trash", get(list).delete(empty))
        .route("/api/trash/{id}", delete(delete_forever))
        .route("/api/trash/{id}/restore", post(restore))
}

/// 垃圾桶中的一個項目
#[derive(Serialize)]
struct TrashResponse {
    id: i64,
    name: String,
    kind: String,
    /// 檔案的大小；資料夾是裡面所有檔案的總和
    size: i64,
    /// 放進垃圾桶的時間（Unix 秒數）
    trashed_at: i64,
    /// 原本的位置，由上往下的資料夾名稱；空的代表 My files
    location: Vec<String>,
}

/// GET /api/trash：垃圾桶的內容，最近刪除的在前
async fn list(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<TrashResponse>>, AppError> {
    let items = files_db::list_trash(&pool, user.id).await?;
    let mut response = Vec::with_capacity(items.len());
    for item in items {
        // 垃圾桶的項目通常不多，每個多查兩次可以接受
        let size = match item.kind.as_str() {
            "folder" => files_db::trash_size(&pool, user.id, item.id).await?,
            _ => item.size.unwrap_or(0),
        };
        let location = match item.parent_id {
            Some(parent) => files_db::folder_path(&pool, user.id, parent).await?,
            None => Vec::new(),
        };
        response.push(TrashResponse {
            id: item.id,
            name: item.name,
            kind: item.kind,
            size,
            trashed_at: item.trashed_at,
            location,
        });
    }
    Ok(Json(response))
}

/// POST /api/trash/{id}/restore：放回原本的資料夾（不在了就放回 My files），
/// 同名時自動改名。回傳還原後的項目
async fn restore(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<FileResponse>, AppError> {
    let entry = files_db::restore(&pool, user.id, id)
        .await
        .map_err(|e| match e {
            RestoreError::NotFound => not_in_trash(),
            RestoreError::NameConflict => AppError::Conflict(
                "There is no free name to restore this item; rename the existing one first"
                    .to_string(),
            ),
            RestoreError::Db(e) => AppError::internal(e),
        })?;
    Ok(Json(entry.into()))
}

/// DELETE /api/trash/{id}：永久刪除垃圾桶中的一個項目
async fn delete_forever(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let keys = files_db::delete_trashed(&pool, user.id, id)
        .await?
        .ok_or_else(not_in_trash)?;
    remove_blobs(&data_dir, &keys).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/trash：清空垃圾桶
async fn empty(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
) -> Result<StatusCode, AppError> {
    let keys = files_db::empty_trash(&pool, user.id).await?;
    remove_blobs(&data_dir, &keys).await;
    Ok(StatusCode::NO_CONTENT)
}

fn not_in_trash() -> AppError {
    AppError::NotFound(NOT_IN_TRASH.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request, request};
    use axum::{
        body::{Body, Bytes},
        http::{Method, StatusCode, header},
    };
    use serde_json::Value;
    use sqlx::SqlitePool;

    async fn call(t: &TestApp, method: Method, uri: &str) -> (StatusCode, Bytes) {
        let (status, _, body) = t.send(empty_request(method, uri, Some(&t.cookie))).await;
        (status, body)
    }

    async fn trash(t: &TestApp, id: i64) {
        let (status, _) = call(t, Method::DELETE, &format!("/api/files/{id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    async fn trash_list(t: &TestApp) -> Value {
        let (status, body) = call(t, Method::GET, "/api/trash").await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    #[sqlx::test]
    async fn list_shows_what_was_deleted_and_where_it_was(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let photos = t.make_folder(t.user_id, None, "相片").await;
        let trip = t.make_folder(t.user_id, Some(photos), "trip").await;
        t.store_file(t.user_id, Some(trip), "a.jpg", b"12345").await;
        trash(&t, trip).await;

        let list = trash_list(&t).await;

        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "trip");
        assert_eq!(list[0]["kind"], "folder");
        assert_eq!(list[0]["size"], 5);
        assert_eq!(list[0]["location"], serde_json::json!(["相片"]));
        assert!(list[0].get("storage_key").is_none());
    }

    #[sqlx::test]
    async fn restore_brings_the_item_back(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;
        trash(&t, id).await;
        // 原處又有了同名的檔案
        t.store_file(t.user_id, None, "a.txt", b"y").await;

        let (status, body) = call(&t, Method::POST, &format!("/api/trash/{id}/restore")).await;

        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["id"], id);
        assert_eq!(body["name"], "a (1).txt");
        assert!(trash_list(&t).await.as_array().unwrap().is_empty());
    }

    #[sqlx::test]
    async fn delete_forever_removes_the_content(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let (_, key) = t.store_file(t.user_id, Some(docs), "a.txt", b"x").await;
        trash(&t, docs).await;

        let (status, _) = call(&t, Method::DELETE, &format!("/api/trash/{docs}")).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!t.blob_exists(&key));
        assert_eq!(t.row_count().await, 0);
    }

    #[sqlx::test]
    async fn items_outside_the_trash_cannot_be_deleted_forever(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let (status, _) = call(&t, Method::DELETE, &format!("/api/trash/{id}")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(t.blob_exists(&key));
    }

    #[sqlx::test]
    async fn empty_removes_everything_in_the_trash(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (a, key_a) = t.store_file(t.user_id, None, "a.txt", b"a").await;
        let (b, key_b) = t.store_file(t.user_id, None, "b.txt", b"b").await;
        let (_, key_c) = t.store_file(t.user_id, None, "c.txt", b"c").await;
        trash(&t, a).await;
        trash(&t, b).await;

        let (status, _) = call(&t, Method::DELETE, "/api/trash").await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!t.blob_exists(&key_a));
        assert!(!t.blob_exists(&key_b));
        assert!(t.blob_exists(&key_c));
        assert_eq!(t.row_count().await, 1);
    }

    #[sqlx::test]
    async fn foreign_and_missing_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let (foreign, key) = t.store_file(alice, None, "secret.txt", b"x").await;
        crate::db::files::move_to_trash(&t.pool, alice, foreign, 100)
            .await
            .unwrap();

        for (method, suffix) in [(Method::POST, "/restore"), (Method::DELETE, "")] {
            let foreign_res =
                call(&t, method.clone(), &format!("/api/trash/{foreign}{suffix}")).await;
            let missing_res = call(&t, method, &format!("/api/trash/999999{suffix}")).await;
            assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
            assert_eq!(foreign_res, missing_res);
        }
        // alice 的垃圾桶完好；清空自己的垃圾桶不會碰到她的
        call(&t, Method::DELETE, "/api/trash").await;
        assert!(t.blob_exists(&key));
        assert!(trash_list(&t).await.as_array().unwrap().is_empty());
    }

    #[sqlx::test]
    async fn cross_site_empty_is_forbidden(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;
        trash(&t, id).await;

        let req = request(Method::DELETE, "/api/trash", Some(&t.cookie))
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = t.send(req).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(t.blob_exists(&key));
    }

    #[sqlx::test]
    async fn trash_requires_login(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, _, _) = t.send(empty_request(Method::GET, "/api/trash", None)).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
