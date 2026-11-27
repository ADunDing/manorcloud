//! 檔案的 API：My files 與各顆硬碟共用，以網址參數 `space=` 區分（見 `crate::space`）。
//! 路徑都是空間中的路徑，例如 `相片/2026/a.jpg`；空字串是空間的最上層。
//!
//!   GET    /api/files?path=                     資料夾的內容
//!   GET    /api/search?path=&q=                 在資料夾（含子資料夾）中依名稱搜尋
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
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    download::{ArchiveFormat, ArchiveItem, MAX_ARCHIVE_ITEMS, archive_response, file_response},
    error::AppError,
    extract::{CurrentUser, InSpace},
    state::{AppState, UploadLimit, WebDavSwitch},
};
use crate::{
    db::{trash as trash_db, user as user_db},
    quota::{self, QuotaError},
    space::{Space, SpaceKey},
    storage::{self, USERS_DIR},
    tree::{self, Entry, OnConflict, RelPath},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/files",
            // 順序很重要：MethodRouter::layer 只套用到呼叫當下已存在的方法。
            // 上傳不使用預設的請求大小上限：上限可以在管理頁面隨時修改，
            // 由 tree::Upload 一邊接收一邊檢查（超過時立刻停止，回 413）
            post(upload)
                .layer(DefaultBodyLimit::disable())
                .get(list)
                .patch(rename)
                .delete(remove),
        )
        .route("/api/files/copy", post(copy))
        .route("/api/search", get(search))
        .route("/api/files/content", get(download))
        .route("/api/archive", get(archive))
        .route("/api/folders", post(create_folder))
        .route("/api/config", get(config))
        .route("/api/me/preferences", get(preferences).put(set_preferences))
        .route("/api/storage", get(storage_info))
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

/// 新增 `extra` 位元組之前確認配額（見 `crate::quota`）。只有一般使用者自己的 My files 有配額
pub(super) async fn check_quota(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &CurrentUser,
    space: &Space,
    extra: u64,
) -> Result<(), AppError> {
    if space.key != SpaceKey::Me {
        return Ok(());
    }
    match quota::reserve(pool, data_dir, user.id, user.role == "admin", extra).await {
        Ok(()) => Ok(()),
        Err(QuotaError::Exceeded) => Err(AppError::InsufficientStorage(
            "This would go over your storage limit. Delete some files and empty the trash, or ask your administrator for more space."
                .to_string(),
        )),
        Err(QuotaError::Space(e)) => Err(AppError::internal(format!("{e:?}"))),
        Err(QuotaError::Db(e)) => Err(AppError::internal(e)),
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
    /// false：刪除就是永久刪除（分享給成員的位置沒有垃圾桶），前端刪除前要先確認
    trash: bool,
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
    let abs = space.abs(&dir)?;
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
        trash: space.keeps_trash,
        items,
    }))
}

// ==================== 搜尋 ====================

/// 一次搜尋最多回傳幾個項目
const SEARCH_LIMIT: usize = 500;

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    path: String,
    q: String,
}

#[derive(Serialize)]
struct FoundResponse {
    /// 空間中的完整路徑
    path: String,
    #[serde(flatten)]
    item: ItemResponse,
}

#[derive(Serialize)]
struct SearchResponse {
    items: Vec<FoundResponse>,
    /// 還有更多結果沒有列出（超過上限）
    more: bool,
}

/// GET /api/search?path=相片&q=旅行 2026：名稱包含每一個關鍵字的檔案與資料夾，上層的在前
async fn search(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, AppError> {
    let dir = parse_path(&query.path)?;
    let terms = tree::search_terms(&query.q);
    if terms.is_empty() {
        return Err(AppError::BadRequest(
            "Enter something to search for".to_string(),
        ));
    }
    let hidden = show_hidden(&pool, &user).await?;
    let root = space.root.clone();
    let abs = space.abs(&dir)?;
    let filter_space = space.clone();
    let filter_dir = dir.clone();
    let (found, more) = blocking(move || {
        let include = |parent: &RelPath, name: &str| {
            (hidden || !tree::is_hidden_name(name))
                && !filter_space.is_ignored(&filter_dir.join(parent), name)
        };
        tree::search(&root, &abs, &terms, &include, SEARCH_LIMIT)
    })
    .await?;
    let items = found
        .into_iter()
        .map(|f| {
            let path = dir.join(&f.path);
            let parent = path.parent().unwrap_or_default();
            FoundResponse {
                item: ItemResponse {
                    locked: space.is_locked(&parent, &f.entry.name),
                    kind: if f.entry.is_dir { "folder" } else { "file" },
                    size: (!f.entry.is_dir).then_some(f.entry.size),
                    modified: f.entry.modified,
                    name: f.entry.name,
                },
                path: path.display(),
            }
        })
        .collect();
    Ok(Json(SearchResponse { items, more }))
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
    let abs = space.abs(&rel)?;
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
    #[serde(default)]
    format: ArchiveFormat,
}

/// GET /api/archive?path=相片&names=a.jpg/2026&format=tar.gz：把選取的項目（連同資料夾底下的所有內容）
/// 邊讀邊打包成一個壓縮檔。`format` 是 zip（預設）、tar、tar.gz 或 tar.zst。
/// 用 GET：瀏覽器直接把它當成一般的檔案下載
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
    // 每個選取的項目各自經過 abs：在最上層選了 users 也會被擋下（見 Space::abs）
    let picked = names
        .iter()
        .map(|name| space.abs(&dir.child(name)))
        .collect::<Result<Vec<_>, _>>()?;
    let entries = blocking(move || {
        let include = |name: &str| hidden || !tree::is_hidden_name(name);
        let mut all = Vec::new();
        for item in &picked {
            all.extend(tree::walk(&root, item, &include)?);
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
    archive_response(items, &names, query.format)
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
    let abs = space.abs(&parent)?;
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
    let (abs_from, abs_parent) = (space.abs(&from)?, space.abs(&parent)?);
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
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    Json(req): Json<CopyRequest>,
) -> Result<(StatusCode, Json<NameResponse>), AppError> {
    let from = parse_path(&req.path)?;
    let parent = parse_path(&req.parent)?;
    let name = from.name().ok_or_else(item_not_found)?.to_string();
    space.check_write_in(&parent).map_err(denied)?;
    let name = check_name(&space, &parent, &name)?;
    let (root, abs) = (space.root.clone(), space.abs(&from)?);
    let size = blocking(move || Ok(tree::tree_size(&tree::resolve(&root, &abs, true)?))).await?;
    check_quota(&pool, &data_dir, &user, &space, size).await?;
    let root = space.root.clone();
    let tmp = space.tmp_dir();
    let (abs_from, abs_parent) = (space.abs(&from)?, space.abs(&parent)?);
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
    /// 垃圾桶中的編號（還原用）。分享給成員的位置沒有垃圾桶：已經永久刪除，是 null
    trash_id: Option<i64>,
}

/// DELETE /api/files?path=：放進垃圾桶；分享給成員的位置直接永久刪除（前端先確認過）
async fn remove(
    InSpace { space, .. }: InSpace,
    State(pool): State<SqlitePool>,
    Query(query): Query<PathQuery>,
) -> Result<Json<DeleteResponse>, AppError> {
    let rel = parse_path(&query.path)?;
    space.check_modify(&rel).map_err(denied)?;
    if !space.keeps_trash {
        let root = space.root.clone();
        let abs = space.abs(&rel)?;
        blocking(move || tree::remove_path(&tree::resolve(&root, &abs, true)?)).await?;
        return Ok(Json(DeleteResponse { trash_id: None }));
    }
    let trash_id = move_to_trash(&pool, &space, &rel).await?;
    Ok(Json(DeleteResponse {
        trash_id: Some(trash_id),
    }))
}

/// 把一個項目放進垃圾桶，回傳垃圾桶中的編號。呼叫前先用 `check_modify` 確認可以刪除
pub(super) async fn move_to_trash(
    pool: &SqlitePool,
    space: &Space,
    rel: &RelPath,
) -> Result<i64, AppError> {
    let root = space.root.clone();
    let abs = space.abs(rel)?;
    let path = blocking(move || tree::resolve(&root, &abs, true)).await?;

    // 先寫紀錄，再搬進垃圾桶：中途失敗只會留下一筆找不到檔案的紀錄
    let measured = path.clone();
    let (is_dir, size) = blocking(move || {
        let meta = std::fs::symlink_metadata(&measured)?;
        Ok((meta.is_dir(), tree::tree_size(&measured)))
    })
    .await?;
    let id = trash_db::add(pool, space.trash, &rel.display(), is_dir, size).await?;
    let target = storage::trash_path(&space.root, id);
    let moved = blocking(move || {
        std::fs::create_dir_all(target.parent().expect("trash_path has a parent"))?;
        std::fs::rename(&path, &target)
    })
    .await;
    if let Err(e) = moved {
        trash_db::remove(pool, id).await?;
        return Err(e);
    }
    Ok(id)
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
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(limit): State<UploadLimit>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<UploadResponse>), AppError> {
    let dir = parse_path(&query.path)?;
    space.check_write_in(&dir).map_err(denied)?;
    let name = check_name(&space, &dir, &query.name)?;
    // 一般上傳只用在小檔案：以請求的大小（略大於檔案）檢查配額
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    check_quota(&pool, &data_dir, &user, &space, declared).await?;
    let root = space.root.clone();
    let abs = space.abs(&dir)?;
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

    let mut upload = tree::Upload::create(&space.tmp_dir(), limit.get())
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
    /// 是否開放 WebDAV：設定頁面依此顯示應用程式密碼
    webdav: bool,
}

/// GET /api/config。需要登入：沒有理由對未登入的人公開伺服器的設定
async fn config(
    _user: CurrentUser,
    State(limit): State<UploadLimit>,
    State(webdav): State<WebDavSwitch>,
) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        max_upload_bytes: limit.get(),
        webdav: webdav.enabled(),
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
async fn storage_info(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
) -> Result<Json<StorageResponse>, AppError> {
    // 有配額的一般使用者：顯示配額與用量，而不是整顆硬碟
    if space.key == SpaceKey::Me
        && user.role != "admin"
        && let Some(limit) = user_db::quota(&pool, user.id).await?
    {
        let used = quota::usage(&pool, &data_dir, user.id)
            .await
            .map_err(|e| AppError::internal(format!("{e:?}")))?;
        return Ok(Json(StorageResponse {
            total_bytes: limit,
            available_bytes: limit.saturating_sub(used),
        }));
    }
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
    async fn archive_formats(pool: SqlitePool) {
        use std::io::Read;

        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir(home.join("docs")).unwrap();
        std::fs::write(home.join("docs/a.txt"), b"abc").unwrap();

        let download = |format: &str| {
            request(
                Method::GET,
                &format!("/api/archive?names=docs&format={format}"),
                Some(&t.cookie),
            )
            .body(Body::empty())
            .unwrap()
        };
        // 解壓縮後都是同一個 tar：資料夾 docs/，然後 docs/a.txt 與它的內容
        let check_tar = |tar: &[u8]| {
            assert_eq!(&tar[..5], b"docs/");
            assert_eq!(&tar[512..522], b"docs/a.txt");
            assert_eq!(&tar[1024..1027], b"abc");
        };

        let (status, headers, body) = t.send(download("tar")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/x-tar");
        assert!(
            headers[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .contains("filename=\"docs.tar\"")
        );
        check_tar(&body);

        let (status, headers, body) = t.send(download("tar.gz")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/gzip");
        let mut tar = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .read_to_end(&mut tar)
            .unwrap();
        check_tar(&tar);

        let (status, headers, body) = t.send(download("tar.zst")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/zstd");
        check_tar(&zstd::decode_all(&body[..]).unwrap());

        // 不支援的格式：400，而不是默默改用 zip
        let (status, _, _) = t.send(download("rar")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
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
    #[sqlx::test]
    async fn search_lists_matches_with_their_paths(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir_all(home.join("相片/旅行")).unwrap();
        std::fs::write(home.join("相片/旅行/Trip.jpg"), b"x").unwrap();
        std::fs::write(home.join("trip.txt"), b"x").unwrap();
        std::fs::write(home.join(".trip"), b"x").unwrap();

        let (status, body) = t.json(Method::GET, "/api/search?q=TRIP", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let paths: Vec<&str> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["trip.txt", "相片/旅行/Trip.jpg"]);
        assert_eq!(body["more"], false);

        let (_, body) = t
            .json(Method::GET, "/api/search?path=相片&q=trip", None)
            .await;
        assert_eq!(body["items"][0]["path"], "相片/旅行/Trip.jpg");
        assert_eq!(body["items"][0]["kind"], "file");

        let (status, _) = t.json(Method::GET, "/api/search?q=%20", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
