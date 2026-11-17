//! 管理員瀏覽其他使用者的檔案（唯讀）：列出資料夾、查看項目、下載、打包下載。
//!
//!   GET /api/admin/users/{user}/files?parent_id=...      資料夾的內容
//!   GET /api/admin/users/{user}/files/{id}               項目與祖先鏈（麵包屑）
//!   GET /api/admin/users/{user}/files/{id}/content       下載
//!   GET /api/admin/users/{user}/archive?ids=1,2,3        打包下載
//!
//! 與使用者自己的 API 共用同一套查詢，只是把「目前的使用者」換成網址中的使用者。
//! 只能看、不能改：沒有上傳、改名、搬移、刪除。
//! 下載會記錄在伺服器的 log 中（誰、下載了誰的哪個項目）。

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
};
use serde::Deserialize;
use sqlx::SqlitePool;

use super::{
    download::{send_archive, send_file},
    error::AppError,
    extract::AdminUser,
    files::{FileResponse, folder_not_found},
    items::{ItemDetail, item_detail},
    state::AppState,
};
use crate::db::{files as files_db, user};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/users/{user}/files", get(list))
        .route("/api/admin/users/{user}/files/{id}", get(item))
        .route("/api/admin/users/{user}/files/{id}/content", get(download))
        .route("/api/admin/users/{user}/archive", get(archive))
}

#[derive(Deserialize)]
struct ParentParams {
    parent_id: Option<i64>,
}

#[derive(Deserialize)]
struct ArchiveQuery {
    ids: String,
}

/// 網址中的使用者必須存在，否則 404（而不是空的資料夾）
async fn ensure_user(pool: &SqlitePool, user_id: i64) -> Result<(), AppError> {
    if user::find_user_summary(pool, user_id).await?.is_none() {
        return Err(AppError::NotFound("User not found".to_string()));
    }
    Ok(())
}

/// GET /api/admin/users/{user}/files?parent_id=...
async fn list(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(owner): UrlPath<i64>,
    Query(params): Query<ParentParams>,
) -> Result<Json<Vec<FileResponse>>, AppError> {
    ensure_user(&pool, owner).await?;
    if let Some(id) = params.parent_id
        && !files_db::is_owned_folder(&pool, owner, id).await?
    {
        return Err(folder_not_found());
    }
    let entries = files_db::list_children(&pool, owner, params.parent_id).await?;
    Ok(Json(entries.into_iter().map(FileResponse::from).collect()))
}

/// GET /api/admin/users/{user}/files/{id}
async fn item(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath((owner, id)): UrlPath<(i64, i64)>,
) -> Result<Json<ItemDetail>, AppError> {
    item_detail(&pool, owner, id).await.map(Json)
}

/// GET /api/admin/users/{user}/files/{id}/content
async fn download(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath((owner, id)): UrlPath<(i64, i64)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let response = send_file(&pool, &data_dir, owner, id, &headers).await?;
    // 續傳會送出很多個 Range 請求：只記錄從頭開始的那一次
    if !headers.contains_key(axum::http::header::RANGE) {
        println!(
            "admin: administrator {} downloaded file {id} of user {owner}",
            admin.id
        );
    }
    Ok(response)
}

/// GET /api/admin/users/{user}/archive?ids=...
async fn archive(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(owner): UrlPath<i64>,
    Query(query): Query<ArchiveQuery>,
) -> Result<Response, AppError> {
    let response = send_archive(&pool, &data_dir, owner, &query.ids).await?;
    println!(
        "admin: administrator {} downloaded items {} of user {owner} as a zip",
        admin.id, query.ids
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request};
    use super::*;
    use axum::http::{Method, StatusCode};

    async fn make_admin(t: &TestApp) {
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    async fn admin_can_browse_and_download_another_users_files(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = t.other_user("alice").await;
        let folder = t.make_folder(alice, None, "Photos").await;
        let (file, _) = t.store_file(alice, Some(folder), "a.txt", b"hello").await;

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/admin/users/{alice}/files?parent_id={folder}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list[0]["name"], "a.txt");

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/admin/users/{alice}/files/{file}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        let item: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(item["ancestors"][0]["name"], "Photos");

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/admin/users/{alice}/files/{file}/content"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"hello");

        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/admin/users/{alice}/archive?ids={folder}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[sqlx::test]
    async fn items_of_someone_else_are_not_found(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let alice = t.other_user("alice").await;
        let (mine, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        // 網址中是 alice，項目卻是 carol 的
        for uri in [
            format!("/api/admin/users/{alice}/files/{mine}"),
            format!("/api/admin/users/{alice}/files/{mine}/content"),
            format!("/api/admin/users/{alice}/files?parent_id={mine}"),
            "/api/admin/users/9999/files".to_string(),
        ] {
            let (status, _, _) = t
                .send(empty_request(Method::GET, &uri, Some(&t.cookie)))
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        }
    }

    #[sqlx::test]
    async fn members_cannot_browse(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;

        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/admin/users/{alice}/files"),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
