//! 分段上傳（可續傳）的 API。
//!
//! 1. POST   /api/uploads               宣告：資料夾、檔名、總大小 → 取得上傳編號
//! 2. PUT    /api/uploads/{id}?offset=N 從第 N 個位元組續寫一段，回傳新的已確認長度
//! 3. POST   /api/uploads/{id}/complete 完成：搬進 blobs/，建立檔案紀錄
//!    GET    /api/uploads/{id}          查詢已確認的長度（斷線後從哪裡繼續）
//!    GET    /api/uploads               列出自己未完成的上傳（重新整理後續傳）
//!    DELETE /api/uploads/{id}          取消
//!
//! 檔案與資料庫的先後順序，處處都是「檔案先、紀錄後」：
//! - 宣告：先建立暫存檔，再寫入紀錄。有紀錄就一定有檔案
//! - 續寫：先寫入並 fsync，再推進已確認的長度。檔案永遠不少於紀錄
//! - 完成：先搬進 blobs/，再建立檔案紀錄。失敗只會留下沒人參照的 blob
//! - 取消：例外，先刪紀錄、再刪檔案。反過來的話，斷電在兩步之間會留下「有紀錄、沒檔案」
//!
//! 續寫與完成都是「先拿暫存檔的鎖，再讀資料庫的已確認長度」，理由見 lock_part。

use std::io;
use std::path::Path;
use std::sync::Arc;

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
    extract::CurrentUser,
    files::{FileResponse, folder_not_found},
    register::{self, NewFile, OnConflict, RegisterError},
    state::{AppState, PendingUploadLimit, UploadLimit},
};

use crate::{
    db::{
        files as files_db,
        uploads::{self as uploads_db, CreateUploadError, NewUpload, Upload},
    },
    files::name::validate_file_name,
    storage::{
        self,
        resumable::{PartError, PartLock, create_part, remove_part},
    },
};

/// 每段的大小上限。前端每段送 8 MiB，這裡留一倍的餘裕
const MAX_CHUNK_BYTES: usize = 16 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/uploads", post(create).get(list))
        .route(
            "/api/uploads/{id}",
            // 順序很重要：layer 只套用到呼叫當下已存在的方法（見 files::router 的說明）。
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
    parent_id: Option<i64>,
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
            parent_id: u.parent_id,
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
                "上傳位置與伺服器不一致".to_string(),
                Some(received),
            ),
            UploadError::Busy => (
                "upload_busy",
                "這個檔案正在其他分頁或裝置上傳".to_string(),
                None,
            ),
            UploadError::Damaged => (
                "upload_damaged",
                "上傳的暫存資料已損毀，請取消後重新上傳".to_string(),
                None,
            ),
            UploadError::Incomplete { received } => (
                "upload_incomplete",
                "檔案還沒有傳完".to_string(),
                Some(received),
            ),
            UploadError::NameConflict => ("name_conflict", "同名項目已存在".to_string(), None),
            UploadError::TooMany { limit } => (
                "too_many_uploads",
                format!("未完成的上傳已達 {limit} 個，請先完成或取消其中一些"),
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
    AppError::NotFound("找不到這個上傳".to_string())
}

/// 同名處理的錯誤 → 回應。同名有分段上傳自己的代碼，前端據此詢問使用者
fn register_error(e: RegisterError) -> UploadError {
    match e {
        RegisterError::NameConflict => UploadError::NameConflict,
        other => other.into_app_error().into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// 沒給代表根目錄
    parent_id: Option<i64>,
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
    /// 同名時怎麼處理：fail（預設）、rename、replace
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
            AppError::PayloadTooLarge("超過宣告的檔案大小".to_string()).into()
        }
        PartError::Io(e) if e.kind() == io::ErrorKind::StorageFull => {
            AppError::InsufficientStorage("伺服器儲存空間不足".to_string()).into()
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

/// 取得暫存檔的獨佔鎖。
///
/// 呼叫端必須**在這之後**才讀取資料庫的已確認長度：持有鎖的期間沒有別人能推進它，
/// 讀到的一定是最新的。反過來的話，讀到的值可能在拿到鎖之前就過時了，
/// 截斷會把另一個請求剛確認的資料切掉。
///
/// 暫存檔不在時再查一次紀錄：紀錄也不在是正常的 404（已經完成或取消，例如連按兩下完成），
/// 紀錄還在才是真的遺失。
async fn lock_part(
    pool: &SqlitePool,
    data_dir: &Path,
    user: &CurrentUser,
    id: &str,
) -> Result<PartLock, UploadError> {
    match PartLock::acquire(data_dir, id).await {
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
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(UploadLimit(limit)): State<UploadLimit>,
    State(PendingUploadLimit(max_pending)): State<PendingUploadLimit>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<UploadResponse>), UploadError> {
    // 1. 檔名與大小
    let name = validate_file_name(&req.name).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let size =
        u64::try_from(req.size).map_err(|_| AppError::BadRequest("檔案大小不正確".to_string()))?;
    if size > limit {
        return Err(AppError::PayloadTooLarge("檔案超過上傳大小上限".to_string()).into());
    }

    // 2. 父項目必須是自己的資料夾。uploads 表只有外鍵，擋不住「父項目是一個檔案」；
    //    不在這裡檢查的話，要等傳完整個檔案、寫入 files 時才會被擋下
    if let Some(parent_id) = req.parent_id
        && !files_db::is_owned_folder(&pool, user.id, parent_id).await?
    {
        return Err(folder_not_found().into());
    }

    // 3. 依照同名的選擇提早檢查，不必傳完才發現。完成時會再檢查一次
    register::precheck(&pool, user.id, req.parent_id, &name, req.on_conflict)
        .await
        .map_err(register_error)?;

    // 4. 空間：其他未完成的上傳還差的部分視為已被預留
    ensure_space(&pool, &data_dir, size).await?;

    // 5. 先建立暫存檔，再寫入紀錄：有紀錄就一定有檔案
    let id = storage::generate_storage_key();
    create_part(&data_dir, &id).await.map_err(part_error)?;

    let new = NewUpload {
        id: &id,
        owner_id: user.id,
        parent_id: req.parent_id,
        name: &name,
        size: req.size,
        file_modified: req.file_modified,
    };
    match uploads_db::create_upload(&pool, &new, max_pending).await {
        Ok(upload) => Ok((StatusCode::CREATED, Json(upload.into()))),
        Err(e) => {
            // 沒有寫入紀錄：剛建立的暫存檔沒有用處，盡力刪除（失敗的話垃圾回收會處理）
            if let Err(remove_err) = remove_part(&data_dir, &id).await {
                eprintln!("failed to remove the unused part file {id}: {remove_err}");
            }
            Err(match e {
                CreateUploadError::TooManyPending { limit } => UploadError::TooMany { limit },
                CreateUploadError::ParentNotFound => folder_not_found().into(),
                CreateUploadError::Db(e) => AppError::internal(e).into(),
            })
        }
    }
}

/// 可用空間扣掉所有未完成上傳還差的部分，必須放得下這個檔案：
/// 不會傳到 90% 才發現磁碟滿了，未完成的上傳也不會把其他人的上傳擠掉
async fn ensure_space(pool: &SqlitePool, data_dir: &Path, size: u64) -> Result<(), UploadError> {
    let available = storage::available_bytes(data_dir).map_err(AppError::internal)?;
    let reserved = to_u64(uploads_db::pending_bytes(pool).await?)?;
    if available.saturating_sub(reserved) < size {
        return Err(AppError::InsufficientStorage("伺服器儲存空間不足".to_string()).into());
    }
    Ok(())
}

// ==================== 查詢 ====================

/// GET /api/uploads：列出自己未完成的上傳，最早開始的在前
async fn list(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<UploadResponse>>, UploadError> {
    let uploads = uploads_db::list_uploads(&pool, user.id).await?;
    Ok(Json(uploads.into_iter().map(Into::into).collect()))
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

/// PUT /api/uploads/{id}?offset=N：從第 N 個位元組續寫。
///
/// 請求內容一塊一塊地寫入；連線中途中斷時，已收到的部分照樣 fsync 並確認，
/// 續傳時只需要從那裡繼續，而不是重傳整段。
async fn append(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<OffsetQuery>,
    body: Body,
) -> Result<Json<ProgressResponse>, UploadError> {
    // 1. 確認是自己的上傳：別人的上傳在碰到檔案之前就 404
    find_own(&pool, &user, &id).await?;

    // 2. 先拿鎖，再讀已確認的長度：這時讀到的一定是最新的（見 lock_part）
    let lock = lock_part(&pool, &data_dir, &user, &id).await?;
    let upload = find_own(&pool, &user, &id).await?;

    // 3. 位置必須與伺服器已確認的長度相同
    if query.offset != upload.received {
        return Err(UploadError::OffsetMismatch {
            received: upload.received,
        });
    }

    // 4. 截斷到已確認的長度（丟掉斷電前沒被確認的部分）
    let confirmed = to_u64(upload.received)?;
    let mut part = lock
        .resume(confirmed, to_u64(upload.size)?)
        .await
        .map_err(part_error)?;

    // 5. 一塊一塊地寫入；出錯時先記下來，不立刻 return，下一步要先保住已寫入的部分
    let mut stream = body.into_data_stream();
    let mut failure: Option<UploadError> = None;
    while let Some(frame) = stream.next().await {
        let result = match frame {
            Ok(bytes) => part.write_chunk(&bytes).await.map_err(part_error),
            // 連線中斷是客戶端的問題，不是 500
            Err(_) => Err(AppError::BadRequest("上傳中斷".to_string()).into()),
        };
        if let Err(e) = result {
            failure = Some(e);
            break;
        }
    }

    // 6. 有新資料：先 fsync，再推進已確認的長度
    let mut received = upload.received;
    if part.len() > confirmed {
        let new_len = to_i64(part.sync().await.map_err(part_error)?)?;
        let advanced =
            uploads_db::advance_received(&pool, user.id, &id, upload.received, new_len).await?;
        if !advanced {
            // 檔案鎖保證同一時間只有一個請求在寫；更新失敗代表上傳在這段期間被取消了
            return Err(upload_not_found().into());
        }
        received = new_len;
    }

    match failure {
        Some(e) => Err(e),
        None => Ok(Json(ProgressResponse { received })),
    }
}

// ==================== 完成 ====================

/// POST /api/uploads/{id}/complete：完成上傳，建立檔案
async fn complete(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<String>,
    Json(req): Json<CompleteRequest>,
) -> Result<(StatusCode, Json<FileResponse>), UploadError> {
    // 1. 確認是自己的上傳，先拿鎖，再讀最新的紀錄：
    //    持有鎖的期間沒有別的請求能續寫、完成或截斷它
    find_own(&pool, &user, &id).await?;
    let lock = lock_part(&pool, &data_dir, &user, &id).await?;
    let upload = find_own(&pool, &user, &id).await?;

    // 2. 必須已經傳完
    if upload.received != upload.size {
        return Err(UploadError::Incomplete {
            received: upload.received,
        });
    }

    // 3. 檔名，以及搬進 blobs/ 之前依照選擇再檢查一次同名：
    //    不通過時暫存檔原封不動，使用者已經傳完的資料不會白費，換個選擇或名稱就能完成
    let name = match req.name {
        Some(name) => validate_file_name(&name).map_err(|e| AppError::BadRequest(e.to_string()))?,
        None => upload.name.clone(),
    };
    register::precheck(&pool, user.id, upload.parent_id, &name, req.on_conflict)
        .await
        .map_err(register_error)?;

    // 4. 搬進 blobs/（先檔案）
    let size = to_u64(upload.size)?;
    let part = lock.resume(size, size).await.map_err(part_error)?;
    let key = storage::generate_storage_key();
    part.finish(&key).await.map_err(part_error)?;

    // 5. 依照同名的選擇建立檔案紀錄（後紀錄）
    let file = NewFile {
        owner_id: user.id,
        parent_id: upload.parent_id,
        name: &name,
        size: upload.size,
        storage_key: &key,
    };
    let created = register::register_file(&pool, &data_dir, &file, req.on_conflict).await;

    // 6. 不論成功或失敗，上傳紀錄都已經沒有用處：暫存檔已經搬走，無法再續傳
    if let Err(e) = uploads_db::delete_upload(&pool, user.id, &id).await {
        // 留下的紀錄沒有對應的暫存檔，24 小時後由垃圾回收刪除
        eprintln!("failed to delete the finished upload {id}: {e}");
    }

    match created {
        Ok(entry) => Ok((StatusCode::CREATED, Json(entry.into()))),
        Err(e) => {
            // 第 3 步之後的極短時間內有人建立了同名項目，或資料夾被刪除了。
            // 這是檔案鎖擋不住的情況（衝突來自 files 表），blob 沒有紀錄參照，盡力刪除
            if let Err(remove_err) = storage::remove_blob(&data_dir, &key).await {
                eprintln!("failed to remove the unregistered blob {key}: {remove_err}");
            }
            Err(match e {
                RegisterError::NameConflict => {
                    AppError::Conflict("同名項目已存在，請重新上傳".to_string()).into()
                }
                other => other.into_app_error().into(),
            })
        }
    }
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
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, UploadError> {
    if !uploads_db::delete_upload(&pool, user.id, &id).await? {
        return Err(upload_not_found().into());
    }
    if let Err(e) = remove_part(&data_dir, &id).await {
        // 沒有紀錄的暫存檔由垃圾回收刪除
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

    fn part_count(t: &TestApp) -> usize {
        count_files(&t.dir.path().join("uploads"))
    }

    /// 已建立的檔案內容
    async fn file_content(t: &TestApp, name: &str) -> Vec<u8> {
        let key: String = sqlx::query_scalar("SELECT storage_key FROM files WHERE name = ?")
            .bind(name)
            .fetch_one(&t.pool)
            .await
            .unwrap();
        std::fs::read(storage::blob_path(t.dir.path(), &key).unwrap()).unwrap()
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
        assert!(body.get("storage_key").is_none());
        assert_eq!(file_content(&t, "movie.mp4").await, b"hello world");
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
        let (file_id, _) = t.store_file(t.user_id, None, "taken.txt", b"x").await;

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
            // 父項目是一個檔案：宣告時就擋下，不必傳完才發現
            (
                json!({ "name": "a.bin", "size": 1, "parent_id": file_id }),
                StatusCode::NOT_FOUND,
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
    async fn foreign_and_missing_folders_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let dave = t.other_user("dave").await;
        let daves = t.make_folder(dave, None, "dave's").await;

        let foreign = declare_with(&t, json!({ "name": "a", "size": 1, "parent_id": daves })).await;
        let missing =
            declare_with(&t, json!({ "name": "a", "size": 1, "parent_id": 999999 })).await;

        assert_eq!(foreign.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign, missing);
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
        t.store_file(t.user_id, None, "a.txt", b"other").await;

        // 預設自動改名：傳好的內容不會因為同名而白費
        let (status, body) = complete(&t, &id, json!({})).await;

        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "a (1).txt");
        assert_eq!(file_content(&t, "a (1).txt").await, b"hello");
        assert_eq!(file_content(&t, "a.txt").await, b"other");
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
        create_part(t.dir.path(), &id).await.unwrap();
        let new = NewUpload {
            id: &id,
            owner_id: dave,
            parent_id: None,
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
        assert_eq!(part_count(&t), 1);
    }

    // ==================== 競爭 ====================

    #[sqlx::test]
    async fn a_stale_request_never_truncates_confirmed_data(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let id = declare(&t, "a.bin", 10).await;
        put(&t, &id, 0, b"hello").await;

        // 請求 C：持有鎖，寫入並確認了後半段，還沒釋放鎖
        let mut c = PartLock::acquire(t.dir.path(), &id)
            .await
            .unwrap()
            .resume(5, 10)
            .await
            .unwrap();
        c.write_chunk(b"world").await.unwrap();
        c.sync().await.unwrap();
        uploads_db::advance_received(&t.pool, t.user_id, &id, 5, 10)
            .await
            .unwrap();

        // 請求 B：前端以為 C 逾時了，用舊的 offset 重送
        let (status, body) = put(&t, &id, 5, b"XXXXX").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "upload_busy");

        // C 結束、釋放鎖之後，B 再重試：拿到鎖之後才讀紀錄，看到的是最新的 10
        drop(c);
        let (status, body) = put(&t, &id, 5, b"XXXXX").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "offset_mismatch");
        assert_eq!(body["received"], 10);

        // C 確認的資料完好無缺
        let (status, _) = complete(&t, &id, json!({})).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(file_content(&t, "a.bin").await, b"helloworld");
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
        let (existing, _) = t.store_file(t.user_id, None, "a.txt", b"old").await;

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
        assert_eq!(body["id"], existing);
        assert_eq!(file_content(&t, "a.txt").await, b"new");
    }
}
