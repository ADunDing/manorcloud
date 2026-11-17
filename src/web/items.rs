//! 對既有項目（檔案或資料夾）的操作：查詢、刪除（放進垃圾桶）、重新命名、搬移與複製。

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, post},
};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::FilesUser,
    files::{FileResponse, folder_not_found},
    state::AppState,
    storage::writable_root,
};
use crate::{
    db::files::{self as files_db, CreateFileError, FileEntry, TreeEntry, UpdateError},
    files::name::{numbered_name, validate_file_name},
    storage::{self, StorageError},
};

/// 「不存在」與「屬於別人」對外一律使用這個訊息
const ITEM_NOT_FOUND: &str = "Item not found";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/files/{id}",
            delete(delete_item).patch(patch_item).get(get_item),
        )
        .route("/api/files/{id}/copy", post(copy_item))
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
pub(super) struct ItemDetail {
    #[serde(flatten)]
    item: FileResponse,
    ancestors: Vec<Crumb>,
}

/// GET /api/files/{id}：項目資訊與由根目錄往下的祖先鏈（麵包屑導覽用）。
/// 不存在與屬於別人回傳相同的 404。
async fn get_item(
    user: FilesUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<ItemDetail>, AppError> {
    item_detail(&pool, user.id, id).await.map(Json)
}

/// `owner` 的一個項目與它的祖先鏈。自己的檔案，或管理員瀏覽別人的檔案（見 browse.rs）共用
pub(super) async fn item_detail(
    pool: &SqlitePool,
    owner: i64,
    id: i64,
) -> Result<ItemDetail, AppError> {
    let entry = files_db::find_file(pool, owner, id)
        .await?
        .ok_or_else(item_not_found)?;
    let ancestors = files_db::ancestors(pool, owner, id)
        .await?
        .into_iter()
        .map(|a| Crumb {
            id: a.id,
            name: a.name,
        })
        .collect();

    Ok(ItemDetail {
        item: entry.into(),
        ancestors,
    })
}

// ==================== 刪除 ====================

/// DELETE /api/files/{id}：把檔案或資料夾（連同底下所有內容）放進垃圾桶。
///
/// 內容與紀錄都還在，可以從垃圾桶還原；永久刪除在垃圾桶的 API 中（見 trash.rs），
/// 超過保留期限的由垃圾回收永久刪除。
/// 不存在、屬於別人、已經在垃圾桶中：都是同一個 404。
async fn delete_item(
    user: FilesUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if files_db::move_to_trash(&pool, user.id, id, now).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(item_not_found())
    }
}

/// 逐一刪除 blob，任何一個失敗都不中斷其他的；失敗的留給垃圾回收。
/// 呼叫前資料庫的紀錄必須已經刪除並 commit（見 `files_db::delete_tree` 的不變式）
pub(super) async fn remove_blobs(data_dir: &Path, keys: &[String]) {
    for key in keys {
        match storage::remove_blob(data_dir, key).await {
            Ok(()) => {}
            // 目標是「blob 不存在」，已經不在就是達成了
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("failed to remove blob {key} (garbage collection will retry): {e}"),
        }
    }
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
    user: FilesUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<UpdateRequest>,
) -> Result<Json<FileResponse>, AppError> {
    if req.name.is_none() && req.parent_id.is_none() {
        return Err(AppError::BadRequest("Nothing to change".to_string()));
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

// ==================== 複製 ====================

/// POST 的請求內容
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyRequest {
    /// 目的地資料夾；`null`（或沒有這個欄位）代表根目錄
    #[serde(default)]
    parent_id: Option<i64>,
}

/// POST /api/files/{id}/copy：把檔案或資料夾（連同底下所有內容）複製到另一個資料夾。
///
/// - 目的地有同名項目時，最上層的複本自動改名為「名稱 (1)」，與上傳相同
/// - 資料夾不能複製進它自己或它的子資料夾（會無窮地包含自己）
/// - 開始前檢查剩餘空間：不夠就直接拒絕，不會複製到一半才失敗
/// - 每個檔案都真的複製一份內容（見 `storage::copy_blob`）
///
/// 複製在獨立的工作中執行：使用者在途中關掉頁面，複製仍會完成或完整地復原，
/// 不會因為 handler 被取消而留下複製到一半的資料夾。
async fn copy_item(
    user: FilesUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<CopyRequest>,
) -> Result<(StatusCode, Json<FileResponse>), AppError> {
    if let Some(target) = req.parent_id
        && !files_db::is_owned_folder(&pool, user.id, target).await?
    {
        return Err(folder_not_found());
    }

    // 一條 SQL 取得整棵子樹：之後的複製都依照這個時間點的狀態
    let tree = files_db::subtree(&pool, user.id, id).await?;
    let Some(root) = tree.first() else {
        return Err(item_not_found());
    };

    if root.kind == "folder"
        && let Some(target) = req.parent_id
        && files_db::is_inside(&pool, user.id, root.id, target).await?
    {
        return Err(AppError::Conflict(
            "A folder can't be copied into itself or one of its subfolders".to_string(),
        ));
    }

    let needed = tree
        .iter()
        .filter_map(|entry| entry.size)
        .map(|size| u64::try_from(size).unwrap_or(0))
        .fold(0u64, u64::saturating_add);
    // 複本放在這個人的檔案所在的位置；正在搬到另一個位置時拒絕
    let root: Arc<Path> = Arc::from(writable_root(&pool, &data_dir, user.id).await?);
    let available = storage::available_bytes(&root).map_err(AppError::internal)?;
    if needed > available {
        return Err(not_enough_space());
    }

    let task = tokio::spawn(copy_tree(pool, root, user.id, tree, req.parent_id));
    let copy = task.await.map_err(AppError::internal)??;
    Ok((StatusCode::CREATED, Json(copy.into())))
}

/// 複製整棵子樹。失敗時刪掉已經建立的部分：要嘛得到完整的複本，要嘛什麼都沒有
async fn copy_tree(
    pool: SqlitePool,
    data_dir: Arc<Path>,
    owner: i64,
    tree: Vec<TreeEntry>,
    target: Option<i64>,
) -> Result<FileEntry, AppError> {
    let mut new_keys = Vec::new();
    let mut new_root = None;
    let result = copy_entries(
        &pool,
        &data_dir,
        owner,
        &tree,
        target,
        &mut new_keys,
        &mut new_root,
    )
    .await;

    if result.is_err() {
        // 先刪資料庫的紀錄，再刪 blob：與刪除項目相同的順序，中途失敗只會留下孤兒 blob
        if let Some(root) = new_root
            && let Err(e) = files_db::delete_tree(&pool, owner, root).await
        {
            eprintln!("failed to remove a partial copy (item {root}): {e}");
        }
        remove_blobs(&data_dir, &new_keys).await;
    }
    result
}

/// 依序建立每個項目的複本。子樹已經依深度排列，父資料夾一定比它的內容先建立
async fn copy_entries(
    pool: &SqlitePool,
    data_dir: &Path,
    owner: i64,
    tree: &[TreeEntry],
    target: Option<i64>,
    new_keys: &mut Vec<String>,
    new_root: &mut Option<i64>,
) -> Result<FileEntry, AppError> {
    // 原本的編號 → 複本的編號：子項目依此找到它的新父資料夾
    let mut copies: HashMap<i64, i64> = HashMap::new();
    let mut root_copy = None;

    for entry in tree {
        let is_root = entry.depth == 0;
        let parent = if is_root {
            target
        } else {
            let original = entry
                .parent_id
                .ok_or_else(|| AppError::internal("copy: an item inside the tree has no parent"))?;
            let copied = copies.get(&original).ok_or_else(|| {
                AppError::internal("copy: the parent folder was not copied first")
            })?;
            Some(*copied)
        };

        let content = if entry.kind == "folder" {
            None
        } else {
            let key = entry.storage_key.as_deref().ok_or_else(|| {
                AppError::internal(format!("file {} has no storage_key", entry.id))
            })?;
            let (new_key, size) = storage::copy_blob(data_dir, key)
                .await
                .map_err(copy_storage_error)?;
            new_keys.push(new_key.clone());
            Some((new_key, i64::try_from(size).map_err(AppError::internal)?))
        };

        let created = create_copy(
            pool,
            owner,
            parent,
            &entry.name,
            entry.mime.as_deref(),
            content.as_ref(),
            is_root,
        )
        .await?;

        copies.insert(entry.id, created.id);
        if is_root {
            *new_root = Some(created.id);
            root_copy = Some(created);
        }
    }

    root_copy.ok_or_else(item_not_found)
}

/// 建立一筆複本的紀錄（`content` 為 None 是資料夾）。
///
/// 最上層的項目遇到同名時自動改名為「名稱 (1)」、「名稱 (2)」⋯⋯；
/// 資料夾裡面的項目放進全新建立的資料夾，不會有同名的問題。
async fn create_copy(
    pool: &SqlitePool,
    owner: i64,
    parent: Option<i64>,
    name: &str,
    mime: Option<&str>,
    content: Option<&(String, i64)>,
    rename_on_conflict: bool,
) -> Result<FileEntry, AppError> {
    let mut candidate = name.to_string();
    // 編號的型別由 numbered_name 決定；最多嘗試到「名稱 (999)」
    let mut number = 0;
    loop {
        let result = match content {
            None => files_db::create_folder(pool, owner, parent, &candidate).await,
            Some((key, size)) => {
                files_db::create_file(pool, owner, parent, &candidate, *size, mime, key).await
            }
        };
        match result {
            Ok(entry) => return Ok(entry),
            Err(CreateFileError::NameConflict) if rename_on_conflict && number < 999 => {
                number += 1;
                candidate = numbered_name(name, number);
            }
            Err(e) => return Err(create_error(e)),
        }
    }
}

// ==================== 錯誤對應 ====================

fn item_not_found() -> AppError {
    AppError::NotFound(ITEM_NOT_FOUND.to_string())
}

fn not_enough_space() -> AppError {
    AppError::InsufficientStorage("Not enough storage space on the server".to_string())
}

/// 更新失敗的原因 → HTTP 狀態碼。
/// 循環回 409：同一個請求在不同的目錄狀態下可能合法也可能不合法，屬於「與目前狀態衝突」。
fn update_error(e: UpdateError) -> AppError {
    match e {
        UpdateError::NotFound => item_not_found(),
        UpdateError::TargetNotFolder => folder_not_found(),
        UpdateError::Cycle => AppError::Conflict(
            "A folder can't be moved into itself or one of its subfolders".to_string(),
        ),
        UpdateError::NameConflict => {
            AppError::Conflict("An item with this name already exists".to_string())
        }
        UpdateError::Db(e) => AppError::internal(e),
    }
}

/// 建立複本失敗的原因。目的地在複製途中被刪除時是 ParentNotFolder，與一開始就不存在相同
fn create_error(e: CreateFileError) -> AppError {
    match e {
        CreateFileError::NameConflict => {
            AppError::Conflict("An item with this name already exists".to_string())
        }
        CreateFileError::ParentNotFolder | CreateFileError::ParentNotOwned => folder_not_found(),
        CreateFileError::Db(e) => AppError::internal(e),
    }
}

fn copy_storage_error(e: StorageError) -> AppError {
    match e {
        StorageError::Io(ref io_error) if io_error.kind() == io::ErrorKind::StorageFull => {
            not_enough_space()
        }
        e => AppError::internal(e),
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

    async fn copy(t: &TestApp, id: i64, json: &str) -> (StatusCode, Bytes) {
        let req = request(
            Method::POST,
            &format!("/api/files/{id}/copy"),
            Some(&t.cookie),
        )
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
    async fn delete_moves_a_file_to_the_trash(pool: SqlitePool) {
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
        // 紀錄與內容都還在（可以還原），但一般的 API 已經找不到它
        assert_eq!(t.row_count().await, 1);
        assert!(t.blob_exists(&key));
        let (status, _, _) = t
            .send(empty_request(Method::GET, &item_uri(id), Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn delete_moves_a_folder_and_its_contents_to_the_trash(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let sub = t.make_folder(t.user_id, Some(docs), "sub").await;
        let (inner, key) = t.store_file(t.user_id, Some(sub), "b.txt", b"b").await;

        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &item_uri(docs),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(t.blob_exists(&key));
        // 裡面的內容也一起不見了
        for id in [sub, inner] {
            let (status, _, _) = t
                .send(empty_request(Method::GET, &item_uri(id), Some(&t.cookie)))
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        }
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

    // ==================== 複製 ====================

    #[sqlx::test]
    async fn copy_file_into_a_folder_is_independent(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"hello").await;

        let (status, body) = copy(&t, id, &format!(r#"{{"parent_id":{docs}}}"#)).await;

        assert_eq!(status, StatusCode::CREATED);
        let body = json(&body);
        assert_eq!(body["name"], "a.txt");
        assert_eq!(body["parent_id"], docs);
        assert_eq!(body["size"], 5);
        assert!(body.get("storage_key").is_none());
        assert_eq!(t.blob_count(), 2);

        // 永久刪除原本的檔案（放進垃圾桶、再從垃圾桶刪除），複本與它的內容仍然存在
        t.send(empty_request(
            Method::DELETE,
            &item_uri(id),
            Some(&t.cookie),
        ))
        .await;
        t.send(empty_request(
            Method::DELETE,
            &format!("/api/trash/{id}"),
            Some(&t.cookie),
        ))
        .await;
        assert!(!t.blob_exists(&key));
        assert_eq!(t.blob_count(), 1);
        assert_eq!(t.row_count().await, 2);
    }

    #[sqlx::test]
    async fn copy_into_the_same_folder_is_renamed(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let (_, first) = copy(&t, id, r#"{"parent_id":null}"#).await;
        let (_, second) = copy(&t, id, "{}").await;

        assert_eq!(json(&first)["name"], "a (1).txt");
        assert_eq!(json(&second)["name"], "a (2).txt");
    }

    #[sqlx::test]
    async fn copy_folder_copies_the_whole_subtree(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        // docs/
        //   a.txt
        //   sub/
        //     b.txt
        let docs = t.make_folder(t.user_id, None, "docs").await;
        let sub = t.make_folder(t.user_id, Some(docs), "sub").await;
        t.store_file(t.user_id, Some(docs), "a.txt", b"a").await;
        t.store_file(t.user_id, Some(sub), "b.txt", b"b").await;

        let (status, body) = copy(&t, docs, "{}").await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["name"], "docs (1)");
        assert_eq!(json(&body)["kind"], "folder");
        // 4 筆紀錄變成 8 筆，2 個 blob 變成 4 個
        assert_eq!(t.row_count().await, 8);
        assert_eq!(t.blob_count(), 4);
    }

    #[sqlx::test]
    async fn copy_folder_into_itself_is_409(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let a = t.make_folder(t.user_id, None, "a").await;
        let b = t.make_folder(t.user_id, Some(a), "b").await;

        let (into_self, _) = copy(&t, a, &format!(r#"{{"parent_id":{a}}}"#)).await;
        let (into_child, _) = copy(&t, a, &format!(r#"{{"parent_id":{b}}}"#)).await;

        assert_eq!(into_self, StatusCode::CONFLICT);
        assert_eq!(into_child, StatusCode::CONFLICT);
        assert_eq!(t.row_count().await, 2);
    }

    #[sqlx::test]
    async fn copy_foreign_and_missing_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let alice = t.other_user("alice").await;
        let (foreign, _) = t.store_file(alice, None, "secret.txt", b"x").await;
        let foreign_folder = t.make_folder(alice, None, "secret").await;
        let (own, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let foreign_item = copy(&t, foreign, "{}").await;
        let missing_item = copy(&t, 999_999, "{}").await;
        assert_eq!(foreign_item.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_item, missing_item);

        let foreign_target = copy(&t, own, &format!(r#"{{"parent_id":{foreign_folder}}}"#)).await;
        let missing_target = copy(&t, own, r#"{"parent_id":999999}"#).await;
        assert_eq!(foreign_target.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_target, missing_target);

        // 什麼都沒有被建立
        assert_eq!(t.blob_count(), 2);
    }

    #[sqlx::test]
    async fn copy_with_a_missing_blob_leaves_nothing_behind(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let docs = t.make_folder(t.user_id, None, "docs").await;
        t.store_file(t.user_id, Some(docs), "a.txt", b"a").await;
        let (_, broken) = t.store_file(t.user_id, Some(docs), "b.txt", b"b").await;
        // 模擬資料損壞：第二個檔案的 blob 不見了，複製會在途中失敗
        std::fs::remove_file(
            crate::storage::blob_path(&t.dir.path().join("carol"), &broken).unwrap(),
        )
        .unwrap();

        let (status, _) = copy(&t, docs, "{}").await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // 已經建立的資料夾與已經複製的 blob 都被復原了
        assert_eq!(t.row_count().await, 3);
        assert_eq!(t.blob_count(), 1);
    }
}
