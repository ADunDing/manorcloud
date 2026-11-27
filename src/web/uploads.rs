//! 分段上傳（可續傳）的 API。
//!
//! 1. POST   /api/uploads?space=        宣告：資料夾、檔名、總大小 → 取得上傳編號
//! 2. PUT    /api/uploads/{id}?offset=N 寫入從第 N 個位元組開始的一段，回傳新的已確認長度；
//!    好幾段可以同時送，不必按照順序
//! 3. POST   /api/uploads/{id}/complete 完成：放到資料夾中的正式名稱上
//!    GET    /api/uploads/{id}          查詢已確認的長度（斷線後從哪裡繼續）
//!    GET    /api/uploads?space=        列出自己在這個空間中未完成的上傳（重新整理後續傳）
//!    DELETE /api/uploads/{id}          取消
//!
//! 檔案與資料庫的先後順序，處處都是「檔案先、紀錄後」：
//! - 宣告：先建立暫存檔，再寫入紀錄。有紀錄就一定有檔案
//! - 寫入：先寫入並 fsync，再推進已確認的長度。已確認的部分一定已經落盤
//! - 完成：先放到正式的名稱上，再刪除紀錄。放不上去（同名）時暫存檔與紀錄都留著，換個名稱再完成
//! - 取消：例外，先刪紀錄、再刪檔案。反過來的話，斷電在兩步之間會留下「有紀錄、沒檔案」
//!
//! 寫入與完成都是「先拿暫存檔的鎖，再讀資料庫的紀錄」，理由見 lock_part。
//!
//! 暫存檔放在空間所在硬碟的 `.manorcloud/uploads/`，和目的地在同一個檔案系統，完成時只是改名。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as UrlPath, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{post, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::{CurrentUser, InSpace, resolve_space},
    spaces::{blocking, check_name, check_quota, denied, parse_path},
    state::{AppState, PendingUploadLimit, UploadLimit},
    upload_progress::UploadProgress,
};

use crate::{
    db::uploads::{self as uploads_db, CreateUploadError, NewUpload, Upload},
    space::{Space, SpaceKey},
    storage::{
        self,
        resumable::{PartError, PartLock, create_part, remove_part},
    },
    tree::{self, OnConflict, RelPath},
};

/// 每段的大小上限。前端每段送 8 到 64 MiB（依網路速度調整），這裡留一倍的餘裕
const MAX_CHUNK_BYTES: usize = 128 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/uploads", post(create).get(list))
        .route(
            "/api/uploads/{id}",
            // 順序很重要：layer 只套用到呼叫當下已存在的方法（見 spaces::router 的說明）。
            // 放寬的上限只給續寫，查詢與取消維持預設
            put(append)
                .layer(DefaultBodyLimit::max(MAX_CHUNK_BYTES))
                .get(status)
                .delete(cancel),
        )
        .route("/api/uploads/{id}/complete", post(complete))
}

// ==================== 回應與錯誤 ====================

/// 一筆未完成的上傳
#[derive(Serialize)]
struct UploadResponse {
    id: String,
    /// 上傳到哪個空間（與網址的 space= 相同）
    space: String,
    /// 空間中的哪個資料夾
    path: String,
    name: String,
    size: i64,
    received: i64,
    file_modified: Option<i64>,
    updated_at: i64,
}

impl From<Upload> for UploadResponse {
    fn from(u: Upload) -> Self {
        UploadResponse {
            id: u.id,
            space: u.space,
            path: u.dir,
            name: u.name,
            size: u.size,
            received: u.received,
            file_modified: u.file_modified,
            updated_at: u.updated_at,
        }
    }
}

#[derive(Serialize)]
struct ProgressResponse {
    received: i64,
}

/// 上傳的錯誤：一般的 AppError，或分段上傳特有、前端需要分別處理的情況。
/// 各有自己的代碼，而不是共用 conflict：前端對每一種的反應都不同
enum UploadError {
    App(AppError),
    /// offset 與伺服器已確認的長度不符：附上正確的值，前端從那裡重新送
    OffsetMismatch {
        received: i64,
    },
    /// 另一個請求正在寫入同一個上傳（例如另一個分頁）
    Busy,
    /// 暫存資料損毀或遺失：只能取消後重新上傳
    Damaged,
    /// 還沒傳完就要求完成
    Incomplete {
        received: i64,
    },
    /// 資料夾中已有同名項目：前端詢問新檔名後再送一次完成
    NameConflict,
    /// 未完成的上傳已達每人上限
    TooMany {
        limit: u32,
    },
}

impl From<crate::space::OutOfReach> for UploadError {
    fn from(e: crate::space::OutOfReach) -> Self {
        UploadError::App(e.into())
    }
}

impl From<AppError> for UploadError {
    fn from(e: AppError) -> Self {
        UploadError::App(e)
    }
}

/// `?` 只做一次轉換，資料庫錯誤需要直接轉換
impl From<sqlx::Error> for UploadError {
    fn from(e: sqlx::Error) -> Self {
        UploadError::App(e.into())
    }
}

#[derive(Serialize)]
struct UploadErrorBody {
    error: String,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    received: Option<i64>,
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        let (code, error, received) = match self {
            UploadError::App(e) => return e.into_response(),
            UploadError::OffsetMismatch { received } => (
                "offset_mismatch",
                "The upload position doesn't match the server".to_string(),
                Some(received),
            ),
            UploadError::Busy => (
                "upload_busy",
                "This file is being uploaded in another tab or on another device".to_string(),
                None,
            ),
            UploadError::Damaged => (
                "upload_damaged",
                "The partly uploaded data is damaged. Cancel the upload and start again"
                    .to_string(),
                None,
            ),
            UploadError::Incomplete { received } => (
                "upload_incomplete",
                "The file hasn't finished uploading".to_string(),
                Some(received),
            ),
            UploadError::NameConflict => (
                "name_conflict",
                "An item with this name already exists".to_string(),
                None,
            ),
            UploadError::TooMany { limit } => (
                "too_many_uploads",
                format!(
                    "You already have {limit} unfinished uploads. Finish or cancel some of them first"
                ),
                None,
            ),
        };
        (
            StatusCode::CONFLICT,
            Json(UploadErrorBody {
                error,
                code,
                received,
            }),
        )
            .into_response()
    }
}

fn upload_not_found() -> AppError {
    AppError::NotFound("Upload not found".to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// 空間中的哪個資料夾；沒給代表最上層
    #[serde(default)]
    path: String,
    name: String,
    size: i64,
    /// 使用者電腦上檔案的修改時間（毫秒），重新整理後用來比對是不是同一個檔案
    file_modified: Option<i64>,
    /// 同名時怎麼處理：決定宣告時要不要提早拒絕。完成時要再帶一次
    #[serde(default)]
    on_conflict: OnConflict,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteRequest {
    /// 改用新的檔名
    name: Option<String>,
    /// 同名時怎麼處理：rename（預設）、fail、replace
    #[serde(default)]
    on_conflict: OnConflict,
}

/// 暫存檔的錯誤 → 回應
fn part_error(e: PartError) -> UploadError {
    match e {
        PartError::Busy => UploadError::Busy,
        PartError::NotFound | PartError::Damaged { .. } => {
            // 正常流程不會發生：記錄下來，讓管理員知道
            eprintln!("upload: {e}");
            UploadError::Damaged
        }
        PartError::TooLarge { .. } => {
            AppError::PayloadTooLarge("More data than the declared file size".to_string()).into()
        }
        PartError::Io(e) if e.kind() == io::ErrorKind::StorageFull => {
            AppError::InsufficientStorage("Not enough storage space on the server".to_string())
                .into()
        }
        // 上傳編號在查到紀錄之後才會用來組路徑，格式一定正確；長度在呼叫 finish 之前已檢查
        PartError::InvalidId | PartError::Incomplete { .. } | PartError::Io(_) => {
            AppError::internal(e).into()
        }
    }
}

fn to_u64(n: i64) -> Result<u64, UploadError> {
    u64::try_from(n).map_err(|e| AppError::internal(e).into())
}

fn to_i64(n: u64) -> Result<i64, UploadError> {
    i64::try_from(n).map_err(|e| AppError::internal(e).into())
}

/// 自己的上傳；不存在或屬於別人時都是 404
async fn find_own(pool: &SqlitePool, user: &CurrentUser, id: &str) -> Result<Upload, UploadError> {
    uploads_db::find_upload(pool, user.id, id)
        .await?
        .ok_or_else(|| upload_not_found().into())
}

/// 上傳紀錄中的空間：再確認一次權限（例如這段期間被降級為一般成員）
async fn space_of(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &CurrentUser,
    upload: &Upload,
) -> Result<Space, UploadError> {
    let key = SpaceKey::parse(Some(&upload.space)).ok_or_else(upload_not_found)?;
    Ok(resolve_space(pool, data_dir, user, key).await?)
}

/// 同名時的選擇是 fail，而資料夾中已經有同名（不分大小寫）的項目
async fn name_taken(dir: PathBuf, name: String, on_conflict: OnConflict) -> Result<bool, AppError> {
    if on_conflict != OnConflict::Fail {
        return Ok(false);
    }
    blocking(move || Ok(tree::find_same_name(&dir, &name, None)?.is_some())).await
}

/// 取得暫存檔的鎖：寫入用共用鎖（`shared`），完成用獨佔鎖。
///
/// 呼叫端必須**在這之後**才讀取資料庫的紀錄：完成時，持有獨佔鎖的期間沒有別人在寫，
/// 讀到的已確認長度一定是最新的；寫入時，拿到共用鎖就代表上傳還沒被完成。
///
/// 暫存檔不在時再查一次紀錄：紀錄也不在是正常的 404（已經完成或取消，例如連按兩下完成），
/// 紀錄還在才是真的遺失。
async fn lock_part(
    pool: &SqlitePool,
    uploads_dir: &Path,
    user: &CurrentUser,
    id: &str,
    shared: bool,
) -> Result<PartLock, UploadError> {
    let lock = if shared {
        PartLock::acquire_shared(uploads_dir, id).await
    } else {
        PartLock::acquire(uploads_dir, id).await
    };
    match lock {
        Ok(lock) => Ok(lock),
        Err(PartError::NotFound) => match uploads_db::find_upload(pool, user.id, id).await? {
            Some(_) => {
                eprintln!("upload: the part file of {id} is missing");
                Err(UploadError::Damaged)
            }
            None => Err(upload_not_found().into()),
        },
        Err(e) => Err(part_error(e)),
    }
}

// ==================== 宣告 ====================

/// POST /api/uploads：宣告一個新的上傳。便宜的檢查先做，最後才建立暫存檔與紀錄
async fn create(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(limit): State<UploadLimit>,
    State(max_pending): State<PendingUploadLimit>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<UploadResponse>), UploadError> {
    // 1. 資料夾、檔名與大小
    let dir = parse_path(&req.path)?;
    space.check_write_in(&dir).map_err(denied)?;
    let name = check_name(&space, &dir, &req.name)?;
    let limit = limit.get();
    let size = u64::try_from(req.size)
        .map_err(|_| AppError::BadRequest("Invalid file size".to_string()))?;
    if size > limit {
        return Err(AppError::PayloadTooLarge(
            "The file is larger than the upload limit".to_string(),
        )
        .into());
    }

    // 2. 資料夾必須存在。依照同名的選擇提早檢查，不必傳完才發現；完成時會再檢查一次
    let root = space.root.clone();
    let abs = space.abs(&dir)?;
    let target = blocking(move || tree::resolve_dir(&root, &abs)).await?;
    if name_taken(target, name.clone(), req.on_conflict).await? {
        return Err(UploadError::NameConflict);
    }

    // 3. 那顆硬碟的空間：其他未完成的上傳還差的部分視為已被預留；以及自己的配額
    ensure_space(&pool, &space.root, size).await?;
    check_quota(&pool, &data_dir, &user, &space, size).await?;

    // 4. 先建立暫存檔，再寫入紀錄：有紀錄就一定有檔案
    let id = storage::generate_storage_key();
    let uploads_dir = space.uploads_dir();
    create_part(&uploads_dir, &id, size)
        .await
        .map_err(part_error)?;

    let space_param = space.key.to_param();
    let dir_text = dir.display();
    let new = NewUpload {
        id: &id,
        owner_id: user.id,
        space: &space_param,
        dir: &dir_text,
        name: &name,
        size: req.size,
        file_modified: req.file_modified,
    };
    match uploads_db::create_upload(&pool, &new, max_pending.get()).await {
        Ok(upload) => Ok((StatusCode::CREATED, Json(upload.into()))),
        Err(e) => {
            // 沒有寫入紀錄：剛建立的暫存檔沒有用處，盡力刪除（失敗的話垃圾回收會處理）
            if let Err(remove_err) = remove_part(&uploads_dir, &id).await {
                eprintln!("failed to remove the unused part file {id}: {remove_err}");
            }
            Err(match e {
                CreateUploadError::TooManyPending { limit } => UploadError::TooMany { limit },
                CreateUploadError::Db(e) => AppError::internal(e).into(),
            })
        }
    }
}

/// 可用空間扣掉所有未完成上傳還差的部分，必須放得下這個檔案：
/// 不會傳到 90% 才發現磁碟滿了，未完成的上傳也不會把其他人的上傳擠掉
async fn ensure_space(pool: &SqlitePool, root: &Path, size: u64) -> Result<(), UploadError> {
    let available = storage::available_bytes(root).map_err(AppError::internal)?;
    let reserved = to_u64(uploads_db::pending_bytes(pool).await?)?;
    if available.saturating_sub(reserved) < size {
        return Err(AppError::InsufficientStorage(
            "Not enough storage space on the server".to_string(),
        )
        .into());
    }
    Ok(())
}

// ==================== 查詢 ====================

/// GET /api/uploads?space=：列出自己在這個空間中未完成的上傳，最早開始的在前
async fn list(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<UploadResponse>>, UploadError> {
    let key = space.key.to_param();
    let uploads = uploads_db::list_uploads(&pool, user.id).await?;
    Ok(Json(
        uploads
            .into_iter()
            .filter(|upload| upload.space == key)
            .map(Into::into)
            .collect(),
    ))
}

/// GET /api/uploads/{id}：查詢已確認的長度
async fn status(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<UploadResponse>, UploadError> {
    Ok(Json(find_own(&pool, &user, &id).await?.into()))
}

// ==================== 續寫 ====================

#[derive(Deserialize)]
struct OffsetQuery {
    offset: i64,
}

/// 接收一段上傳時，超過這段時間沒有收到任何資料，就當成連線中斷。
///
/// 連線被正常關閉時，讀取會立刻得到錯誤；但連線「靜靜地斷掉」時（Wi-Fi 斷線、筆電休眠、
/// 中間的代理沒有把中斷傳過來），讀取會無限期等待，同時一直握著暫存檔的鎖，
/// 之後的續傳就永遠得到 upload_busy。
/// 30 秒遠長於正常情況下兩塊資料的間隔，又短於前端自動重試的總時間（約 5 分鐘）。
const CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// 已確認長度之後，最遠可以從多遠的地方開始寫一段：
/// 同時上傳好幾段時，前面那段慢了，後面的不能無限制地跑在前面
const MAX_AHEAD: u64 = 4 * MAX_CHUNK_BYTES as u64;

/// PUT /api/uploads/{id}?offset=N：寫入從第 N 個位元組開始的一段，回傳新的已確認長度。
///
/// 同一個上傳可以同時有好幾個請求寫不同的段落（不按順序到達也可以）。
/// 每段寫完並 fsync 之後記在 UploadProgress；前面沒有空缺時，已確認長度推進到最遠的地方。
///
/// 請求內容一塊一塊地寫入；連線中途中斷時，已收到的部分照樣 fsync 並記下，
/// 續傳時只需要從那裡繼續，而不是重傳整段。
async fn append(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(progress): State<UploadProgress>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<OffsetQuery>,
    body: Body,
) -> Result<Json<ProgressResponse>, UploadError> {
    // 1. 確認是自己的上傳：別人的上傳在碰到檔案之前就 404
    let upload = find_own(&pool, &user, &id).await?;
    let space = space_of(&pool, &data_dir, &user, &upload).await?;

    // 2. 先拿共用鎖，再讀已確認的長度（理由見 lock_part）。正在完成時是 Busy
    let lock = lock_part(&pool, &space.uploads_dir(), &user, &id, true)
        .await
        .inspect_err(|_| eprintln!("upload {id}: rejected, the upload is being finished"))?;
    let upload = find_own(&pool, &user, &id).await?;

    // 3. 位置：不能在已確認的範圍內（前端以為還沒收到，問一次正確的位置就好），
    //    也不能跑得太前面
    let confirmed = to_u64(upload.received)?;
    let size = to_u64(upload.size)?;
    let offset = u64::try_from(query.offset).unwrap_or(0);
    if query.offset < upload.received || offset > size || offset > confirmed + MAX_AHEAD {
        return Err(UploadError::OffsetMismatch {
            received: upload.received,
        });
    }

    // 4. 同一段已經有請求在寫（例如前端逾時後重送，舊的請求還沒結束）
    let Some(_active) = progress.begin(&id, offset) else {
        eprintln!("upload {id}: rejected, the segment at {offset} is still being written");
        return Err(UploadError::Busy);
    };
    eprintln!("upload {id}: request at offset {offset} started");
    let mut part = lock
        .write_at(confirmed, offset, size)
        .await
        .map_err(part_error)?;

    // 各階段花的時間，寫在結束的紀錄裡：找出上傳慢的原因（網路、硬碟或資料庫）
    let started = std::time::Instant::now();

    // 5. 一塊一塊地寫入；出錯時先記下來，不立刻 return，下一步要先保住已寫入的部分
    let mut stream = body.into_data_stream();
    let mut failure: Option<UploadError> = None;
    loop {
        // 等待下一塊資料有時間限制：見 CHUNK_IDLE_TIMEOUT
        let frame = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(frame)) => frame,
            // 資料傳完了
            Ok(None) => break,
            // 太久沒有資料：與連線中斷相同的處理
            Err(_) => {
                eprintln!(
                    "upload {id}: no data for {} seconds, giving up on this request",
                    CHUNK_IDLE_TIMEOUT.as_secs()
                );
                failure =
                    Some(AppError::BadRequest("The upload was interrupted".to_string()).into());
                break;
            }
        };
        let result = match frame {
            Ok(bytes) => part.write_chunk(&bytes).await.map_err(part_error),
            // 連線中斷是客戶端的問題，不是 500
            Err(_) => Err(AppError::BadRequest("The upload was interrupted".to_string()).into()),
        };
        if let Err(e) = result {
            failure = Some(e);
            break;
        }
    }
    let receive_ms = started.elapsed().as_millis();
    let mut sync_ms = 0;

    // 6. 有新資料：先 fsync，記下這一段，再推進已確認的長度
    if part.len() > offset {
        let step = std::time::Instant::now();
        let end = part.sync().await.map_err(part_error)?;
        sync_ms = step.elapsed().as_millis();
        progress.record(&id, offset, end);
    }
    drop(part);

    let step = std::time::Instant::now();
    let received = advance(&pool, &user, &progress, &id).await?;
    let db_ms = step.elapsed().as_millis();

    eprintln!(
        "upload {id}: request at offset {offset} ended, {received} bytes confirmed{} \
         (receive {receive_ms} ms, sync {sync_ms} ms, database {db_ms} ms)",
        if failure.is_some() {
            " (interrupted)"
        } else {
            ""
        }
    );
    match failure {
        Some(e) => Err(e),
        None => Ok(Json(ProgressResponse { received })),
    }
}

/// 把已確認長度推進到已落盤的段落連續到達的地方，回傳新的已確認長度。
///
/// 好幾個請求可能同時推進：資料庫只在值仍是讀到的那個時才更新（見 advance_received），
/// 慢了一步就重讀再算。
async fn advance(
    pool: &SqlitePool,
    user: &CurrentUser,
    progress: &UploadProgress,
    id: &str,
) -> Result<i64, UploadError> {
    loop {
        // 上傳在這段期間被取消時，這裡是 404
        let current = find_own(pool, user, id).await?.received;
        let reach = to_i64(progress.reach(id, to_u64(current)?))?;
        if reach <= current {
            return Ok(current);
        }
        if uploads_db::advance_received(pool, user.id, id, current, reach).await? {
            progress.confirm(id, to_u64(reach)?);
            return Ok(reach);
        }
    }
}

// ==================== 完成 ====================

#[derive(Serialize)]
struct CompleteResponse {
    /// 實際使用的名稱（同名時可能加上了編號）
    name: String,
    size: i64,
}

/// POST /api/uploads/{id}/complete：完成上傳，放到資料夾中
async fn complete(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(progress): State<UploadProgress>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<CompleteRequest>,
) -> Result<(StatusCode, Json<CompleteResponse>), UploadError> {
    // 1. 確認是自己的上傳，先拿鎖，再讀最新的紀錄：
    //    持有鎖的期間沒有別的請求能續寫、完成或截斷它
    let upload = find_own(&pool, &user, &id).await?;
    let space = space_of(&pool, &data_dir, &user, &upload).await?;
    let lock = lock_part(&pool, &space.uploads_dir(), &user, &id, false).await?;
    let upload = find_own(&pool, &user, &id).await?;

    // 2. 必須已經傳完
    if upload.received != upload.size {
        return Err(UploadError::Incomplete {
            received: upload.received,
        });
    }

    // 3. 資料夾與檔名（資料夾在上傳期間可能被改名或刪除）
    let dir = RelPath::parse(&upload.dir).map_err(|_| upload_not_found())?;
    space.check_write_in(&dir).map_err(denied)?;
    let name = match &req.name {
        Some(name) => check_name(&space, &dir, name)?,
        None => upload.name.clone(),
    };
    let root = space.root.clone();
    let abs = space.abs(&dir)?;
    let target = blocking(move || tree::resolve_dir(&root, &abs))
        .await
        .map_err(|e| match e {
            AppError::NotFound(_) => AppError::NotFound(
                "The folder no longer exists. Cancel the upload and upload the file again"
                    .to_string(),
            ),
            other => other,
        })?;
    if name_taken(target.clone(), name.clone(), req.on_conflict).await? {
        return Err(UploadError::NameConflict);
    }

    // 4. 放到正式的名稱上。放不上去時暫存檔原封不動，換個選擇或名稱就能完成
    let size = to_u64(upload.size)?;
    let part = lock.resume(size, size).await.map_err(part_error)?;
    let placed = match part.finish(&target, &name, req.on_conflict).await {
        Ok(placed) => placed,
        Err(PartError::Io(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(UploadError::NameConflict);
        }
        Err(e) => return Err(part_error(e)),
    };

    // 5. 上傳紀錄已經沒有用處
    progress.forget(&id);
    if let Err(e) = uploads_db::delete_upload(&pool, user.id, &id).await {
        // 留下的紀錄沒有對應的暫存檔，24 小時後由垃圾回收刪除
        eprintln!("failed to delete the finished upload {id}: {e}");
    }
    Ok((
        StatusCode::CREATED,
        Json(CompleteResponse {
            name: placed,
            size: upload.size,
        }),
    ))
}

// ==================== 取消 ====================

/// DELETE /api/uploads/{id}：取消上傳。
///
/// 先刪紀錄、再刪檔案：反過來的話，斷電在兩步之間會留下「有紀錄、沒檔案」。
/// 正在寫入的請求不受影響：它寫完後更新紀錄時會發現紀錄已經不在。
async fn cancel(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(progress): State<UploadProgress>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, UploadError> {
    let upload = find_own(&pool, &user, &id).await?;
    if !uploads_db::delete_upload(&pool, user.id, &id).await? {
        return Err(upload_not_found().into());
    }
    progress.forget(&id);
    // 空間找不到（例如硬碟沒有掛載）也沒關係：沒有紀錄的暫存檔由垃圾回收刪除
    if let Ok(space) = space_of(&pool, &data_dir, &user, &upload).await
        && let Err(e) = remove_part(&space.uploads_dir(), &id).await
    {
        eprintln!("failed to remove the cancelled part file {id}: {e}");
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, count_files, empty_request, request};
    use super::*;
    use axum::body::Bytes;
    use axum::http::{Method, Request, header};
    use serde_json::{Value, json};

    // ==================== 測試輔助 ====================

    fn json_request(t: &TestApp, method: Method, uri: &str, body: Value) -> Request<Body> {
        request(method, uri, Some(&t.cookie))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn put_request(t: &TestApp, id: &str, offset: i64, body: Body) -> Request<Body> {
        request(
            Method::PUT,
            &format!("/api/uploads/{id}?offset={offset}"),
            Some(&t.cookie),
        )
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .unwrap()
    }

    /// 送出請求，回傳狀態碼與 JSON 內容（沒有內容時為 Null）
    async fn call(t: &TestApp, req: Request<Body>) -> (StatusCode, Value) {
        let (status, _, body) = t.send(req).await;
        // axum 內建的錯誤（例如 JSON 解析失敗的 422）是純文字，不是 JSON：存成字串，
        // 錯誤訊息中仍然看得到內容
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
        };
        (status, value)
    }

    async fn declare_with(t: &TestApp, body: Value) -> (StatusCode, Value) {
        call(t, json_request(t, Method::POST, "/api/uploads", body)).await
    }

    /// 宣告一個上傳，回傳上傳編號
    async fn declare(t: &TestApp, name: &str, size: i64) -> String {
        let (status, body) = declare_with(t, json!({ "name": name, "size": size })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    }

    async fn put(t: &TestApp, id: &str, offset: i64, data: &[u8]) -> (StatusCode, Value) {
        call(t, put_request(t, id, offset, Body::from(data.to_vec()))).await
    }

    async fn complete(t: &TestApp, id: &str, body: Value) -> (StatusCode, Value) {
        let uri = format!("/api/uploads/{id}/complete");
        call(t, json_request(t, Method::POST, &uri, body)).await
    }

    async fn status_of(t: &TestApp, id: &str) -> (StatusCode, Value) {
        let uri = format!("/api/uploads/{id}");
        call(t, empty_request(Method::GET, &uri, Some(&t.cookie))).await
    }

    fn uploads_dir(t: &TestApp) -> PathBuf {
        storage::uploads_dir(t.dir.path())
    }

    fn part_count(t: &TestApp) -> usize {
        count_files(&uploads_dir(t))
    }

    /// 完成的檔案內容
    fn file_content(t: &TestApp, name: &str) -> Vec<u8> {
        std::fs::read(t.home("carol").join(name)).unwrap()
    }

    // ==================== 完整流程 ====================

    #[sqlx::test]
    async fn upload_in_chunks_then_complete(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "movie.mp4", 11).await;
        assert_eq!(part_count(&t), 1);

        assert_eq!(put(&t, &id, 0, b"hello ").await.1["received"], 6);
        assert_eq!(put(&t, &id, 6, b"world").await.1["received"], 11);

        let (status, body) = complete(&t, &id, json!({})).await;

        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "movie.mp4");
        assert_eq!(body["size"], 11);
        assert_eq!(file_content(&t, "movie.mp4"), b"hello world");
        // 上傳紀錄與暫存檔都已清除
        assert_eq!(status_of(&t, &id).await.0, StatusCode::NOT_FOUND);
        assert_eq!(part_count(&t), 0);
    }

    #[sqlx::test]
    async fn status_and_list_show_progress(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"1234").await;

        let (status, body) = status_of(&t, &id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["received"], 4);
        assert_eq!(body["size"], 10);

        let (_, list) = call(
            &t,
            empty_request(Method::GET, "/api/uploads", Some(&t.cookie)),
        )
        .await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], id.as_str());
    }

    // ==================== 續寫 ====================

    #[sqlx::test]
    async fn wrong_offset_tells_the_client_where_to_resume(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"1234").await;

        // 前端以為伺服器還在 0（例如上一個回應沒收到）
        let (status, body) = put(&t, &id, 0, b"1234").await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "offset_mismatch");
        assert_eq!(body["received"], 4);
    }

    #[sqlx::test]
    async fn interrupted_chunk_keeps_what_arrived(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 100).await;

        // 送出 5 個位元組之後連線中斷
        let chunks: Vec<Result<Bytes, io::Error>> = vec![
            Ok(Bytes::from_static(b"hello")),
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "gone")),
        ];
        let body = Body::from_stream(futures_util::stream::iter(chunks));
        let (status, _) = call(&t, put_request(&t, &id, 0, body)).await;

        // 中斷是客戶端的問題，不是 500；已收到的 5 個位元組被保住了
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(status_of(&t, &id).await.1["received"], 5);
    }

    #[sqlx::test]
    async fn cannot_send_more_than_declared(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 5).await;

        let (status, _) = put(&t, &id, 0, b"hello!").await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(status_of(&t, &id).await.1["received"], 0);
    }

    // ==================== 宣告時的檢查 ====================

    #[sqlx::test]
    async fn declaration_is_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        std::fs::write(t.home("carol").join("taken.txt"), b"x").unwrap();

        let cases = [
            (json!({ "name": "a/b", "size": 1 }), StatusCode::BAD_REQUEST),
            (
                json!({ "name": "a.bin", "size": -1 }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "name": "a.bin", "size": 2 * MIB }),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            // 資料夾其實是一個檔案、或不存在：宣告時就擋下，不必傳完才發現
            (
                json!({ "name": "a.bin", "size": 1, "path": "taken.txt" }),
                StatusCode::NOT_FOUND,
            ),
            (
                json!({ "name": "a.bin", "size": 1, "path": "../jerry" }),
                StatusCode::BAD_REQUEST,
            ),
            // 明確要求同名時拒絕（不分大小寫）。預設是自動改名，不會拒絕
            (
                json!({ "name": "TAKEN.TXT", "size": 1, "on_conflict": "fail" }),
                StatusCode::CONFLICT,
            ),
            // 打錯欄位名稱
            (
                json!({ "name": "a.bin", "size": 1, "sise": 1 }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];

        for (body, expected) in cases {
            let (status, response) = declare_with(&t, body.clone()).await;
            assert_eq!(status, expected, "{body} → {response}");
        }
        // 被拒絕的宣告都沒有留下暫存檔
        assert_eq!(part_count(&t), 0);
    }

    #[sqlx::test]
    async fn pending_limit_is_enforced_without_leaking_part_files(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        for n in 0..5 {
            declare(&t, &format!("{n}.bin"), 1).await;
        }

        let (status, body) = declare_with(&t, json!({ "name": "6.bin", "size": 1 })).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "too_many_uploads");
        // 被拒絕的那一個沒有留下暫存檔
        assert_eq!(part_count(&t), 5);
    }

    #[sqlx::test]
    async fn not_enough_space_is_507(pool: SqlitePool) {
        // 上傳上限設得極大，讓空間檢查成為唯一的阻擋
        let t = TestApp::new(pool, 1 << 62).await;

        let (status, _) = declare_with(&t, json!({ "name": "huge.bin", "size": 1u64 << 61 })).await;

        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(part_count(&t), 0);
    }

    // ==================== 完成時的檢查 ====================

    #[sqlx::test]
    async fn cannot_complete_before_all_bytes_arrive(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"1234").await;

        let (status, body) = complete(&t, &id, json!({})).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "upload_incomplete");
        assert_eq!(body["received"], 4);
    }

    #[sqlx::test]
    async fn name_taken_meanwhile_is_renamed_on_completion(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.txt", 5).await;
        put(&t, &id, 0, b"hello").await;
        // 傳輸期間有人建立了同名檔案
        std::fs::write(t.home("carol").join("a.txt"), b"other").unwrap();

        // 預設自動改名：傳好的內容不會因為同名而白費
        let (status, body) = complete(&t, &id, json!({})).await;

        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "a (1).txt");
        assert_eq!(file_content(&t, "a (1).txt"), b"hello");
        assert_eq!(file_content(&t, "a.txt"), b"other");
    }

    // ==================== 取消與權限 ====================

    #[sqlx::test]
    async fn cancel_removes_the_record_and_part_file(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"1234").await;

        let uri = format!("/api/uploads/{id}");
        let (status, _) = call(&t, empty_request(Method::DELETE, &uri, Some(&t.cookie))).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(part_count(&t), 0);
        assert_eq!(status_of(&t, &id).await.0, StatusCode::NOT_FOUND);
        // 再取消一次：已經不存在
        let (status, _) = call(&t, empty_request(Method::DELETE, &uri, Some(&t.cookie))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn other_users_upload_is_not_found(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = t.other_user("dave").await;
        let id = storage::generate_storage_key();
        create_part(&uploads_dir(&t), &id, 10).await.unwrap();
        let new = NewUpload {
            id: &id,
            owner_id: dave,
            space: "me",
            dir: "",
            name: "dave.bin",
            size: 10,
            file_modified: None,
        };
        uploads_db::create_upload(&t.pool, &new, 5).await.unwrap();

        // carol 對 dave 的上傳：查詢、續寫、完成、取消都是 404
        assert_eq!(status_of(&t, &id).await.0, StatusCode::NOT_FOUND);
        assert_eq!(put(&t, &id, 0, b"x").await.0, StatusCode::NOT_FOUND);
        assert_eq!(complete(&t, &id, json!({})).await.0, StatusCode::NOT_FOUND);
        let uri = format!("/api/uploads/{id}");
        let (status, _) = call(&t, empty_request(Method::DELETE, &uri, Some(&t.cookie))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // dave 的暫存檔沒有被動到
        let part = storage::resumable::part_path(&uploads_dir(&t), &id).unwrap();
        assert!(part.exists());
    }

    // ==================== 競爭 ====================

    #[sqlx::test]
    async fn segments_can_arrive_out_of_order(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 15).await;

        // 後面的段落先到：已確認長度還不能推進
        assert_eq!(put(&t, &id, 10, b"!!!!!").await.1["received"], 0);
        assert_eq!(put(&t, &id, 5, b"world").await.1["received"], 0);
        // 空缺補上：一次推進到最後
        assert_eq!(put(&t, &id, 0, b"hello").await.1["received"], 15);

        let (status, body) = complete(&t, &id, json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(file_content(&t, "a.bin"), b"helloworld!!!!!");
    }

    #[sqlx::test]
    async fn a_segment_too_far_ahead_is_refused(pool: SqlitePool) {
        let t = TestApp::new(pool, 1 << 40).await;
        let size = 2 * MAX_AHEAD as i64;
        let id = declare(&t, "big.bin", size).await;

        let (status, body) = put(&t, &id, MAX_AHEAD as i64 + 1, b"x").await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "offset_mismatch");
        assert_eq!(body["received"], 0);
    }

    #[sqlx::test]
    async fn writers_never_overlap_and_block_completion(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"hello").await;

        // 請求 C：正在寫 5 開始的那一段，還沒結束
        let progress = t.state.upload_progress.clone();
        let c = progress.begin(&id, 5).unwrap();
        let c_lock = PartLock::acquire_shared(&uploads_dir(&t), &id)
            .await
            .unwrap();

        // 前端以為 C 逾時了，重送同一段：拒絕，不會兩個請求同時寫同一個位置
        let (status, body) = put(&t, &id, 5, b"world").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "upload_busy");
        // 還有人在寫：不能完成
        let (status, body) = complete(&t, &id, json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "upload_busy");

        // C 結束之後，重送就能成功
        drop((c, c_lock));
        assert_eq!(put(&t, &id, 5, b"world").await.1["received"], 10);
        assert_eq!(complete(&t, &id, json!({})).await.0, StatusCode::CREATED);
        assert_eq!(file_content(&t, "a.bin"), b"helloworld");
    }

    #[sqlx::test]
    async fn segments_lost_by_a_restart_are_sent_again(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 5, b"world").await;

        // 程式重新啟動：記憶體中「5 之後已經收到」的紀錄不見了
        t.state.upload_progress.forget(&id);
        assert_eq!(put(&t, &id, 0, b"hello").await.1["received"], 5);

        // 前端從已確認的 5 重送
        assert_eq!(put(&t, &id, 5, b"world").await.1["received"], 10);
        assert_eq!(complete(&t, &id, json!({})).await.0, StatusCode::CREATED);
        assert_eq!(file_content(&t, "a.bin"), b"helloworld");
    }

    #[sqlx::test]
    async fn completing_twice_is_not_found_rather_than_damaged(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 5).await;
        put(&t, &id, 0, b"hello").await;

        assert_eq!(complete(&t, &id, json!({})).await.0, StatusCode::CREATED);

        // 連按兩下完成：第二次是正常的 404，不是「暫存資料已損毀」
        let (status, body) = complete(&t, &id, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    // ==================== 同名的選擇 ====================

    #[sqlx::test]
    async fn complete_can_keep_both_or_replace(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        std::fs::write(t.home("carol").join("a.txt"), b"old").unwrap();

        // 保留兩者：宣告時不拒絕，完成時自動改名
        let (_, body) = declare_with(
            &t,
            json!({ "name": "a.txt", "size": 3, "on_conflict": "rename" }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        put(&t, &id, 0, b"two").await;
        let (status, body) = complete(&t, &id, json!({ "on_conflict": "rename" })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "a (1).txt");

        // 取代：同一個檔案，內容換成新的
        let (_, body) = declare_with(
            &t,
            json!({ "name": "a.txt", "size": 3, "on_conflict": "replace" }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        put(&t, &id, 0, b"new").await;
        let (status, body) = complete(&t, &id, json!({ "on_conflict": "replace" })).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "a.txt");
        assert_eq!(file_content(&t, "a.txt"), b"new");
    }
}
