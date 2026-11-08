//! 檔案相關的 API。

use std::io;
use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Query, State, multipart::MultipartError},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::CurrentUser,
    register::{self, NewFile, OnConflict},
    state::{AppState, UploadLimit},
};
use crate::{
    db::files::{self as files_db, CreateFileError, FileEntry},
    files::name::validate_file_name,
    storage::{self, StorageError, TempBlob},
};

/// multipart 格式本身的額外開銷（邊界字串、欄位標頭）的預留空間。
/// 請求本體上限 = 檔案上限 + 這個值，否則剛好等於上限的合法檔案會被誤擋。
const MULTIPART_OVERHEAD: u64 = 1024 * 1024;

/// 父資料夾「不存在」與「屬於別人」對外一律使用這個訊息，避免他人資料夾被探測
const FOLDER_NOT_FOUND: &str = "找不到資料夾";

/// 檔案相關路由。
pub fn router(upload_limit: UploadLimit) -> Router<AppState> {
    Router::new()
        .route(
            "/api/files",
            // 順序很重要：MethodRouter::layer 只套用到呼叫當下已存在的方法。
            // 先 post(...).layer(...) 再 .get(...)，放寬的上限只作用在上傳；
            // 反過來寫成 get(...).layer(...).post(...) 會讓上傳退回 2 MB 預設值。
            // 登入等其他 API 一律維持預設上限，沒有理由接受 10 GiB 的 JSON。
            post(upload)
                .layer(DefaultBodyLimit::max(body_limit(upload_limit)))
                .get(list_files),
        )
        .route("/api/folders", post(create_folder))
        .route("/api/config", get(config))
}

/// 計算請求本體上限。
///
/// 不用 `disable()`：那樣請求大小完全依賴 handler 自己控制，
/// 將來若有人呼叫 `field.text()` 或 `field.bytes()` 把欄位整個讀進記憶體，就沒有任何上限。
///
/// `DefaultBodyLimit::max` 接受 usize；32 位元平台上 usize 最大約 4 GiB，
/// 放不下 10 GiB 的設定。轉換用 try_from，失敗代表設定在此平台無效，
/// 依照設定模組的原則在啟動時停止（路由在開始監聽之前建立）。
fn body_limit(UploadLimit(limit): UploadLimit) -> usize {
    let total = limit.saturating_add(MULTIPART_OVERHEAD);
    usize::try_from(total).unwrap_or_else(|_| {
        panic!(
            "MANORCLOUD_MAX_UPLOAD_BYTES（{limit}）超出此平台可處理的範圍，\
             上限加上 multipart 開銷不能超過 {} 位元組",
            usize::MAX
        )
    })
}

// ==================== 共用型別 ====================

/// `?parent_id=...`：沒給代表根目錄。上傳與列表共用。
#[derive(Deserialize)]
struct ParentParams {
    parent_id: Option<i64>,
}

/// 上傳的網址參數
#[derive(Deserialize)]
struct UploadParams {
    /// 上傳到哪個資料夾；沒給代表根目錄
    parent_id: Option<i64>,
    /// 檔名。優先於 multipart 中的檔名，見 `upload` 的說明
    name: Option<String>,
    /// 同名時怎麼處理：fail（預設）、rename、replace
    #[serde(default)]
    on_conflict: OnConflict,
}

/// 前端需要知道的伺服器設定
#[derive(Serialize)]
struct ConfigResponse {
    /// 單一檔案的上傳上限；前端用它在送出前先檢查，不必傳到一半才收到 413
    max_upload_bytes: u64,
}

/// GET /api/config：前端需要知道的伺服器設定。
/// 需要登入：沒有理由對未登入的人公開伺服器的設定。
async fn config(
    _user: CurrentUser,
    State(UploadLimit(limit)): State<UploadLimit>,
) -> Json<ConfigResponse> {
    Json(ConfigResponse {
        max_upload_bytes: limit,
    })
}

/// 檔案或資料夾資訊的對外格式。刻意不含 `storage_key`。
#[derive(Serialize)]
pub(super) struct FileResponse {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    kind: String,
    size: Option<i64>,
    mime: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl From<FileEntry> for FileResponse {
    fn from(entry: FileEntry) -> Self {
        FileResponse {
            id: entry.id,
            parent_id: entry.parent_id,
            name: entry.name,
            kind: entry.kind,
            size: entry.size,
            mime: entry.mime,
            created_at: entry.created_at,
            updated_at: entry.updated_at,
        }
    }
}

/// 若有指定 parent_id，確認它是使用者自己的資料夾，否則回 404。
/// 「不存在」「屬於別人」「是檔案」都得到同一個錯誤。
async fn ensure_owned_folder(
    pool: &SqlitePool,
    user: &CurrentUser,
    parent_id: Option<i64>,
) -> Result<(), AppError> {
    if let Some(id) = parent_id
        && !files_db::is_owned_folder(pool, user.id, id).await?
    {
        return Err(folder_not_found());
    }
    Ok(())
}

// ==================== 列表 ====================

/// GET /api/files?parent_id=...：列出資料夾（未指定則為根目錄）的直接子項目。
///
/// 指定的資料夾不是自己的就回 404，而不是空陣列：
/// 否則「空資料夾」與「不存在的資料夾」看起來一樣，使用者打錯 id 時會誤以為資料夾是空的，
/// 前端也無法判斷一個收藏起來的連結是否已失效。這也與上傳時的行為一致。
async fn list_files(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    Query(params): Query<ParentParams>,
) -> Result<Json<Vec<FileResponse>>, AppError> {
    ensure_owned_folder(&pool, &user, params.parent_id).await?;

    let entries = files_db::list_children(&pool, user.id, params.parent_id).await?;
    Ok(Json(entries.into_iter().map(FileResponse::from).collect()))
}

// ==================== 建立資料夾 ====================

#[derive(Deserialize)]
struct CreateFolderRequest {
    name: String,
    /// 欄位省略或為 null 都代表根目錄
    parent_id: Option<i64>,
}

/// POST /api/folders：建立資料夾。
///
/// 不做提早檢查：請求只是一小段 JSON，直接交給資料庫約束判斷即可，
/// 少一次查詢，也沒有「檢查與寫入之間」的時間差。
async fn create_folder(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    Json(req): Json<CreateFolderRequest>,
) -> Result<(StatusCode, Json<FileResponse>), AppError> {
    let name = validate_file_name(&req.name).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let entry = files_db::create_folder(&pool, user.id, req.parent_id, &name)
        .await
        .map_err(create_error)?;

    Ok((StatusCode::CREATED, Json(entry.into())))
}

// ==================== 上傳 ====================

/// POST /api/files?parent_id=...&name=...&on_conflict=...：上傳一個檔案。
///
/// extractor 順序：`CurrentUser`、`State`、`Query` 只讀 header 與網址，
/// `Multipart` 讀取本體，必須放最後。未登入的請求在讀取本體之前就被 401 擋下。
async fn upload(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    State(UploadLimit(limit)): State<UploadLimit>,
    Query(params): Query<UploadParams>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FileResponse>), AppError> {
    // 1. 提早檢查父資料夾：不讓使用者傳完大檔案才知道失敗。
    //    這只是為了使用體驗，正確性由 create_file 的資料庫約束保證。
    ensure_owned_folder(&pool, &user, params.parent_id).await?;

    // 2. 取第一個欄位，並在接收內容之前驗證欄位名稱與檔名
    let mut field = multipart
        .next_field()
        .await
        .map_err(multipart_error)?
        .ok_or_else(|| AppError::BadRequest("缺少檔案欄位".to_string()))?;

    if field.name() != Some("file") {
        return Err(AppError::BadRequest(
            "第一個欄位必須是名為 file 的檔案".to_string(),
        ));
    }

    // 3. 檔名：優先使用網址參數 name，沒有時才用 multipart 的檔名（curl 等客戶端）。
    //    瀏覽器編碼 multipart 檔名時會把 " 變成 %22，卻不編碼 % 本身，
    //    伺服器無法分辨原名是 `100"` 還是 `100%22`。網址參數的編碼與解碼規則都由我們控制，
    //    前端用 URLSearchParams 編碼後，檔名能一字不差地傳到這裡。
    let raw_name = match params.name {
        Some(name) => name,
        None => field
            .file_name()
            .ok_or_else(|| AppError::BadRequest("缺少檔名".to_string()))?
            .to_string(),
    };
    let name = validate_file_name(&raw_name).map_err(|e| AppError::BadRequest(e.to_string()))?;

    // 4. 依照同名的選擇提早檢查：預設的 fail 在接收內容之前就回 409，不必傳完才發現
    register::precheck(&pool, user.id, params.parent_id, &name, params.on_conflict)
        .await
        .map_err(register::RegisterError::into_app_error)?;

    // 5. 串流寫入暫存檔：收到一塊就寫一塊，記憶體用量與檔案大小無關。
    //    任何一步失敗（含使用者斷線或取消），blob 被丟棄時 Drop 會刪除暫存檔。
    let mut blob = TempBlob::create(&data_dir, limit)
        .await
        .map_err(storage_error)?;
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        blob.write_chunk(&chunk).await.map_err(storage_error)?;
    }

    // 6. 在 persist 之前轉換大小：失敗時只需丟棄 TempBlob，不會在 blobs/ 留下孤兒
    let size = i64::try_from(blob.size()).map_err(AppError::internal)?;

    // 7. 搬到正式位置（先寫 blob、再寫資料庫）
    let key = storage::generate_storage_key();
    blob.persist(&key).await.map_err(storage_error)?;

    // 8. 依照同名的選擇建立紀錄；失敗時盡力刪除已 persist 的 blob。
    //    同名重複上傳是常見情況，全部留給垃圾回收會讓磁碟被慢慢吃光。
    let file = NewFile {
        owner_id: user.id,
        parent_id: params.parent_id,
        name: &name,
        size,
        storage_key: &key,
    };
    match register::register_file(&pool, &data_dir, &file, params.on_conflict).await {
        Ok(entry) => Ok((StatusCode::CREATED, Json(entry.into()))),
        Err(e) => {
            if let Err(remove_err) = storage::remove_blob(&data_dir, &key).await {
                // 刪除失敗只記錄，不蓋掉原本的錯誤；殘留的 blob 交給垃圾回收
                eprintln!("failed to remove the unregistered blob {key}: {remove_err}");
            }
            Err(e.into_app_error())
        }
    }
}

// ==================== 錯誤對應 ====================

pub(super) fn folder_not_found() -> AppError {
    AppError::NotFound(FOLDER_NOT_FOUND.to_string())
}

/// 資料庫拒絕建立檔案或資料夾的原因 → HTTP 狀態碼。
/// ParentNotFolder 與 ParentNotOwned 必須完全相同，見 `CreateFileError` 的說明。
fn create_error(e: CreateFileError) -> AppError {
    match e {
        CreateFileError::NameConflict => AppError::Conflict("同名項目已存在".to_string()),
        CreateFileError::ParentNotFolder | CreateFileError::ParentNotOwned => folder_not_found(),
        CreateFileError::Db(e) => AppError::internal(e),
    }
}

/// 儲存層錯誤 → HTTP 狀態碼
fn storage_error(e: StorageError) -> AppError {
    match e {
        StorageError::TooLarge { limit } => {
            AppError::PayloadTooLarge(format!("檔案超過大小上限（{limit} 位元組）"))
        }
        StorageError::Io(io_err) if io_err.kind() == io::ErrorKind::StorageFull => {
            eprintln!("disk full: {io_err}");
            AppError::InsufficientStorage("伺服器儲存空間不足".to_string())
        }
        // InvalidKey 是伺服器自己產生的 key 格式錯誤，屬於程式 bug
        other => AppError::internal(other),
    }
}

/// multipart 解析錯誤 → HTTP 狀態碼。
///
/// - 413：超過請求本體上限
/// - 其他 4xx：格式錯誤（缺少邊界、標頭不完整等）
/// - 5xx：axum 只在兩種情況回傳 5xx：
///   1. 讀取請求本體失敗：本體來自客戶端連線，失敗代表斷線或網路問題，**不是伺服器錯誤**
///   2. 內部鎖定失敗：只在同時持有多個欄位時發生，本 handler 依序讀取，結構上不會遇到
///
///   所以這裡把 5xx 視為「上傳中斷」，不記成內部錯誤，避免 log 誤導。
///   這個判斷依賴 axum 目前的對應方式；為了不遺失資訊，log 會保留完整的錯誤來源鏈。
fn multipart_error(e: MultipartError) -> AppError {
    let status = e.status();

    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return AppError::PayloadTooLarge("上傳內容超過大小上限".to_string());
    }

    if status.is_client_error() {
        return AppError::BadRequest(format!("上傳內容格式錯誤：{}", e.body_text()));
    }

    eprintln!(
        "upload interrupted (client connection problem): {}",
        error_chain(&e)
    );
    AppError::BadRequest("上傳中斷".to_string())
}

/// 把錯誤與它所有的來源串成一行，例如：
/// `failed to read stream ← error reading a body from connection ← connection reset`
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut source = e.source();
    while let Some(s) = source {
        parts.push(s.to_string());
        source = s.source();
    }
    parts.join(" ← ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::session::{generate_token, hash_token},
        db::{session, user},
    };
    use axum::{
        body::{Body, Bytes},
        http::{Method, Request, header},
    };
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const MIB: u64 = 1024 * 1024;
    const BOUNDARY: &str = "manorcloud-test-boundary";

    /// 測試環境：一個已登入的使用者、一個臨時資料目錄、一個組裝好的 app
    struct TestApp {
        app: Router,
        pool: SqlitePool,
        dir: TempDir,
        cookie: String,
    }

    async fn setup(pool: SqlitePool, limit: u64) -> TestApp {
        let user_id = user::create_user(&pool, "carol", "x").await.unwrap();
        let token = generate_token();
        session::create_session(&pool, user_id, &hash_token(&token), 4_000_000_000)
            .await
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        storage::init_dirs(dir.path()).unwrap();
        let state = AppState::new(pool.clone(), dir.path().to_path_buf(), limit);

        TestApp {
            app: super::super::app(state),
            pool,
            dir,
            cookie: format!("{}={token}", super::super::auth::SESSION_COOKIE),
        }
    }

    // ---------- 請求組裝 ----------

    /// multipart 中檔案欄位的開頭部分（邊界與標頭）。
    /// Content-Type 刻意宣告成 text/html，用來驗證伺服器不相信客戶端宣告的 MIME。
    fn part_head(filename: &str) -> String {
        format!(
            "--{BOUNDARY}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/html\r\n\r\n"
        )
    }

    fn multipart_request(uri: &str, cookie: Option<&str>, body: Body) -> Request<Body> {
        let mut builder = Request::post(uri).header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        builder.body(body).unwrap()
    }

    /// 組出完整的 multipart 上傳請求
    fn upload_request(
        uri: &str,
        cookie: Option<&str>,
        filename: &str,
        content: &[u8],
    ) -> Request<Body> {
        let mut body = part_head(filename).into_bytes();
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        multipart_request(uri, cookie, Body::from(body))
    }

    fn json_request(uri: &str, cookie: Option<&str>, json: &str) -> Request<Body> {
        let mut builder = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        builder.body(Body::from(json.to_string())).unwrap()
    }

    fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method(Method::GET).uri(uri);
        if let Some(c) = cookie {
            builder = builder.header(header::COOKIE, c);
        }
        builder.body(Body::empty()).unwrap()
    }

    // ---------- 送出與檢查 ----------

    async fn send(t: &TestApp, req: Request<Body>) -> (StatusCode, Bytes) {
        let res = t.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    fn json(body: &Bytes) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    /// 透過 API 建立資料夾，回傳 id
    async fn make_folder(t: &TestApp, name: &str, parent: Option<i64>) -> i64 {
        let parent = parent.map_or("null".to_string(), |p| p.to_string());
        let body = format!(r#"{{"name":"{name}","parent_id":{parent}}}"#);
        let (status, body) = send(t, json_request("/api/folders", Some(&t.cookie), &body)).await;
        assert_eq!(status, StatusCode::CREATED);
        json(&body)["id"].as_i64().unwrap()
    }

    /// 列出資料夾內容，回傳名稱清單（依回應順序）
    async fn list_names(t: &TestApp, uri: &str) -> Vec<String> {
        let (status, body) = send(t, get_request(uri, Some(&t.cookie))).await;
        assert_eq!(status, StatusCode::OK);
        json(&body)
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect()
    }

    /// 建立另一個使用者與他的資料夾，回傳資料夾 id
    async fn foreign_folder(t: &TestApp) -> i64 {
        let other = user::create_user(&t.pool, "alice", "x").await.unwrap();
        files_db::create_folder(&t.pool, other, None, "secret")
            .await
            .unwrap()
            .id
    }

    /// 遞迴計算目錄下的一般檔案數量
    fn count_files(dir: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| {
                let path = e.path();
                if path.is_dir() { count_files(&path) } else { 1 }
            })
            .sum()
    }

    fn blob_count(t: &TestApp) -> usize {
        count_files(&t.dir.path().join("blobs"))
    }

    fn tmp_count(t: &TestApp) -> usize {
        count_files(&t.dir.path().join("tmp"))
    }

    async fn row_count(t: &TestApp) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files")
            .fetch_one(&t.pool)
            .await
            .unwrap()
    }

    // ==================== 上傳 ====================

    #[sqlx::test]
    async fn upload_creates_blob_and_row(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let req = upload_request("/api/files", Some(&t.cookie), "a.txt", b"hello");
        let (status, body) = send(&t, req).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(blob_count(&t), 1);
        assert_eq!(tmp_count(&t), 0);
        assert_eq!(row_count(&t).await, 1);

        let body = std::str::from_utf8(&body).unwrap();
        // MIME 由檔名推斷，客戶端宣告的 text/html 被忽略；回應不含 storage_key
        assert!(body.contains("text/plain"), "{body}");
        assert!(!body.contains("text/html"), "{body}");
        assert!(!body.contains("storage_key"), "{body}");
    }

    #[sqlx::test]
    async fn larger_than_default_2mb_is_accepted(pool: SqlitePool) {
        let t = setup(pool, 4 * MIB).await;

        let content = vec![0u8; 3 * MIB as usize];
        let req = upload_request("/api/files", Some(&t.cookie), "big.bin", &content);
        let (status, _) = send(&t, req).await;

        assert_eq!(status, StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn too_large_returns_413_and_cleans_up(pool: SqlitePool) {
        let t = setup(pool, 1024).await;

        let content = vec![0u8; 2048];
        let req = upload_request("/api/files", Some(&t.cookie), "big.bin", &content);
        let (status, _) = send(&t, req).await;

        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(tmp_count(&t), 0);
        assert_eq!(blob_count(&t), 0);
        assert_eq!(row_count(&t).await, 0);
    }

    #[sqlx::test]
    async fn client_disconnect_is_not_an_internal_error(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        // 請求本體：檔案標頭、一部分內容，接著連線被重設（模擬使用者關掉瀏覽器）
        let chunks: Vec<Result<Bytes, io::Error>> = vec![
            Ok(Bytes::from(part_head("a.bin"))),
            Ok(Bytes::from(vec![0u8; 1024])),
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "client went away",
            )),
        ];
        let body = Body::from_stream(futures_util::stream::iter(chunks));
        let req = multipart_request("/api/files", Some(&t.cookie), body);

        let (status, _) = send(&t, req).await;

        // 斷線是客戶端的問題，不能是 500
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // Drop 守衛清掉了寫到一半的暫存檔，也沒有留下 blob 或資料庫紀錄
        assert_eq!(tmp_count(&t), 0);
        assert_eq!(blob_count(&t), 0);
        assert_eq!(row_count(&t).await, 0);
    }

    #[sqlx::test]
    async fn duplicate_name_keeps_both_by_default(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let first = upload_request("/api/files", Some(&t.cookie), "a.txt", b"one");
        let second = upload_request("/api/files", Some(&t.cookie), "A.TXT", b"two");
        assert_eq!(send(&t, first).await.0, StatusCode::CREATED);

        // 沒有指定任何選擇：自動改名，不拒絕
        let (status, body) = send(&t, second).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["name"], "A (1).TXT");
        assert_eq!(row_count(&t).await, 2);
        assert_eq!(blob_count(&t), 2);
    }

    #[sqlx::test]
    async fn duplicate_name_can_still_be_refused_without_leaking_blob(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let first = upload_request("/api/files", Some(&t.cookie), "a.txt", b"one");
        let second = upload_request(
            "/api/files?on_conflict=fail",
            Some(&t.cookie),
            "A.TXT",
            b"two",
        );
        assert_eq!(send(&t, first).await.0, StatusCode::CREATED);

        let (status, _) = send(&t, second).await;

        assert_eq!(status, StatusCode::CONFLICT);
        // 在接收內容之前就拒絕：磁碟上只有第一個
        assert_eq!(blob_count(&t), 1);
        assert_eq!(tmp_count(&t), 0);
        assert_eq!(row_count(&t).await, 1);
    }

    #[sqlx::test]
    async fn duplicate_name_can_replace(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let first = upload_request("/api/files", Some(&t.cookie), "a.txt", b"one");
        let (_, body) = send(&t, first).await;
        let original_id = json(&body)["id"].clone();

        let second = upload_request(
            "/api/files?on_conflict=replace",
            Some(&t.cookie),
            "a.txt",
            b"two",
        );
        let (status, body) = send(&t, second).await;

        assert_eq!(status, StatusCode::CREATED);
        // 同一個檔案，內容換了；舊的 blob 已經刪除
        assert_eq!(json(&body)["id"], original_id);
        assert_eq!(json(&body)["size"], 3);
        assert_eq!(row_count(&t).await, 1);
        assert_eq!(blob_count(&t), 1);
    }

    #[sqlx::test]
    async fn upload_into_own_folder(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let folder = make_folder(&t, "docs", None).await;

        let uri = format!("/api/files?parent_id={folder}");
        let req = upload_request(&uri, Some(&t.cookie), "a.txt", b"x");

        assert_eq!(send(&t, req).await.0, StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn upload_to_foreign_and_missing_folders_is_indistinguishable(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let foreign = foreign_folder(&t).await;

        let foreign_req = upload_request(
            &format!("/api/files?parent_id={foreign}"),
            Some(&t.cookie),
            "a.txt",
            b"x",
        );
        let missing_req = upload_request(
            "/api/files?parent_id=999999",
            Some(&t.cookie),
            "a.txt",
            b"x",
        );

        let foreign_res = send(&t, foreign_req).await;
        let missing_res = send(&t, missing_req).await;

        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        // 狀態碼與內容完全相同，無法從回應分辨兩者
        assert_eq!(foreign_res, missing_res);
        assert_eq!(blob_count(&t), 0);
    }

    #[sqlx::test]
    async fn invalid_filename_returns_400_before_writing(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let req = upload_request("/api/files", Some(&t.cookie), "a/b", b"x");
        let (status, _) = send(&t, req).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(tmp_count(&t), 0);
        assert_eq!(blob_count(&t), 0);
    }

    #[sqlx::test]
    async fn unauthenticated_upload_returns_401(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let req = upload_request("/api/files", None, "a.txt", b"x");

        assert_eq!(send(&t, req).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(row_count(&t).await, 0);
    }

    // ==================== 建立資料夾 ====================

    #[sqlx::test]
    async fn create_folder_returns_201(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let req = json_request("/api/folders", Some(&t.cookie), r#"{"name":"docs"}"#);
        let (status, body) = send(&t, req).await;

        assert_eq!(status, StatusCode::CREATED);
        let body = json(&body);
        assert_eq!(body["name"], "docs");
        assert_eq!(body["kind"], "folder");
        assert_eq!(body["parent_id"], Value::Null);
        assert!(body.get("storage_key").is_none());
    }

    #[sqlx::test]
    async fn duplicate_folder_returns_409(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        make_folder(&t, "docs", None).await;

        let req = json_request("/api/folders", Some(&t.cookie), r#"{"name":"DOCS"}"#);

        assert_eq!(send(&t, req).await.0, StatusCode::CONFLICT);
    }

    #[sqlx::test]
    async fn folder_name_is_validated(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        for name in ["", "..", "a/b", " docs"] {
            let body = format!(r#"{{"name":"{name}"}}"#);
            let req = json_request("/api/folders", Some(&t.cookie), &body);
            assert_eq!(send(&t, req).await.0, StatusCode::BAD_REQUEST, "{name:?}");
        }
        assert_eq!(row_count(&t).await, 0);
    }

    #[sqlx::test]
    async fn nested_folder_in_foreign_and_missing_parent_is_indistinguishable(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let foreign = foreign_folder(&t).await;

        let foreign_body = format!(r#"{{"name":"x","parent_id":{foreign}}}"#);
        let foreign_res = send(
            &t,
            json_request("/api/folders", Some(&t.cookie), &foreign_body),
        )
        .await;
        let missing_body = r#"{"name":"x","parent_id":999999}"#;
        let missing_res = send(
            &t,
            json_request("/api/folders", Some(&t.cookie), missing_body),
        )
        .await;

        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_res, missing_res);
    }

    #[sqlx::test]
    async fn unauthenticated_create_folder_returns_401(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let req = json_request("/api/folders", None, r#"{"name":"docs"}"#);

        assert_eq!(send(&t, req).await.0, StatusCode::UNAUTHORIZED);
    }

    // ==================== 列表 ====================

    #[sqlx::test]
    async fn list_root_puts_folders_first(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        for name in ["b.txt", "A.txt"] {
            let req = upload_request("/api/files", Some(&t.cookie), name, b"x");
            assert_eq!(send(&t, req).await.0, StatusCode::CREATED);
        }
        make_folder(&t, "zeta", None).await;
        make_folder(&t, "Alpha", None).await;

        assert_eq!(
            list_names(&t, "/api/files").await,
            ["Alpha", "zeta", "A.txt", "b.txt"]
        );
    }

    #[sqlx::test]
    async fn list_folder_contents(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let docs = make_folder(&t, "docs", None).await;
        make_folder(&t, "sub", Some(docs)).await;
        let req = upload_request(
            &format!("/api/files?parent_id={docs}"),
            Some(&t.cookie),
            "inner.txt",
            b"x",
        );
        assert_eq!(send(&t, req).await.0, StatusCode::CREATED);

        assert_eq!(list_names(&t, "/api/files").await, ["docs"]);
        assert_eq!(
            list_names(&t, &format!("/api/files?parent_id={docs}")).await,
            ["sub", "inner.txt"]
        );
    }

    #[sqlx::test]
    async fn list_empty_folder_returns_empty_array(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let docs = make_folder(&t, "docs", None).await;

        assert!(
            list_names(&t, &format!("/api/files?parent_id={docs}"))
                .await
                .is_empty()
        );
    }

    #[sqlx::test]
    async fn list_foreign_and_missing_folders_is_indistinguishable(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        let foreign = foreign_folder(&t).await;

        let foreign_res = send(
            &t,
            get_request(&format!("/api/files?parent_id={foreign}"), Some(&t.cookie)),
        )
        .await;
        let missing_res = send(
            &t,
            get_request("/api/files?parent_id=999999", Some(&t.cookie)),
        )
        .await;

        // 不是空陣列，而是與「不存在」完全相同的 404
        assert_eq!(foreign_res.0, StatusCode::NOT_FOUND);
        assert_eq!(foreign_res, missing_res);
    }

    #[sqlx::test]
    async fn list_does_not_show_other_users_root(pool: SqlitePool) {
        let t = setup(pool, MIB).await;
        foreign_folder(&t).await;

        assert!(list_names(&t, "/api/files").await.is_empty());
    }

    #[sqlx::test]
    async fn unauthenticated_list_returns_401(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        assert_eq!(
            send(&t, get_request("/api/files", None)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    // ==================== 路由組合 ====================

    /// 同一路徑的 GET 與 POST 都正常運作，而且放寬的上限仍作用在上傳上。
    /// 若組合順序寫錯（例如 get(...).layer(...).post(...)），上傳會退回 2 MB 預設值，
    /// 下面的 3 MB 上傳就會變成 413。
    #[sqlx::test]
    async fn get_and_post_share_path_with_scoped_limit(pool: SqlitePool) {
        let t = setup(pool, 4 * MIB).await;

        let content = vec![0u8; 3 * MIB as usize];
        let upload = upload_request("/api/files", Some(&t.cookie), "big.bin", &content);
        assert_eq!(send(&t, upload).await.0, StatusCode::CREATED);

        assert_eq!(list_names(&t, "/api/files").await, ["big.bin"]);
    }
    // ==================== 網址參數的檔名與設定 ====================

    #[sqlx::test]
    async fn name_parameter_preserves_quotes_and_percent(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        // 原名 a"b 100%.txt，經 URL 編碼；multipart 中的檔名會被忽略
        let req = upload_request(
            "/api/files?name=a%22b%20100%25.txt",
            Some(&t.cookie),
            "ignored.txt",
            b"x",
        );
        let (status, body) = send(&t, req).await;

        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json(&body)["name"], "a\"b 100%.txt");
    }

    #[sqlx::test]
    async fn name_parameter_is_validated(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        // a/b 經 URL 編碼：解碼後含有 /，必須被拒絕
        let req = upload_request("/api/files?name=a%2Fb", Some(&t.cookie), "ok.txt", b"x");

        assert_eq!(send(&t, req).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(blob_count(&t), 0);
    }

    #[sqlx::test]
    async fn config_reports_upload_limit(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        let (status, body) = send(&t, get_request("/api/config", Some(&t.cookie))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json(&body)["max_upload_bytes"], MIB);
    }

    #[sqlx::test]
    async fn config_requires_login(pool: SqlitePool) {
        let t = setup(pool, MIB).await;

        assert_eq!(
            send(&t, get_request("/api/config", None)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
