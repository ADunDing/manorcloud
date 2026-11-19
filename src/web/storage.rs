//! 儲存位置：每個人的檔案放在哪顆硬碟。
//!
//!   GET    /api/admin/storage                       所有位置、每個人在哪裡、搬移的進度
//!   POST   /api/admin/storage                       新增位置
//!   DELETE /api/admin/storage/{id}                  刪除位置（沒有人使用、硬碟上沒有檔案時）
//!   PUT    /api/admin/users/{id}/storage            更換使用者的位置
//!   POST   /api/admin/users/{id}/storage/retry      重試失敗的搬移
//!   DELETE /api/admin/users/{id}/storage/move       取消搬移，檔案留在原處
//!
//! 一般使用者換位置時，他的 `users/{名稱}` 與垃圾桶在背景複製過去（見 mover.rs）；
//! 管理員的 My files 是硬碟的最上層，換位置只是改成以另一顆硬碟作為他的 My files，不搬任何檔案。

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

use super::{error::AppError, extract::AdminUser, spaces::blocking, state::AppState};
use crate::{
    db::{
        storage::{
            self as storage_db, AddVolumeError, DeleteVolumeError, StartMove, StartMoveError,
        },
        trash as trash_db,
    },
    folders, mover,
    storage::{self, USERS_DIR, resumable},
    tree,
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
/// 路徑必須是已經存在、ManorCloud 可以寫入的資料夾（通常是硬碟的掛載點，或底下的資料夾）。
/// 裡面原本就有的檔案會出現在這顆硬碟的最上層。
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

    // 不能和既有的位置（含資料目錄）重疊：同一個資料夾會同時出現在兩顆「硬碟」上
    let mut taken: Vec<(String, PathBuf)> = storage_db::list_volumes(&pool)
        .await?
        .into_iter()
        .map(|v| (format!("the storage {}", v.name), PathBuf::from(v.path)))
        .collect();
    taken.push(("the storage Default".to_string(), data_dir.to_path_buf()));

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
                AppError::Conflict("That folder is already a storage".to_string())
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

/// 檢查資料夾並建立標記檔，回傳解析過符號連結的絕對路徑。阻塞操作。
/// `taken`：不能重疊的資料夾（說明, 路徑）
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
    if path == Path::new("/") {
        return Err(bad("The whole system disk can't be a storage"));
    }

    for (what, other) in taken {
        // 資料目錄可能還不存在（測試）或是相對路徑：比較前先盡量解析
        let other = other.canonicalize().unwrap_or_else(|_| other.clone());
        if path.starts_with(&other) || other.starts_with(&path) {
            return Err(AppError::Conflict(format!(
                "That folder overlaps with {what}"
            )));
        }
    }

    // 實際寫入一次：權限、唯讀掛載、systemd 的 ReadWritePaths 限制都會在這裡出現
    let writable = storage::mark_volume(&path).and_then(|()| storage::init_dirs(&path));
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

/// 硬碟上還有沒有檔案（`.manorcloud`、`lost+found` 與空的 `users` 不算）
fn has_files(root: &Path) -> io::Result<bool> {
    for item in std::fs::read_dir(root)? {
        let name = item?.file_name();
        let name = name.to_string_lossy();
        if tree::is_system_name(&name) || name == "lost+found" {
            continue;
        }
        if name == USERS_DIR
            && std::fs::read_dir(root.join(USERS_DIR))?.all(|user| {
                user.is_ok_and(|user| {
                    std::fs::read_dir(user.path()).is_ok_and(|mut items| items.next().is_none())
                })
            })
        {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// DELETE /api/admin/storage/{id}：刪除儲存位置。只有沒有人使用、硬碟上也沒有檔案時可以刪除；
/// 只移除 ManorCloud 自己的 `.manorcloud` 與空的資料夾
async fn delete_volume(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let volume = storage_db::find_volume(&pool, id)
        .await?
        .ok_or_else(volume_not_found)?;
    let root = PathBuf::from(&volume.path);
    let trashed = !trash_db::on_volume(&pool, Some(id)).await?.is_empty();
    let checked = root.clone();
    let files = storage::volume_ready(&root) && blocking(move || has_files(&checked)).await?;
    if files || trashed {
        return Err(AppError::Conflict(format!(
            "Delete the files in {} first, including the ones in Trash",
            volume.name
        )));
    }
    storage_db::delete_volume(&pool, id)
        .await
        .map_err(|e| match e {
            DeleteVolumeError::NotFound => volume_not_found(),
            DeleteVolumeError::InUse => AppError::Conflict(
                "Move everyone's files to another storage before removing this one".to_string(),
            ),
            DeleteVolumeError::Db(e) => AppError::internal(e),
        })?;
    if storage::volume_ready(&root) {
        let _ = blocking(move || {
            let users = root.join(USERS_DIR);
            if let Ok(items) = std::fs::read_dir(&users) {
                for item in items.flatten() {
                    let _ = std::fs::remove_dir(item.path());
                }
                let _ = std::fs::remove_dir(&users);
            }
            std::fs::remove_dir_all(storage::system_dir(&root))
        })
        .await;
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
    /// true：檔案正在背景搬移；false：已經換好了
    moving: bool,
}

/// PUT /api/admin/users/{id}/storage：更換使用者的位置。
///
/// 一般使用者有檔案時在背景搬移，這段期間他可以照常瀏覽與下載，但不能修改。
/// 他上傳到 My files、還沒完成的上傳會被取消（暫存檔在原本的位置）
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
                    "That storage isn't available. Is the disk connected?".to_string(),
                ));
            }
            path
        }
    };
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(&pool)
        .await?;
    let Some(role) = role else {
        return Err(AppError::NotFound("User not found".to_string()));
    };
    let from = storage_db::user_location(&pool, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".to_string()))?;
    if from.volume_id == req.volume_id {
        return Err(AppError::Conflict(
            "Their files are already there".to_string(),
        ));
    }

    // 管理員：只是換一顆硬碟作為他的 My files
    if role == "admin" {
        storage_db::set_volume(&pool, user_id, req.volume_id).await?;
        println!(
            "storage: administrator {user_id} now uses {} as My files (by administrator {})",
            target.display(),
            admin.id
        );
        return Ok(Json(ChangeResponse { moving: false }));
    }

    let old_root = folders::volume_root(&pool, &data_dir, from.volume_id)
        .await
        .map_err(AppError::internal)?;
    let source = storage::user_dir(&old_root, &from.folder)
        .ok_or_else(|| AppError::internal("invalid folder name"))?;

    // 要搬的大小：My files 與垃圾桶。目的地放不下時，在開始之前就拒絕
    let trash = trash_db::of_owner(&pool, user_id).await?;
    let measured = source.clone();
    let (has_any, mut needed) = blocking(move || {
        let has = std::fs::read_dir(&measured).is_ok_and(|mut items| items.next().is_some());
        Ok((has, tree::tree_size(&measured)))
    })
    .await?;
    needed += trash
        .iter()
        .map(|item| u64::try_from(item.size).unwrap_or(0))
        .sum::<u64>();
    let available = storage::available_bytes(&target).map_err(|e| {
        AppError::Conflict(format!(
            "The storage can't be reached ({e}). Is the disk connected?"
        ))
    })?;
    if needed > available {
        return Err(AppError::InsufficientStorage(
            "There isn't enough free space on that storage".to_string(),
        ));
    }
    let total = (has_any || !trash.is_empty()).then(|| i64::try_from(needed).unwrap_or(i64::MAX));

    let started = storage_db::start_move(&pool, user_id, req.volume_id, total)
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
            let _ = tokio::fs::remove_dir(&source).await;
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
        if let Err(e) = resumable::remove_part(&storage::uploads_dir(&old_root), &id).await {
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
/// 已經複製過去的部分在目的地的 .manorcloud/tmp 中，由垃圾回收清除
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
    AppError::NotFound("Storage not found".to_string())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use super::*;
    use axum::http::Method;
    use serde_json::json;
    use std::time::Duration;

    async fn add(t: &TestApp, dir: &Path) -> i64 {
        let (status, body) = t
            .json(
                Method::POST,
                "/api/admin/storage",
                Some(json!({"name": "Disk 2", "path": dir})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_i64().unwrap()
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
        let (status, _) = t.json(Method::GET, "/api/admin/storage", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn adding_a_location_validates_the_folder(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let disk = tempfile::tempdir().unwrap();
        let data = t.data_dir();
        std::fs::create_dir(data.join("inside")).unwrap();

        let cases = [
            ("relative/path".into(), StatusCode::BAD_REQUEST),
            ("/definitely/not/here".into(), StatusCode::BAD_REQUEST),
            // 資料目錄本身，以及它裡面的資料夾
            (data.clone(), StatusCode::CONFLICT),
            (data.join("inside"), StatusCode::CONFLICT),
        ];
        for (path, expected) in cases {
            let (status, _) = t
                .json(
                    Method::POST,
                    "/api/admin/storage",
                    Some(json!({"name": "x", "path": path})),
                )
                .await;
            assert_eq!(status, expected, "{}", path.display());
        }

        add(&t, disk.path()).await;
        // 標記檔已經建立
        assert!(storage::volume_ready(disk.path()));
        let (status, _) = t
            .json(
                Method::POST,
                "/api/admin/storage",
                Some(json!({"name": "Other", "path": disk.path()})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn changing_location_moves_the_files_and_the_trash(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let disk = tempfile::tempdir().unwrap();
        let volume = add(&t, disk.path()).await;
        let alice = t.other_user("alice").await;
        let home = t.home("alice");
        std::fs::create_dir(home.join("相片")).unwrap();
        std::fs::write(home.join("相片/a.txt"), b"hello").unwrap();
        // 垃圾桶中的一個檔案
        let trashed = trash_db::add(
            &t.pool,
            trash_db::Place {
                volume_id: None,
                owner_id: Some(alice),
            },
            "b.txt",
            false,
            3,
        )
        .await
        .unwrap();
        std::fs::create_dir_all(storage::trash_dir(&t.data_dir())).unwrap();
        std::fs::write(storage::trash_path(&t.data_dir(), trashed), b"bye").unwrap();

        let (status, body) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{alice}/storage"),
                Some(json!({"volume_id": volume})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["moving"], true);
        wait_for_moves(&t).await;

        let disk = disk.path().canonicalize().unwrap();
        assert_eq!(
            std::fs::read(disk.join("users/alice/相片/a.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            std::fs::read(storage::trash_path(&disk, trashed)).unwrap(),
            b"bye"
        );
        assert!(!home.exists());
        assert!(!storage::trash_path(&t.data_dir(), trashed).exists());

        // 有人在使用的位置不能刪除
        let (status, _) = t
            .json(
                Method::DELETE,
                &format!("/api/admin/storage/{volume}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // 搬回資料目錄
        let (status, _) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{alice}/storage"),
                Some(json!({"volume_id": null})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        wait_for_moves(&t).await;
        assert!(t.data_dir().join("users/alice/相片/a.txt").is_file());
    }

    #[sqlx::test]
    async fn administrators_switch_without_moving(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let disk = tempfile::tempdir().unwrap();
        let volume = add(&t, disk.path()).await;
        std::fs::write(t.data_dir().join("mine.txt"), b"x").unwrap();

        let (status, body) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{}/storage", t.user_id),
                Some(json!({"volume_id": volume})),
            )
            .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["moving"], false);
        // 檔案留在原處；My files 現在是 Disk 2 的最上層
        assert!(t.data_dir().join("mine.txt").exists());
        let (_, body) = t.json(Method::GET, "/api/files", None).await;
        assert_eq!(body["items"], json!([]));
    }

    #[sqlx::test]
    async fn changes_are_refused_while_moving(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let volume = storage_db::add_volume(&t.pool, "disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        std::fs::write(t.home("carol").join("a.txt"), b"x").unwrap();
        // 直接建立搬移紀錄，不啟動背景工作：狀態固定在「搬移中」
        storage_db::start_move(&t.pool, t.user_id, Some(volume), Some(1))
            .await
            .unwrap();

        let (status, body) = t.json(Method::GET, "/api/files", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["read_only"].is_string());
        let (status, _) = t
            .json(
                Method::POST,
                "/api/uploads",
                Some(json!({"name": "b.txt", "size": 3})),
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

        let (status, _) = t
            .json(
                Method::POST,
                "/api/uploads",
                Some(json!({"name": "b.txt", "size": 3})),
            )
            .await;

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(std::fs::read_dir(mountpoint.path()).unwrap().count(), 0);
    }

    #[sqlx::test]
    async fn a_location_with_files_cannot_be_removed(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let disk = tempfile::tempdir().unwrap();
        let volume = add(&t, disk.path()).await;
        std::fs::write(disk.path().join("報告.pdf"), b"x").unwrap();

        let uri = format!("/api/admin/storage/{volume}");
        let (status, _) = t.json(Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::CONFLICT);

        std::fs::remove_file(disk.path().join("報告.pdf")).unwrap();
        let (status, _) = t.json(Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!disk.path().join(".manorcloud").exists());
    }
}
