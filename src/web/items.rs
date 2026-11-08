//! 對既有項目（檔案或資料夾）的操作：查詢、刪除、重新命名與搬移。

use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::delete,
};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::CurrentUser,
    files::{FileResponse, folder_not_found},
    state::AppState,
};
use crate::{
    db::files::{self as files_db, UpdateError},
    files::name::validate_file_name,
    storage,
};

/// 「不存在」與「屬於別人」對外一律使用這個訊息
const ITEM_NOT_FOUND: &str = "找不到項目";

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/files/{id}",
        delete(delete_item).patch(patch_item).get(get_item),
    )
}

// ==================== 查詢 ====================

/// 祖先鏈的一層
#[derive(Serialize)]
struct Crumb {
    id: i64,
    name: String,
}

/// 項目資訊加上祖先鏈。`flatten` 讓項目的欄位直接攤平在最外層，
/// 回應的形狀是 `{"id": ..., "name": ..., ..., "ancestors": [...]}`
#[derive(Serialize)]
struct ItemDetail {
    #[serde(flatten)]
    item: FileResponse,
    ancestors: Vec<Crumb>,
}

/// GET /api/files/{id}：項目資訊與由根目錄往下的祖先鏈（麵包屑導覽用）。
/// 不存在與屬於別人回傳相同的 404。
async fn get_item(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<ItemDetail>, AppError> {
    let entry = files_db::find_file(&pool, user.id, id)
        .await?
        .ok_or_else(item_not_found)?;
    let ancestors = files_db::ancestors(&pool, user.id, id)
        .await?
        .into_iter()
        .map(|a| Crumb {
            id: a.id,
            name: a.name,
        })
        .collect();

    Ok(Json(ItemDetail {
        item: entry.into(),
        ancestors,
    }))
}

// ==================== 刪除 ====================

/// DELETE /api/files/{id}：刪除檔案或資料夾（連同底下所有內容）。
///
/// 順序由不變式決定：資料庫先刪並 commit，之後才刪 blob。
/// 刪 blob 失敗只記 log，留給垃圾回收；不影響回應，因為資料庫中的項目確實已經刪除了。
///
/// 取消安全：使用者在 commit 前斷線，交易自動 rollback，什麼都沒刪；
/// 在 commit 後、刪 blob 的過程中斷線，剩下的 blob 成為孤兒，由垃圾回收處理。
async fn delete_item(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let keys = files_db::delete_tree(&pool, user.id, id)
        .await?
        .ok_or_else(item_not_found)?;

    // 資料庫已 commit；逐一刪除 blob，任何一個失敗都不中斷其他的
    for key in &keys {
        match storage::remove_blob(&data_dir, key).await {
            Ok(()) => {}
            // 目標是「blob 不存在」，已經不在就是達成了
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("failed to remove blob {key} (garbage collection will retry): {e}"),
        }
    }

    Ok(StatusCode::NO_CONTENT)
}

// ==================== 重新命名與搬移 ====================

/// PATCH 的請求內容，兩個欄位都是選填。
///
/// `deny_unknown_fields`：前端打錯欄位名稱（例如 `parentId`）時直接回錯誤。
/// 否則 serde 會默默忽略它，請求變成「什麼都不改」卻回傳成功，這種 bug 很難察覺。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    name: Option<String>,
    /// 三種狀態：
    /// - 沒有這個欄位 → `None`（不搬移，由 `#[serde(default)]` 填入）
    /// - `null` → `Some(None)`（搬到根目錄）
    /// - 數字 → `Some(Some(id))`（搬到該資料夾）
    #[serde(default, deserialize_with = "present")]
    parent_id: Option<Option<i64>>,
}

/// 欄位「有出現」時才會被呼叫，把 serde 解析出的 `Option<i64>` 再包一層 `Some`，
/// 用來區分「欄位為 null」與「沒有這個欄位」。
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<i64>>, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

/// PATCH /api/files/{id}：重新命名及／或搬移。
async fn patch_item(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<UpdateRequest>,
) -> Result<Json<FileResponse>, AppError> {
    if req.name.is_none() && req.parent_id.is_none() {
        return Err(AppError::BadRequest("沒有要修改的內容".to_string()));
    }

    // 有給名稱才驗證；Option<Result> 經 transpose 變成 Result<Option>，才能用 ?
    let name = req
        .name
        .as_deref()
        .map(validate_file_name)
        .transpose()
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let entry = files_db::update_item(&pool, user.id, id, name.as_deref(), req.parent_id)
        .await
        .map_err(update_error)?;

    Ok(Json(entry.into()))
}

// ==================== 錯誤對應 ====================

fn item_not_found() -> AppError {
    AppError::NotFound(ITEM_NOT_FOUND.to_string())
}

/// 更新失敗的原因 → HTTP 狀態碼。
/// 循環回 409：同一個請求在不同的目錄狀態下可能合法也可能不合法，屬於「與目前狀態衝突」。
fn update_error(e: UpdateError) -> AppError {
    match e {
        UpdateError::NotFound => item_not_found(),
        UpdateError::TargetNotFolder => folder_not_found(),
        UpdateError::Cycle => {
            AppError::Conflict("不能把資料夾搬進它自己或它的子資料夾".to_string())
        }
        UpdateError::NameConflict => AppError::Conflict("同名項目已存在".to_string()),
        UpdateError::Db(e) => AppError::internal(e),
    }
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

    fn item_uri(id: i64) -> String {
        format!("/api/files/{id}")
    }

    async fn patch(t: &TestApp, id: i64, json: &str) -> (StatusCode, Bytes) {
        let req = request(Method::PATCH, &item_uri(id), Some(&t.cookie))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json.to_string()))
            .unwrap();
        let (status, _, body) = t.send(req).await;
        (status, body)
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    // ==================== 查詢 ====================

    #[sqlx::test]
    async fn get_item_returns_ancestors(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let photos = t.make_folder(t.user_id, None, "相片").await;
        let year = t.make_folder(t.user_id, Some(photos), "2026").await;

        let (status, _, body) = t
            .send(empty_request(Method::GET, &item_uri(year), Some(&t.cookie)))
            .await;

        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        assert_eq!(body["name"], "2026");
        assert_eq!(body["kind"], "folder");
        assert_eq!(body["ancestors"][0]["name"], "相片");
        assert_eq!(body["ancestors"].as_array().unwrap().len(), 1);
        assert!(body.get("storage_key").is_none());
    }

    #[sqlx::test]
    async fn get_foreign_and_missing_item_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let foreign = t.make_folder(alice, None, "secret").await;

        let foreign_res = t
            .send(empty_request(
                Method::GET,
                &item_uri(foreign),
                Some(&t.cookie),
            ))
            .await;
        let missing_res = t
            .send(empty_request(
                Method::GET,
                &item_uri(999_999),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        assert_eq!(
            (foreign_res.0, &foreign_res.2),
            (missing_res.0, &missing_res.2)
        );
    }

    // ==================== 刪除 ====================

    #[sqlx::test]
    async fn delete_file_removes_row_and_blob(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"hello").await;

        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(id),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(t.row_count().await, 0);
        assert!(!t.blob_exists(&key));
    }

    #[sqlx::test]
    async fn delete_folder_removes_subtree_and_blobs(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let sub = t.make_folder(t.user_id, Some(docs), "sub").await;
        let (_, key_a) = t.store_file(t.user_id, Some(docs), "a.txt", b"a").await;
        let (_, key_b) = t.store_file(t.user_id, Some(sub), "b.txt", b"b").await;
        let (_, key_c) = t.store_file(t.user_id, None, "c.txt", b"c").await;

        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(docs),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        // 子樹裡的 blob 都刪了，資料夾外的 c.txt 不受影響
        assert!(!t.blob_exists(&key_a));
        assert!(!t.blob_exists(&key_b));
        assert!(t.blob_exists(&key_c));
        assert_eq!(t.blob_count(), 1);
        assert_eq!(t.row_count().await, 1);
    }

    #[sqlx::test]
    async fn delete_foreign_and_missing_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let (foreign, foreign_key) = t.store_file(alice, None, "secret.txt", b"secret").await;

        let foreign_res = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(foreign),
                Some(&t.cookie),
            ))
            .await;
        let missing_res = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(999_999),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(missing_res.0, StatusCode::NOT_FOUND);
        assert_eq!(
            (foreign_res.0, &foreign_res.2),
            (missing_res.0, &missing_res.2)
        );
        // alice 的檔案與 blob 完好
        assert_eq!(t.row_count().await, 1);
        assert!(t.blob_exists(&foreign_key));
    }

    #[sqlx::test]
    async fn deleting_twice_returns_404(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let first = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(id),
                Some(&t.cookie),
            ))
            .await;
        let second = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(id),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(first.0, StatusCode::NO_CONTENT);
        assert_eq!(second.0, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn already_missing_blob_still_succeeds(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;
        std::fs::remove_file(crate::storage::blob_path(t.dir.path(), &key).unwrap()).unwrap();

        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(id),
                Some(&t.cookie),
            ))
            .await;

        // blob 已經不在，刪除的目標本來就達成了
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(t.row_count().await, 0);
    }

    #[sqlx::test]
    async fn unauthenticated_delete_returns_401(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let (status, _, _) = t
            .send(empty_request(Method::DELETE, &item_uri(id), None))
            .await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(t.blob_exists(&key));
    }

    #[sqlx::test]
    async fn cross_site_delete_is_forbidden(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let req = request(Method::DELETE, &item_uri(id), Some(&t.cookie))
            .header(header::ORIGIN, "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = t.send(req).await;

        // DELETE 不是安全方法，受 Origin 檢查保護
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(t.row_count().await, 1);
        assert!(t.blob_exists(&key));
    }

    // ==================== 重新命名與搬移 ====================

    #[sqlx::test]
    async fn rename_returns_updated_item(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let (status, body) = patch(&t, id, r#"{"name":"b.txt"}"#).await;

        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        assert_eq!(body["name"], "b.txt");
        assert!(body.get("storage_key").is_none());
    }

    #[sqlx::test]
    async fn parent_id_three_states(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        // 數字：搬到資料夾
        let (_, body) = patch(&t, id, &format!(r#"{{"parent_id":{docs}}}"#)).await;
        assert_eq!(json(&body)["parent_id"], docs);

        // 沒有這個欄位：只改名，位置不變
        let (_, body) = patch(&t, id, r#"{"name":"b.txt"}"#).await;
        assert_eq!(json(&body)["parent_id"], docs);

        // null：搬到根目錄
        let (_, body) = patch(&t, id, r#"{"parent_id":null}"#).await;
        assert_eq!(json(&body)["parent_id"], Value::Null);
    }

    #[sqlx::test]
    async fn unknown_field_is_rejected(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        // 打錯欄位名稱：axum 的 Json 對「格式正確但內容不符型別」回 422
        let (status, _) = patch(&t, id, &format!(r#"{{"parentId":{docs}}}"#)).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        // 沒有默默變成「什麼都不改」的成功
        let (_, body) = patch(&t, id, r#"{"name":"a.txt"}"#).await;
        assert_eq!(json(&body)["parent_id"], Value::Null);
    }

    #[sqlx::test]
    async fn empty_update_is_400(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        assert_eq!(patch(&t, id, "{}").await.0, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn invalid_name_is_400(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        for name in ["", "..", "a/b", " a.txt"] {
            let body = format!(r#"{{"name":"{name}"}}"#);
            assert_eq!(
                patch(&t, id, &body).await.0,
                StatusCode::BAD_REQUEST,
                "{name:?}"
            );
        }
    }

    #[sqlx::test]
    async fn cycle_is_409(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let a = t.make_folder(t.user_id, None, "a").await;
        let b = t.make_folder(t.user_id, Some(a), "b").await;

        let (status, _) = patch(&t, a, &format!(r#"{{"parent_id":{b}}}"#)).await;

        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn name_conflict_is_409(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;
        t.store_file(t.user_id, None, "b.txt", b"y").await;

        assert_eq!(
            patch(&t, id, r#"{"name":"B.TXT"}"#).await.0,
            StatusCode::CONFLICT
        );
    }

    #[sqlx::test]
    async fn foreign_and_missing_item_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let (foreign, _) = t.store_file(alice, None, "secret.txt", b"x").await;

        let foreign_res = patch(&t, foreign, r#"{"name":"pwned.txt"}"#).await;
        let missing_res = patch(&t, 999_999, r#"{"name":"pwned.txt"}"#).await;

        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_res, missing_res);
    }

    #[sqlx::test]
    async fn foreign_and_missing_target_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let foreign_folder = t.make_folder(alice, None, "secret").await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let foreign_res = patch(&t, id, &format!(r#"{{"parent_id":{foreign_folder}}}"#)).await;
        let missing_res = patch(&t, id, r#"{"parent_id":999999}"#).await;

        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_res, missing_res);
    }

    #[sqlx::test]
    async fn cross_site_patch_is_forbidden(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let req = request(Method::PATCH, &item_uri(id), Some(&t.cookie))
            .header(header::ORIGIN, "http://evil.example")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"name":"pwned.txt"}"#))
            .unwrap();
        let (status, _, _) = t.send(req).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}
