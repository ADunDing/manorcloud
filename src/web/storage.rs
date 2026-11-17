//! 儲存位置：每個使用者的檔案放在哪顆硬碟。
//!
//! - 給其他 handler 用：`files_root`（讀取、刪除）與 `writable_root`（新增檔案）
//! - 管理員 API：新增、刪除儲存位置，為使用者更換位置，查看搬移的進度
//!
//!   GET    /api/admin/storage                       所有位置、每個人在哪裡、搬移的進度
//!   POST   /api/admin/storage                       新增位置
//!   DELETE /api/admin/storage/{id}                  刪除位置（沒有人使用時）
//!   PUT    /api/admin/users/{id}/storage            更換使用者的位置
//!   POST   /api/admin/users/{id}/storage/retry      重試失敗的搬移
//!   DELETE /api/admin/users/{id}/storage/move       取消搬移，檔案留在原處

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{error::AppError, extract::AdminUser, state::AppState};
use crate::{
    db::storage::{
        self as storage_db, AddVolumeError, DeleteVolumeError, StartMove, StartMoveError,
    },
    folders, mover,
    storage::{self, resumable},
};

/// 名稱的長度上限，與 migration 的 CHECK 一致
const NAME_MAX: usize = 40;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/storage", get(overview).post(add_volume))
        .route("/api/admin/storage/{id}", delete(delete_volume))
        .route("/api/admin/users/{id}/storage", put(change_location))
        .route("/api/admin/users/{id}/storage/retry", post(retry))
        .route("/api/admin/users/{id}/storage/move", delete(cancel))
}

// ==================== 給其他 handler 用 ====================

/// 使用者的資料夾（`{位置}/{帳號名稱}`）：讀取、下載、刪除用。搬移期間仍是原本的位置
pub(super) async fn files_root(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
) -> Result<PathBuf, AppError> {
    Ok(location(pool, data_dir, user_id).await?.0)
}

/// 新增檔案（上傳、複製）用的目錄。搬移期間拒絕：
/// 新的檔案會寫進原本的位置，搬完之後就找不到了
pub(super) async fn writable_root(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
) -> Result<PathBuf, AppError> {
    let (root, moving) = location(pool, data_dir, user_id).await?;
    if moving {
        return Err(AppError::Conflict(
            "Your files are being moved to another disk. Try again when it's finished".to_string(),
        ));
    }
    // 第一次寫入時才建立資料夾。之後只多一次 stat
    if !tokio::fs::metadata(storage::blobs_dir(&root))
        .await
        .is_ok_and(|m| m.is_dir())
    {
        let dir = root.clone();
        tokio::task::spawn_blocking(move || storage::init_dirs(&dir))
            .await
            .map_err(AppError::internal)?
            .map_err(|e| AppError::internal(format!("cannot create {}: {e}", root.display())))?;
    }
    Ok(root)
}

async fn location(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
) -> Result<(PathBuf, bool), AppError> {
    let here = storage_db::user_location(pool, user_id)
        .await?
        // 已經通過身分驗證的使用者在這一刻被刪除了
        .ok_or_else(|| AppError::internal(format!("user {user_id} no longer exists")))?;
    // 硬碟沒有掛載：不能讓檔案寫到系統碟上空的掛載點（見 storage::VOLUME_MARKER）
    if let Some(path) = &here.path
        && !storage::volume_ready(Path::new(path))
    {
        eprintln!("storage: {path} is not available (is the disk mounted?)");
        return Err(disk_unavailable());
    }
    let root = folders::user_root(data_dir, &here).map_err(AppError::internal)?;
    Ok((root, here.moving))
}

fn disk_unavailable() -> AppError {
    AppError::Unavailable(
        "The disk where your files are kept isn't available right now. Ask your administrator"
            .to_string(),
    )
}

// ==================== 查看 ====================

/// 一個位置，以及它所在硬碟的容量
#[derive(Serialize)]
struct VolumeItem {
    /// None：資料目錄
    id: Option<i64>,
    name: String,
    path: String,
    /// 硬碟沒有掛載、或讀不到容量時為 None
    total_bytes: Option<u64>,
    available_bytes: Option<u64>,
    /// 有幾個使用者在這裡
    users: usize,
}

#[derive(Serialize)]
struct AssignmentItem {
    user_id: i64,
    username: String,
    volume_id: Option<i64>,
    /// 他在這個位置中的資料夾名稱（通常等於帳號名稱）
    folder: String,
}

#[derive(Serialize)]
struct MoveItem {
    user_id: i64,
    target_volume_id: Option<i64>,
    total_bytes: i64,
    moved_bytes: i64,
    /// 失敗的原因；None 代表進行中
    error: Option<String>,
}

#[derive(Serialize)]
struct Overview {
    /// 第一個是資料目錄
    volumes: Vec<VolumeItem>,
    users: Vec<AssignmentItem>,
    moves: Vec<MoveItem>,
}

/// GET /api/admin/storage
async fn overview(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
) -> Result<Json<Overview>, AppError> {
    let volumes = storage_db::list_volumes(&pool).await?;
    let users = storage_db::assignments(&pool).await?;
    let moves = storage_db::list_moves(&pool).await?;
    let count = |id: Option<i64>| users.iter().filter(|u| u.volume_id == id).count();

    let mut items = Vec::with_capacity(volumes.len() + 1);
    items.push(volume_item(
        None,
        "Default".to_string(),
        &data_dir,
        count(None),
    ));
    for v in volumes {
        let users = count(Some(v.id));
        items.push(volume_item(Some(v.id), v.name, Path::new(&v.path), users));
    }

    Ok(Json(Overview {
        volumes: items,
        users: users
            .into_iter()
            .map(|u| AssignmentItem {
                user_id: u.user_id,
                username: u.username,
                volume_id: u.volume_id,
                folder: u.folder,
            })
            .collect(),
        moves: moves
            .into_iter()
            .map(|m| MoveItem {
                user_id: m.user_id,
                target_volume_id: m.target_volume_id,
                total_bytes: m.total_bytes,
                moved_bytes: m.moved_bytes,
                error: m.error,
            })
            .collect(),
    }))
}

fn volume_item(id: Option<i64>, name: String, path: &Path, users: usize) -> VolumeItem {
    // 資料目錄沒有標記檔；其他位置沒有標記檔代表硬碟沒有掛載，容量顯示為無法使用
    let ready = id.is_none() || storage::volume_ready(path);
    let space = storage::disk_space(path).ok().filter(|_| ready);
    VolumeItem {
        id,
        name,
        path: path.display().to_string(),
        total_bytes: space.map(|s| s.total),
        available_bytes: space.map(|s| s.available),
        users,
    }
}

// ==================== 新增與刪除位置 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddRequest {
    name: String,
    path: String,
}

/// POST /api/admin/storage：新增儲存位置。
///
/// 路徑必須是已經存在、ManorCloud 可以寫入的資料夾（通常是硬碟的掛載點底下的資料夾）。
/// 存入的是解析過符號連結的真實路徑，之後連結被改掉也不會指到別的地方
async fn add_volume(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    Json(req): Json<AddRequest>,
) -> Result<(StatusCode, Json<VolumeItem>), AppError> {
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(format!(
            "The name must be 1–{NAME_MAX} characters"
        )));
    }
    if name.eq_ignore_ascii_case("default") {
        return Err(AppError::Conflict("That name is already taken".to_string()));
    }

    let existing = storage_db::list_volumes(&pool).await?;
    let mut taken: Vec<(String, PathBuf)> = existing
        .into_iter()
        .map(|v| (v.name, PathBuf::from(v.path)))
        .collect();
    taken.push(("Default".to_string(), data_dir.to_path_buf()));

    let raw = req.path.trim().to_string();
    let path = tokio::task::spawn_blocking(move || prepare_folder(&raw, &taken))
        .await
        .map_err(AppError::internal)??;
    let path_text = path
        .to_str()
        .ok_or_else(|| AppError::BadRequest("The path must be valid UTF-8".to_string()))?;

    let volume = storage_db::add_volume(&pool, &name, path_text)
        .await
        .map_err(|e| match e {
            AddVolumeError::NameTaken => {
                AppError::Conflict("That name is already taken".to_string())
            }
            AddVolumeError::PathTaken => {
                AppError::Conflict("That folder is already a storage location".to_string())
            }
            AddVolumeError::Db(e) => AppError::internal(e),
        })?;
    println!(
        "storage: location {} ({}) added by administrator {}",
        volume.name, volume.path, admin.id
    );

    Ok((
        StatusCode::CREATED,
        Json(volume_item(Some(volume.id), volume.name, &path, 0)),
    ))
}

/// 檢查資料夾並建立 tmp/、blobs/、uploads/，回傳解析過符號連結的絕對路徑。阻塞操作。
///
/// `taken`：既有的位置（含資料目錄）。新的位置不能在它們裡面，也不能包含它們：
/// 垃圾回收會把每個位置的 blobs/ 當成自己的，重疊時會把別人的檔案當成孤兒
fn prepare_folder(raw: &str, taken: &[(String, PathBuf)]) -> Result<PathBuf, AppError> {
    let bad = |m: &str| AppError::BadRequest(m.to_string());

    let given = Path::new(raw);
    if !given.is_absolute() {
        return Err(bad("Enter a full path that starts with /"));
    }
    let path = given.canonicalize().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => bad("That folder doesn't exist"),
        io::ErrorKind::PermissionDenied => bad("ManorCloud isn't allowed to open that folder"),
        _ => AppError::internal(format!("cannot resolve {raw}: {e}")),
    })?;
    if !path.is_dir() {
        return Err(bad("That path isn't a folder"));
    }

    for (name, other) in taken {
        // 資料目錄可能還不存在（測試）或是相對路徑：比較前先盡量解析
        let other = other.canonicalize().unwrap_or_else(|_| other.clone());
        if path.starts_with(&other) || other.starts_with(&path) {
            return Err(AppError::Conflict(format!(
                "That folder overlaps with the storage location {name}"
            )));
        }
    }

    // 實際寫入一次：權限、唯讀掛載、systemd 的 ReadWritePaths 限制都會在這裡出現。
    // 標記檔同時證明這裡寫得進去。每個人的資料夾在他第一次寫入時才建立
    let writable = storage::mark_volume(&path);
    if let Err(e) = writable {
        return Err(match e.kind() {
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => bad(
                "ManorCloud can't write to that folder. Make sure the account ManorCloud runs as \
                 (manorcloud when installed as a service) owns it, and allow the folder with \
                 `sudo systemctl edit manorcloud` (ReadWritePaths=)",
            ),
            _ => AppError::internal(format!("cannot prepare {}: {e}", path.display())),
        });
    }
    Ok(path)
}

/// DELETE /api/admin/storage/{id}：刪除儲存位置。資料夾本身留在磁碟上
async fn delete_volume(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let volume = storage_db::find_volume(&pool, id)
        .await?
        .ok_or_else(volume_not_found)?;
    let space = storage_db::delete_volume(&pool, id)
        .await
        .map_err(|e| match e {
            DeleteVolumeError::NotFound => volume_not_found(),
            DeleteVolumeError::InUse => AppError::Conflict(
                "Move everyone's files to another location before removing this one".to_string(),
            ),
            DeleteVolumeError::HasFiles => AppError::Conflict(format!(
                "Delete the files in {} first, including the ones in Trash",
                volume.name
            )),
            DeleteVolumeError::Db(e) => AppError::internal(e),
        })?;
    // 管理員在這裡的檔案空間（已經是空的）
    if let Some(folder) = space.and_then(|f| storage::user_root(Path::new(&volume.path), &f)) {
        folders::remove_user_folder(&folder).await;
    }
    println!(
        "storage: location {id} removed by administrator {}",
        admin.id
    );
    Ok(StatusCode::NO_CONTENT)
}

// ==================== 更換使用者的位置 ====================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeRequest {
    /// None：資料目錄
    volume_id: Option<i64>,
}

#[derive(Serialize)]
struct ChangeResponse {
    /// true：檔案正在背景搬移；false：沒有檔案，已經換好了
    moving: bool,
}

/// PUT /api/admin/users/{id}/storage：更換使用者的位置。
///
/// 有檔案時在背景搬移，這段期間這個人可以照常瀏覽與下載，但不能上傳或複製。
/// 他未完成的上傳會被取消（暫存檔在原本的位置）
async fn change_location(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(user_id): UrlPath<i64>,
    Json(req): Json<ChangeRequest>,
) -> Result<Json<ChangeResponse>, AppError> {
    let target = match req.volume_id {
        None => data_dir.to_path_buf(),
        Some(id) => {
            let path = PathBuf::from(
                storage_db::find_volume(&pool, id)
                    .await?
                    .ok_or_else(volume_not_found)?
                    .path,
            );
            if !storage::volume_ready(&path) {
                return Err(AppError::Conflict(
                    "That storage location isn't available. Is the disk connected?".to_string(),
                ));
            }
            path
        }
    };
    let from = storage_db::user_location(&pool, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".to_string()))?;
    let source = folders::user_root(&data_dir, &from).map_err(AppError::internal)?;

    // 目的地放不下時，在開始之前就拒絕，而不是搬到一半才失敗
    let needed: i64 = storage_db::move_keys(&pool, user_id)
        .await?
        .iter()
        .map(|(_, size)| *size)
        .sum();
    let available = storage::available_bytes(&target).map_err(|e| {
        AppError::Conflict(format!(
            "The storage location can't be reached ({e}). Is the disk connected?"
        ))
    })?;
    if u64::try_from(needed).unwrap_or(0) > available {
        return Err(AppError::InsufficientStorage(
            "There isn't enough free space on that storage location".to_string(),
        ));
    }

    let started = storage_db::start_move(&pool, user_id, req.volume_id)
        .await
        .map_err(|e| match e {
            StartMoveError::UserNotFound => AppError::NotFound("User not found".to_string()),
            StartMoveError::AlreadyThere => {
                AppError::Conflict("Their files are already there".to_string())
            }
            StartMoveError::InProgress => {
                AppError::Conflict("Their files are already being moved".to_string())
            }
            StartMoveError::FolderTaken => AppError::Conflict(
                "Someone else already has a folder with that name there".to_string(),
            ),
            StartMoveError::Db(e) => AppError::internal(e),
        })?;

    let (moving, cancelled) = match started {
        StartMove::Switched { cancelled_uploads } => {
            // 沒有檔案要搬：在新的位置建立他的資料夾，原本那個（空的）資料夾移除
            folders::create_user_folder(&pool, &data_dir, user_id).await;
            folders::remove_user_folder(&source).await;
            (false, cancelled_uploads)
        }
        StartMove::Started {
            move_id,
            cancelled_uploads,
        } => {
            mover::spawn(pool.clone(), Arc::clone(&data_dir), move_id);
            (true, cancelled_uploads)
        }
    };
    // 被取消的上傳：紀錄已經刪除，暫存檔在原本的位置。刪不掉的由垃圾回收處理
    for id in cancelled {
        if let Err(e) = resumable::remove_part(&source, &id).await {
            eprintln!("storage: cannot remove the cancelled upload {id}: {e}");
        }
    }

    println!(
        "storage: user {user_id} assigned to {} by administrator {}",
        target.display(),
        admin.id
    );
    Ok(Json(ChangeResponse { moving }))
}

/// POST /api/admin/users/{id}/storage/retry：重試失敗的搬移
async fn retry(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(user_id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let move_id = storage_db::retry_move(&pool, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("There's no failed move to retry".to_string()))?;
    mover::spawn(pool, data_dir, move_id);
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/admin/users/{id}/storage/move：取消搬移，檔案留在原本的位置。
/// 已經複製過去的部分由垃圾回收清除
async fn cancel(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(user_id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    if !storage_db::cancel_move(&pool, user_id).await? {
        return Err(AppError::NotFound(
            "Their files aren't being moved".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn volume_not_found() -> AppError {
    AppError::NotFound("Storage location not found".to_string())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request, request};
    use super::*;
    use axum::{
        body::{Body, Bytes},
        http::{HeaderMap, Method, header},
    };
    use serde_json::Value;
    use std::time::Duration;

    async fn make_admin(t: &TestApp) {
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
    }

    async fn send(
        t: &TestApp,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let req = request(method, uri, Some(&t.cookie))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        t.send(req).await
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    async fn add(t: &TestApp, dir: &Path) -> i64 {
        let body = format!(r#"{{"name":"Disk 2","path":"{}"}}"#, dir.display());
        let (status, _, body) = send(t, Method::POST, "/api/admin/storage", &body).await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
        json(&body)["id"].as_i64().unwrap()
    }

    /// 等背景的搬移做完
    async fn wait_for_moves(t: &TestApp) {
        for _ in 0..200 {
            if storage_db::list_moves(&t.pool).await.unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the move did not finish");
    }

    #[sqlx::test]
    async fn members_cannot_use_the_storage_api(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/storage",
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn adding_a_location_validates_the_folder(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let data = t.data_dir();
        std::fs::create_dir(data.join("inside")).unwrap();

        let cases = [
            ("relative/path".to_string(), StatusCode::BAD_REQUEST),
            ("/definitely/not/here".to_string(), StatusCode::BAD_REQUEST),
            // 資料目錄本身，以及它裡面的資料夾
            (data.display().to_string(), StatusCode::CONFLICT),
            (
                data.join("inside").display().to_string(),
                StatusCode::CONFLICT,
            ),
        ];
        for (path, expected) in cases {
            let body = format!(r#"{{"name":"x","path":"{path}"}}"#);
            let (status, _, _) = send(&t, Method::POST, "/api/admin/storage", &body).await;
            assert_eq!(status, expected, "{path}");
        }

        add(&t, disk.path()).await;
        // 標記檔已經建立
        assert!(storage::volume_ready(disk.path()));
        // 同一個資料夾不能加兩次
        let body = format!(r#"{{"name":"Other","path":"{}"}}"#, disk.path().display());
        let (status, _, _) = send(&t, Method::POST, "/api/admin/storage", &body).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn changing_location_moves_the_files(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let volume = add(&t, disk.path()).await;
        let alice = t.other_user("alice").await;
        let (_, key) = t.store_file(alice, None, "a.txt", b"hello").await;

        let uri = format!("/api/admin/users/{alice}/storage");
        let (status, _, body) = send(
            &t,
            Method::PUT,
            &uri,
            &format!(r#"{{"volume_id":{volume}}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["moving"], true);
        wait_for_moves(&t).await;

        // 新的位置有，原本的位置沒有了
        // 在 disk2 中 alice 自己的資料夾
        let folder = disk.path().canonicalize().unwrap().join("alice");
        assert!(storage::blob_path(&folder, &key).unwrap().exists());
        assert!(!t.blob_exists(&key));
        assert_eq!(
            files_root(&t.pool, &t.data_dir(), alice).await.unwrap(),
            folder
        );
        // 有人在使用的位置不能刪除
        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &format!("/api/admin/storage/{volume}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // 搬回資料目錄
        let (status, _, _) = send(&t, Method::PUT, &uri, r#"{"volume_id":null}"#).await;
        assert_eq!(status, StatusCode::OK);
        wait_for_moves(&t).await;
        assert!(t.blob_exists(&key));
    }

    #[sqlx::test]
    async fn new_files_are_refused_while_moving(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let volume = storage_db::add_volume(&t.pool, "disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        t.store_file(t.user_id, None, "a.txt", b"hello").await;
        // 直接建立搬移紀錄，不啟動背景工作：狀態固定在「搬移中」
        storage_db::start_move(&t.pool, t.user_id, Some(volume))
            .await
            .unwrap();

        assert!(matches!(
            writable_root(&t.pool, &t.data_dir(), t.user_id).await,
            Err(AppError::Conflict(_))
        ));
        // 讀取照常是原本的位置
        assert_eq!(
            files_root(&t.pool, &t.data_dir(), t.user_id).await.unwrap(),
            t.data_dir().join("carol")
        );

        let (status, _, _) = send(
            &t,
            Method::POST,
            "/api/uploads",
            r#"{"name":"b.txt","size":3}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn a_disk_that_is_not_mounted_is_never_written_to(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        // 掛載點還在，但硬碟沒有掛上：裡面沒有標記檔
        let mountpoint = tempfile::tempdir().unwrap();
        let volume = storage_db::add_volume(&t.pool, "disk2", mountpoint.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        sqlx::query("UPDATE users SET storage_volume_id = ? WHERE id = ?")
            .bind(volume)
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();

        let (status, _, _) = send(
            &t,
            Method::POST,
            "/api/uploads",
            r#"{"name":"b.txt","size":3}"#,
        )
        .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(std::fs::read_dir(mountpoint.path()).unwrap().count(), 0);
    }

    // ==================== 管理員在各硬碟上的檔案空間 ====================

    #[sqlx::test]
    async fn admin_files_on_another_disk_are_kept_apart(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = add(&t, disk.path()).await;
        let space = format!("space={volume}");

        // 在 Disk 2 建立資料夾、上傳檔案
        let (status, _, _) = send(
            &t,
            Method::POST,
            &format!("/api/folders?{space}"),
            r#"{"name":"Backup"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // Disk 2 看得到，自己的 My files 看不到
        let (_, _, body) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/files?{space}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(json(&body)[0]["name"], "Backup");
        let (_, _, body) = t
            .send(empty_request(Method::GET, "/api/files", Some(&t.cookie)))
            .await;
        assert_eq!(json(&body).as_array().unwrap().len(), 0);

        // 自己的檔案所在的位置（Default）就是 My files
        let (_, _, body) = t
            .send(empty_request(
                Method::GET,
                "/api/files?space=default",
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(json(&body).as_array().unwrap().len(), 0);

        // 檔案空間不是使用者：不出現在使用者清單中
        let (_, _, body) = t
            .send(empty_request(
                Method::GET,
                "/api/admin/users",
                Some(&t.cookie),
            ))
            .await;
        assert!(
            json(&body)
                .as_array()
                .unwrap()
                .iter()
                .all(|u| !u["username"].as_str().unwrap().starts_with('~'))
        );

        // 還有檔案時不能移除這個位置
        let uri = format!("/api/admin/storage/{volume}");
        let (status, _, _) = t
            .send(empty_request(Method::DELETE, &uri, Some(&t.cookie)))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn an_empty_space_is_removed_with_its_location(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        make_admin(&t).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = add(&t, disk.path()).await;
        // 打開一次 Disk 2：建立檔案空間
        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                &format!("/api/files?space={volume}"),
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _, _) = t
            .send(empty_request(
                Method::DELETE,
                &format!("/api/admin/storage/{volume}"),
                Some(&t.cookie),
            ))
            .await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        let spaces: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM storage_spaces")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert_eq!(spaces, 0);
    }

    #[sqlx::test]
    async fn members_cannot_use_spaces(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;

        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                "/api/files?space=default",
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // space=me 是自己的檔案，任何人都可以
        let (status, _, _) = t
            .send(empty_request(
                Method::GET,
                "/api/files?space=me",
                Some(&t.cookie),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
}
