//! 檔案的 API：My files 與各顆硬碟共用，以網址參數 `space=` 區分（見 `crate::space`）。
//! 路徑都是空間中的路徑，例如 `相片/2026/a.jpg`；空字串是空間的最上層。
//!
//!   GET    /api/files?path=                     資料夾的內容
//!   POST   /api/files?path=&name=&on_conflict=  一般上傳（multipart，欄位名稱 file）
//!   PATCH  /api/files                           改名，或搬到同一個空間中的另一個資料夾
//!   DELETE /api/files?path=                     刪除：放進垃圾桶
//!   POST   /api/files/copy                      複製到同一個空間中的另一個資料夾
//!   GET    /api/files/content?path=             下載（支援 Range）
//!   GET    /api/archive?path=&names=a/b         打包下載 path 底下的幾個項目
//!   POST   /api/folders                         建立資料夾
//!   GET    /api/storage                         空間所在硬碟的容量
//!   GET    /api/config                          前端需要知道的伺服器設定
//!   GET    /api/me/preferences                  自己的偏好設定（是否顯示隱藏檔）
//!   PUT    /api/me/preferences
//!
//! 所有檔案系統的操作都在背景執行緒進行（spawn_blocking），不會卡住其他請求。

use std::io;
use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Query, State, multipart::MultipartError},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    download::{ArchiveItem, MAX_ARCHIVE_ITEMS, archive_response, file_response},
    error::AppError,
    extract::{CurrentUser, InSpace},
    state::{AppState, UploadLimit},
};
use crate::{
    db::{trash as trash_db, user as user_db},
    space::Space,
    storage::{self, USERS_DIR},
    tree::{self, Entry, OnConflict, RelPath},
};

/// multipart 格式本身的額外開銷（邊界字串、欄位標頭）的預留空間。
/// 請求本體上限 = 檔案上限 + 這個值，否則剛好等於上限的合法檔案會被誤擋。
const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

pub fn router(upload_limit: UploadLimit) -> Router<AppState> {
    Router::new()
        .route(
            "/api/files",
            // 順序很重要：MethodRouter::layer 只套用到呼叫當下已存在的方法。
            // 放寬的上限只作用在上傳，其他方法維持預設
            post(upload)
                .layer(DefaultBodyLimit::max(body_limit(upload_limit)))
                .get(list)
                .patch(rename)
                .delete(remove),
        )
        .route("/api/files/copy", post(copy))
        .route("/api/files/content", get(download))
        .route("/api/archive", get(archive))
        .route("/api/folders", post(create_folder))
        .route("/api/config", get(config))
        .route("/api/me/preferences", get(preferences).put(set_preferences))
        .route("/api/storage", get(storage_info))
}

/// 計算請求本體上限。32 位元平台上 usize 放不下 10 GiB 的設定時，在啟動時停止
fn body_limit(UploadLimit(limit): UploadLimit) -> usize {
    let total = limit.saturating_add(MULTIPART_OVERHEAD);
    usize::try_from(total).unwrap_or_else(|_| {
        panic!(
            "MANORCLOUD_MAX_UPLOAD_BYTES ({limit}) is too large for this platform: \
             the limit plus the multipart overhead can't exceed {} bytes",
            usize::MAX
        )
    })
}

// ==================== 共用 ====================

pub(super) fn item_not_found() -> AppError {
    AppError::NotFound("Item not found".to_string())
}

pub(super) fn parse_path(text: &str) -> Result<RelPath, AppError> {
    RelPath::parse(text).map_err(|_| AppError::BadRequest("Invalid path".to_string()))
}

/// 不能修改時的錯誤
pub(super) fn denied(reason: String) -> AppError {
    AppError::Conflict(reason)
}

/// 新項目的名稱：一般的規則，另外硬碟最上層的 `users` 保留給每個人的資料夾
pub(super) fn check_name(space: &Space, dir: &RelPath, name: &str) -> Result<String, AppError> {
    let name = tree::check_new_name(name).map_err(AppError::BadRequest)?;
    if space.disk_root && dir.is_root() && name.eq_ignore_ascii_case(USERS_DIR) {
        return Err(AppError::BadRequest(
            "That name is reserved for people's folders".to_string(),
        ));
    }
    Ok(name)
}

/// 檔案系統的錯誤 → 給使用者看的錯誤
pub(super) fn io_error(e: io::Error) -> AppError {
    match e.kind() {
        io::ErrorKind::NotFound => item_not_found(),
        io::ErrorKind::AlreadyExists => AppError::Conflict(e.to_string()),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => AppError::Conflict(
            "ManorCloud isn't allowed to change this. Check the folder's permissions".to_string(),
        ),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            AppError::InsufficientStorage("Not enough storage space on the disk".to_string())
        }
        io::ErrorKind::FileTooLarge => {
            AppError::PayloadTooLarge("The file is larger than the upload limit".to_string())
        }
        io::ErrorKind::InvalidInput => AppError::BadRequest(e.to_string()),
        io::ErrorKind::CrossesDevices => {
            AppError::Conflict("That can't be moved across disks here. Copy it instead".to_string())
        }
        _ => AppError::internal(e),
    }
}

/// 在背景執行緒執行阻塞的檔案系統操作
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(AppError::internal)?
        .map_err(io_error)
}

/// 這個人要不要看到以點開頭的隱藏檔
pub(super) async fn show_hidden(pool: &SqlitePool, user: &CurrentUser) -> Result<bool, AppError> {
    Ok(user_db::show_hidden(pool, user.id).await?)
}

// ==================== 列表 ====================

#[derive(Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct ItemResponse {
    name: String,
    /// "file" 或 "folder"
    kind: &'static str,
    /// 資料夾沒有大小
    size: Option<u64>,
    /// 修改時間（Unix 秒）
    modified: i64,
    /// 不能改名、搬移或刪除（硬碟最上層的 users）
    locked: bool,
}

#[derive(Serialize)]
struct ListResponse {
    /// 這個資料夾不能修改時的原因；可以修改時是 null
    read_only: Option<String>,
    items: Vec<ItemResponse>,
}

/// GET /api/files?path=相片/2026
async fn list(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    Query(query): Query<PathQuery>,
) -> Result<Json<ListResponse>, AppError> {
    let dir = parse_path(&query.path)?;
    let hidden = show_hidden(&pool, &user).await?;
    let root = space.root.clone();
    let abs = space.abs(&dir);
    let entries: Vec<Entry> = blocking(move || tree::list(&root, &abs)).await?;
    let items = entries
        .into_iter()
        .filter(|e| hidden || !tree::is_hidden_name(&e.name))
        .filter(|e| !space.is_ignored(&dir, &e.name))
        .map(|e| ItemResponse {
            locked: space.is_locked(&dir, &e.name),
            kind: if e.is_dir { "folder" } else { "file" },
            size: (!e.is_dir).then_some(e.size),
            modified: e.modified,
            name: e.name,
        })
        .collect();
    Ok(Json(ListResponse {
        read_only: space.check_write_in(&dir).err(),
        items,
    }))
}

// ==================== 下載 ====================

/// GET /api/files/content?path=a.jpg：下載，支援 Range 與 If-Range
async fn download(
    InSpace { space, .. }: InSpace,
    Query(query): Query<PathQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let rel = parse_path(&query.path)?;
    let name = rel.name().ok_or_else(item_not_found)?.to_string();
    let root = space.root.clone();
    let abs = space.abs(&rel);
    let (file, meta, etag) = blocking(move || {
        let (file, meta) = tree::open_file(&root, &abs)?;
        let etag = tree::etag(&tree::resolve(&root, &abs, true)?, &meta);
        Ok((file, meta, etag))
    })
    .await?;
    file_response(
        tokio::fs::File::from_std(file),
        meta.len(),
        &name,
        &etag,
        &headers,
    )
    .await
}

#[derive(Deserialize)]
struct ArchiveQuery {
    /// 選取的項目所在的資料夾
    #[serde(default)]
    path: String,
    /// 選取的項目名稱，以 `/` 分隔（名稱中不會有 `/`）
    names: String,
}

/// GET /api/archive?path=相片&names=a.jpg/2026：把選取的項目（連同資料夾底下的所有內容）
/// 邊讀邊打包成一個 .zip。用 GET：瀏覽器直接把它當成一般的檔案下載
async fn archive(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    Query(query): Query<ArchiveQuery>,
) -> Result<Response, AppError> {
    let dir = parse_path(&query.path)?;
    let mut names: Vec<String> = Vec::new();
    for name in query.names.split('/') {
        let name = parse_path(name)?
            .name()
            .ok_or_else(|| AppError::BadRequest("Invalid path".to_string()))?
            .to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() || names.len() > MAX_ARCHIVE_ITEMS {
        return Err(AppError::BadRequest(format!(
            "Choose 1–{MAX_ARCHIVE_ITEMS} items to download"
        )));
    }

    let hidden = show_hidden(&pool, &user).await?;
    let root = space.root.clone();
    let abs = space.abs(&dir);
    let picked = names.clone();
    let entries = blocking(move || {
        let include = |name: &str| hidden || !tree::is_hidden_name(name);
        let mut all = Vec::new();
        for name in &picked {
            all.extend(tree::walk(&root, &abs.child(name), &include)?);
        }
        Ok(all)
    })
    .await?;
    let items = entries
        .into_iter()
        .map(|item| ArchiveItem {
            path: item.path,
            mtime: item.modified,
            source: (!item.is_dir).then_some(item.source),
        })
        .collect();
    archive_response(items, &names)
}

// ==================== 修改 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateFolderRequest {
    /// 在哪個資料夾中建立
    #[serde(default)]
    path: String,
    name: String,
}

#[derive(Serialize)]
pub(super) struct NameResponse {
    /// 實際使用的名稱（同名時可能加上了編號）
    pub name: String,
}

/// POST /api/folders
async fn create_folder(
    InSpace { space, .. }: InSpace,
    Json(req): Json<CreateFolderRequest>,
) -> Result<(StatusCode, Json<NameResponse>), AppError> {
    let parent = parse_path(&req.path)?;
    space.check_write_in(&parent).map_err(denied)?;
    let name = check_name(&space, &parent, &req.name)?;
    let root = space.root.clone();
    let abs = space.abs(&parent);
    let created = name.clone();
    blocking(move || tree::create_dir(&root, &abs, &created)).await?;
    Ok((StatusCode::CREATED, Json(NameResponse { name })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameRequest {
    path: String,
    /// 新的名稱；沒給時沿用原本的名稱（只搬移）
    name: Option<String>,
    /// 搬到哪個資料夾；沒給時留在原本的資料夾（只改名）
    parent: Option<String>,
}

#[derive(Serialize)]
struct PathResponse {
    /// 新的路徑
    path: String,
}

/// PATCH /api/files
async fn rename(
    InSpace { space, .. }: InSpace,
    Json(req): Json<RenameRequest>,
) -> Result<Json<PathResponse>, AppError> {
    let from = parse_path(&req.path)?;
    space.check_modify(&from).map_err(denied)?;
    let parent = match &req.parent {
        Some(parent) => parse_path(parent)?,
        None => from.parent().ok_or_else(item_not_found)?,
    };
    space.check_write_in(&parent).map_err(denied)?;
    let name = match &req.name {
        Some(name) => check_name(&space, &parent, name)?,
        None => {
            let current = from.name().ok_or_else(item_not_found)?;
            check_name(&space, &parent, current)?
        }
    };
    let moved = parent.child(&name);
    let root = space.root.clone();
    let (abs_from, abs_parent) = (space.abs(&from), space.abs(&parent));
    blocking(move || tree::rename(&root, &abs_from, &abs_parent, &name)).await?;
    Ok(Json(PathResponse {
        path: moved.display(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CopyRequest {
    path: String,
    /// 複製到哪個資料夾
    #[serde(default)]
    parent: String,
}

/// POST /api/files/copy：同名時自動改名（在原本的資料夾複製一份就是「名稱 (1)」）
async fn copy(
    InSpace { space, .. }: InSpace,
    Json(req): Json<CopyRequest>,
) -> Result<(StatusCode, Json<NameResponse>), AppError> {
    let from = parse_path(&req.path)?;
    let parent = parse_path(&req.parent)?;
    let name = from.name().ok_or_else(item_not_found)?.to_string();
    space.check_write_in(&parent).map_err(denied)?;
    let name = check_name(&space, &parent, &name)?;
    let root = space.root.clone();
    let tmp = space.tmp_dir();
    let (abs_from, abs_parent) = (space.abs(&from), space.abs(&parent));
    let copied = blocking(move || {
        let source = tree::resolve(&root, &abs_from, true)?;
        let target = tree::resolve_dir(&root, &abs_parent)?;
        let needed = tree::tree_size(&source);
        if needed > storage::available_bytes(&root)? {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "not enough space",
            ));
        }
        tree::copy(&source, &tmp, &target, &name)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(NameResponse { name: copied })))
}

#[derive(Serialize)]
struct DeleteResponse {
    /// 垃圾桶中的編號（還原用）
    trash_id: i64,
}

/// DELETE /api/files?path=：放進垃圾桶
async fn remove(
    InSpace { space, .. }: InSpace,
    State(pool): State<SqlitePool>,
    Query(query): Query<PathQuery>,
) -> Result<Json<DeleteResponse>, AppError> {
    let rel = parse_path(&query.path)?;
    space.check_modify(&rel).map_err(denied)?;
    let root = space.root.clone();
    let abs = space.abs(&rel);
    let path = blocking(move || tree::resolve(&root, &abs, true)).await?;

    // 先寫紀錄，再搬進垃圾桶：中途失敗只會留下一筆找不到檔案的紀錄
    let measured = path.clone();
    let (is_dir, size) = blocking(move || {
        let meta = std::fs::symlink_metadata(&measured)?;
        Ok((meta.is_dir(), tree::tree_size(&measured)))
    })
    .await?;
    let id = trash_db::add(&pool, space.trash, &rel.display(), is_dir, size).await?;
    let target = storage::trash_path(&space.root, id);
    let moved = blocking(move || {
        std::fs::create_dir_all(target.parent().expect("trash_path has a parent"))?;
        std::fs::rename(&path, &target)
    })
    .await;
    if let Err(e) = moved {
        trash_db::remove(&pool, id).await?;
        return Err(e);
    }
    Ok(Json(DeleteResponse { trash_id: id }))
}

// ==================== 一般上傳 ====================

#[derive(Deserialize)]
struct UploadQuery {
    /// 上傳到哪個資料夾
    #[serde(default)]
    path: String,
    name: String,
    #[serde(default)]
    on_conflict: OnConflict,
}

#[derive(Serialize)]
struct UploadResponse {
    name: String,
    size: u64,
}

/// POST /api/files?path=&name=&on_conflict=（multipart，第一個欄位必須是 file）。
/// 一次送完；大檔案用分段上傳（uploads.rs），可以續傳
async fn upload(
    InSpace { space, .. }: InSpace,
    State(UploadLimit(limit)): State<UploadLimit>,
    Query(query): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let dir = parse_path(&query.path)?;
    space.check_write_in(&dir).map_err(denied)?;
    let name = check_name(&space, &dir, &query.name)?;
    let root = space.root.clone();
    let abs = space.abs(&dir);
    let target_dir: PathBuf = blocking(move || tree::resolve_dir(&root, &abs)).await?;

    let mut field = multipart
        .next_field()
        .await
        .map_err(multipart_error)?
        .ok_or_else(|| AppError::BadRequest("The file field is missing".to_string()))?;
    if field.name() != Some("file") {
        return Err(AppError::BadRequest(
            "The first field must be the file field".to_string(),
        ));
    }

    let mut upload = tree::Upload::create(&space.tmp_dir(), limit)
        .await
        .map_err(io_error)?;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        upload.write_chunk(&chunk).await.map_err(io_error)?;
    }
    let size = upload.size();
    let placed = upload
        .finish(target_dir, name, query.on_conflict)
        .await
        .map_err(io_error)?;
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse { name: placed, size }),
    ))
}

/// multipart 解析錯誤 → HTTP 狀態碼。axum 只在讀取請求本體失敗（客戶端斷線）時回傳 5xx，
/// 那不是伺服器的錯誤，所以當作「上傳中斷」
pub(super) fn multipart_error(e: MultipartError) -> AppError {
    let status = e.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return AppError::PayloadTooLarge("The upload is larger than the size limit".to_string());
    }
    if status.is_client_error() {
        return AppError::BadRequest(format!("The upload is malformed: {}", e.body_text()));
    }
    eprintln!("upload interrupted (client connection problem): {e}");
    AppError::BadRequest("The upload was interrupted".to_string())
}

// ==================== 設定與容量 ====================

#[derive(Serialize)]
struct ConfigResponse {
    /// 單一檔案的上傳上限；前端用它在送出前先檢查，不必傳到一半才收到 413
    max_upload_bytes: u64,
}

/// GET /api/config。需要登入：沒有理由對未登入的人公開伺服器的設定
async fn config(
    _user: CurrentUser,
    State(UploadLimit(limit)): State<UploadLimit>,
) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        max_upload_bytes: limit,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    /// 是否顯示以點開頭的隱藏檔（例如 .DS_Store）
    show_hidden: bool,
}

/// GET /api/me/preferences
async fn preferences(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Preferences>, AppError> {
    Ok(Json(Preferences {
        show_hidden: show_hidden(&pool, &user).await?,
    }))
}

/// PUT /api/me/preferences
async fn set_preferences(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<Preferences>,
) -> Result<Json<Preferences>, AppError> {
    user_db::set_show_hidden(&pool, user.id, req.show_hidden).await?;
    Ok(Json(req))
}

#[derive(Serialize)]
struct StorageResponse {
    total_bytes: u64,
    available_bytes: u64,
}

/// GET /api/storage：空間所在硬碟的總容量與可用空間，給畫面左側的容量顯示
async fn storage_info(InSpace { space, .. }: InSpace) -> Result<Json<StorageResponse>, AppError> {
    let root = space.root.clone();
    let disk = blocking(move || storage::disk_space(&root)).await?;
    Ok(Json(StorageResponse {
        total_bytes: disk.total,
        available_bytes: disk.available,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, request};
    use super::*;
    use axum::{
        body::Body,
        http::{Method, header},
    };
    use serde_json::{Value, json};

    fn names(value: &Value) -> Vec<String> {
        let mut names: Vec<String> = value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    #[sqlx::test]
    async fn members_work_inside_their_own_folder(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");

        let (status, _) = t
            .json(Method::POST, "/api/folders", Some(json!({"name": "相片"})))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, body) = t.upload("path=相片&name=a.txt", b"hello").await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        assert_eq!(std::fs::read(home.join("相片/a.txt")).unwrap(), b"hello");
        let (_, body) = t.json(Method::GET, "/api/files?path=相片", None).await;
        assert_eq!(names(&body), ["a.txt"]);
        assert_eq!(body["items"][0]["size"], 5);
        assert_eq!(body["read_only"], Value::Null);

        // 其他空間：一般使用者不能使用硬碟的最上層
        let (status, _) = t.json(Method::GET, "/api/files?space=default", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn rename_move_copy_and_download(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir(home.join("docs")).unwrap();
        std::fs::write(home.join("a.txt"), b"abc").unwrap();

        let (status, body) = t
            .json(
                Method::PATCH,
                "/api/files",
                Some(json!({"path": "a.txt", "name": "b.txt", "parent": "docs"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["path"], "docs/b.txt");

        let (status, body) = t
            .json(
                Method::POST,
                "/api/files/copy",
                Some(json!({"path": "docs/b.txt", "parent": "docs"})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["name"], "b (1).txt");

        let req = request(
            Method::GET,
            "/api/files/content?path=docs/b.txt",
            Some(&t.cookie),
        )
        .header(header::RANGE, "bytes=1-")
        .body(Body::empty())
        .unwrap();
        let (status, headers, body) = t.send(req).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(&body[..], b"bc");
        assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");

        let (status, _, body) = t
            .send(
                request(Method::GET, "/api/archive?names=docs", Some(&t.cookie))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..4], b"PK\x03\x04");
    }

    #[sqlx::test]
    async fn deleting_goes_to_the_trash(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir(home.join("docs")).unwrap();
        std::fs::write(home.join("docs/a.txt"), b"abc").unwrap();

        let (status, body) = t.json(Method::DELETE, "/api/files?path=docs", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["trash_id"].is_i64());

        assert!(!home.join("docs").exists());
        let (id, path, size): (i64, String, i64) =
            sqlx::query_as("SELECT id, path, size FROM trash")
                .fetch_one(&t.pool)
                .await
                .unwrap();
        assert_eq!((path.as_str(), size), ("docs", 3));
        assert!(
            t.data_dir()
                .join(format!(".manorcloud/trash/{id}/a.txt"))
                .is_file()
        );
    }

    #[sqlx::test]
    async fn hidden_files_follow_the_setting(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::write(home.join(".DS_Store"), b"x").unwrap();
        std::fs::write(home.join("a.txt"), b"x").unwrap();

        let (_, body) = t.json(Method::GET, "/api/files", None).await;
        assert_eq!(names(&body), ["a.txt"]);

        let (status, _) = t
            .json(
                Method::PUT,
                "/api/me/preferences",
                Some(json!({"show_hidden": true})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = t.json(Method::GET, "/api/files", None).await;
        assert_eq!(names(&body), [".DS_Store", "a.txt"]);
    }

    #[sqlx::test]
    async fn administrators_see_the_disk_and_cannot_change_others(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let jerry = t.home("jerry");
        std::fs::write(jerry.join("x.jpg"), b"x").unwrap();
        std::fs::write(t.data_dir().join("報告.pdf"), b"r").unwrap();
        std::fs::create_dir(t.data_dir().join("lost+found")).unwrap();

        let (status, body) = t.json(Method::GET, "/api/files", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(names(&body), ["users", "報告.pdf"]);
        let users = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "users")
            .unwrap();
        assert_eq!(users["locked"], true);

        // users/jerry：可以看、可以下載，不能修改
        let (_, body) = t
            .json(Method::GET, "/api/files?path=users/jerry", None)
            .await;
        assert_eq!(names(&body), ["x.jpg"]);
        assert!(body["read_only"].is_string());
        let (status, _) = t
            .json(Method::DELETE, "/api/files?path=users/jerry/x.jpg", None)
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = t.json(Method::DELETE, "/api/files?path=users", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = t.upload("path=users/jerry&name=y.txt", b"y").await;
        assert_eq!(status, StatusCode::CONFLICT);
        // 最上層不能建立另一個 users
        let (status, _) = t
            .json(Method::POST, "/api/folders", Some(json!({"name": "Users"})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(jerry.join("x.jpg").exists());
    }

    #[sqlx::test]
    async fn paths_cannot_escape_the_space(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), home.join("link")).unwrap();
        std::fs::write(t.home("jerry").join("j.txt"), b"j").unwrap();

        for path in [
            "../jerry/j.txt",
            "link/secret",
            "%2E%2E/jerry/j.txt",
            ".manorcloud",
        ] {
            let (status, _) = t
                .json(
                    Method::GET,
                    &format!("/api/files/content?path={path}"),
                    None,
                )
                .await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND,
                "{path}: {status}"
            );
        }
    }
}
