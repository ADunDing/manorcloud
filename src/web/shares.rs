//! 分享連結：把一個檔案或資料夾分享給沒有帳號的人，只能瀏覽與下載。
//!
//! 分享者（已登入）：
//!   POST   /api/shares?space=           建立：{path, password, expires_at}
//!   GET    /api/shares                  自己的分享，最新的在前
//!   DELETE /api/shares/{id}             停止分享
//!
//! 拿到連結的人（不需要登入）；網頁是 /s/{token}（assets.rs 回傳同一個 index.html）：
//!   GET    /api/public/shares/{token}                 分享的內容是什麼、要不要密碼
//!   POST   /api/public/shares/{token}/unlock          輸入密碼：{password}
//!   GET    /api/public/shares/{token}/items?path=     資料夾的內容
//!   GET    /api/public/shares/{token}/file?path=      下載一個檔案（支援 Range）
//!   GET    /api/public/shares/{token}/archive?path=&format=   把資料夾打包下載
//!
//! 安全：
//! - 連結中的代碼是 128 位元的亂數，猜不到
//! - 分享的是「路徑」：檔案改名、搬走或刪除後連結就失效；`path=` 只能往下走（RelPath 不接受 ..），
//!   不會碰到分享範圍以外的檔案。隱藏檔不列出也不能下載
//! - 有密碼時，輸入正確後瀏覽器收到一個只用在這個連結的 Cookie。它是由連結的代碼與密碼的雜湊
//!   算出來的：不需要另外存在伺服器上，改密碼或刪除分享後就失效。猜密碼與登入使用同一套次數限制
//! - 只能分享自己可以修改的項目：一般使用者自己的 My files，管理員不能分享別人資料夾中的檔案
//! - 分享者被停用或刪除後，連結就失效
//! - 只能下載一次的連結（只有檔案）：下載開始時就刪除分享，之後的請求一律當成失效。
//!   不支援續傳：下載中斷就要重新建立連結
//! - 下載的回應與一般下載相同（attachment、nosniff、sandbox，見 download.rs）

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{delete, get, post},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    auth::{LoginAttempt, LoginError, hash_password, verify_password},
    download::{ArchiveFormat, ArchiveItem, archive_response, file_response},
    error::AppError,
    extract::{ClientIp, CurrentUser, InSpace},
    spaces::{blocking, parse_path},
    state::{AppState, CookieSecure},
};
use crate::{
    bootstrap::generate_password,
    db::shares::{self as shares_db, NewShare, Share},
    space::{self, Space, SpaceError, SpaceKey},
    tree::{self, RelPath},
};

const SHARE_COOKIE: &str = "manorcloud_share";
/// 分享的密碼最長幾個字元（與帳號密碼相同的理由：不把超長輸入交給 Argon2）
const PASSWORD_MAX: usize = 128;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/shares", get(list).post(create))
        .route("/api/shares/{id}", delete(remove))
        .route("/api/public/shares/{token}", get(info))
        .route("/api/public/shares/{token}/unlock", post(unlock))
        .route("/api/public/shares/{token}/items", get(items))
        .route("/api/public/shares/{token}/file", get(file))
        .route("/api/public/shares/{token}/archive", get(archive))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 連結失效時一律回答相同的訊息：不透露是過期、被刪除還是檔案不見了
fn gone() -> AppError {
    AppError::NotFound("This link has expired or is no longer shared".to_string())
}

// ==================== 分享者 ====================

#[derive(Serialize)]
struct ShareResponse {
    id: i64,
    token: String,
    space: String,
    path: String,
    /// 分享的項目名稱
    name: String,
    kind: String,
    has_password: bool,
    /// 只能下載一次
    one_time: bool,
    expires_at: Option<i64>,
    expired: bool,
    downloads: i64,
    created_at: i64,
}

impl From<Share> for ShareResponse {
    fn from(s: Share) -> Self {
        ShareResponse {
            name: s.path.rsplit('/').next().unwrap_or("").to_string(),
            expired: s.expires_at.is_some_and(|at| at <= unix_now()),
            has_password: s.password_hash.is_some(),
            one_time: s.one_time,
            id: s.id,
            token: s.token,
            space: s.space,
            path: s.path,
            kind: s.kind,
            expires_at: s.expires_at,
            downloads: s.downloads,
            created_at: s.created_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    path: String,
    /// 空的或 null：不需要密碼
    #[serde(default)]
    password: Option<String>,
    /// 到期時間（Unix 秒）；null：不會到期
    #[serde(default)]
    expires_at: Option<i64>,
    /// 只能下載一次（只有檔案可以）
    #[serde(default)]
    one_time: bool,
}

/// POST /api/shares?space=
async fn create(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<ShareResponse>), AppError> {
    let rel = parse_path(&req.path)?;
    if rel.is_root() {
        return Err(AppError::BadRequest(
            "Choose a file or folder to share".to_string(),
        ));
    }
    space
        .check_modify(&rel)
        .map_err(|_| AppError::Conflict("You can only share your own files".to_string()))?;
    if req.expires_at.is_some_and(|at| at <= unix_now()) {
        return Err(AppError::BadRequest(
            "The expiry date must be in the future".to_string(),
        ));
    }
    let password = req.password.filter(|p| !p.is_empty());
    if password
        .as_ref()
        .is_some_and(|p| p.chars().count() > PASSWORD_MAX)
    {
        return Err(AppError::BadRequest(format!(
            "The password can be at most {PASSWORD_MAX} characters"
        )));
    }

    let root = space.root.clone();
    let abs = space.abs(&rel);
    let is_dir = blocking(move || {
        Ok(std::fs::symlink_metadata(tree::resolve(&root, &abs, true)?)?.is_dir())
    })
    .await?;
    if req.one_time && is_dir {
        return Err(AppError::BadRequest(
            "Only a file can be shared for a single download".to_string(),
        ));
    }
    let password_hash = match password {
        Some(p) => Some(hash_password(p).await?),
        None => None,
    };
    let token = generate_password();
    let created = shares_db::create(
        &pool,
        &NewShare {
            token: &token,
            owner_id: user.id,
            space: &space.key.to_param(),
            path: &rel.display(),
            kind: if is_dir { "folder" } else { "file" },
            password_hash: password_hash.as_deref(),
            expires_at: req.expires_at,
            one_time: req.one_time,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// GET /api/shares
async fn list(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<ShareResponse>>, AppError> {
    let shares = shares_db::list_for(&pool, user.id).await?;
    Ok(Json(shares.into_iter().map(ShareResponse::from).collect()))
}

/// DELETE /api/shares/{id}
async fn remove(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    if shares_db::delete(&pool, id, user.id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("Share not found".to_string()))
    }
}

// ==================== 拿到連結的人 ====================

/// 找出分享與它所在的空間。過期、分享者被停用、空間不存在：一律當成連結失效
async fn open(
    pool: &SqlitePool,
    data_dir: &Path,
    token: &str,
) -> Result<(Share, Space, RelPath), AppError> {
    let share = shares_db::find_by_token(pool, token)
        .await?
        .ok_or_else(gone)?;
    if share.expires_at.is_some_and(|at| at <= unix_now()) {
        return Err(gone());
    }
    let owner: Option<(String, bool)> =
        sqlx::query_as("SELECT role, disabled FROM users WHERE id = ?")
            .bind(share.owner_id)
            .fetch_optional(pool)
            .await?;
    let Some((role, false)) = owner else {
        return Err(gone());
    };
    let key = SpaceKey::parse(Some(&share.space)).ok_or_else(gone)?;
    let space = match space::resolve(pool, data_dir, share.owner_id, role == "admin", key).await {
        Ok(space) => space,
        Err(SpaceError::Unavailable(m)) => return Err(AppError::Unavailable(m)),
        Err(SpaceError::Db(e)) => return Err(e.into()),
        Err(_) => return Err(gone()),
    };
    let rel = RelPath::parse(&share.path).map_err(|_| gone())?;
    Ok((share, space, rel))
}

/// 由連結的代碼與密碼的雜湊算出的通行證（見最上方的說明）
fn pass_for(share: &Share) -> Option<String> {
    use sha2::{Digest, Sha256};
    let hash = share.password_hash.as_ref()?;
    let digest = Sha256::digest(format!("manorcloud-share:{}:{hash}", share.token).as_bytes());
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// 不需要密碼，或已經輸入過正確的密碼
fn unlocked(share: &Share, jar: &CookieJar) -> bool {
    match pass_for(share) {
        None => true,
        Some(expected) => jar.get(SHARE_COOKIE).is_some_and(|cookie| {
            let given = cookie.value().as_bytes();
            // 固定時間比對
            given.len() == expected.len()
                && given
                    .iter()
                    .zip(expected.as_bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        }),
    }
}

/// 需要密碼而還沒有輸入
fn locked() -> AppError {
    AppError::Unauthorized("Enter the password to open this link".to_string())
}

/// 分享範圍中的路徑。檔案的分享只有它自己；不能進入隱藏的資料夾
fn inside(share: &Share, space: &Space, rel: &RelPath, sub: &str) -> Result<RelPath, AppError> {
    let sub = parse_path(sub)?;
    if share.kind == "file" && !sub.is_root() {
        return Err(AppError::NotFound("Item not found".to_string()));
    }
    if sub.parts().iter().any(|part| tree::is_hidden_name(part)) {
        return Err(AppError::NotFound("Item not found".to_string()));
    }
    Ok(space.abs(&rel.join(&sub)))
}

#[derive(Serialize)]
struct InfoResponse {
    /// 需要密碼而還沒有輸入：其他欄位是 null
    locked: bool,
    name: Option<String>,
    kind: Option<String>,
    /// 檔案的大小
    size: Option<u64>,
    owner: Option<String>,
    expires_at: Option<i64>,
    /// 只能下載一次
    one_time: bool,
}

/// GET /api/public/shares/{token}
async fn info(
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(token): UrlPath<String>,
    jar: CookieJar,
) -> Result<Json<InfoResponse>, AppError> {
    let (share, space, rel) = open(&pool, &data_dir, &token).await?;
    if !unlocked(&share, &jar) {
        return Ok(Json(InfoResponse {
            locked: true,
            name: None,
            kind: None,
            size: None,
            owner: None,
            expires_at: None,
            one_time: share.one_time,
        }));
    }
    let root = space.root.clone();
    let abs = space.abs(&rel);
    let kind = share.kind.clone();
    let size = blocking(move || {
        let meta = std::fs::symlink_metadata(tree::resolve(&root, &abs, true)?)?;
        // 分享後被換成了不同種類的項目（檔案換成同名的資料夾）：當成不見了
        if meta.is_dir() != (kind == "folder") {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "changed"));
        }
        Ok(meta.is_file().then_some(meta.len()))
    })
    .await
    .map_err(|_| gone())?;
    let owner: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
        .bind(share.owner_id)
        .fetch_optional(&pool)
        .await?;
    Ok(Json(InfoResponse {
        locked: false,
        name: rel.name().map(str::to_string),
        kind: Some(share.kind),
        size,
        owner,
        expires_at: share.expires_at,
        one_time: share.one_time,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnlockRequest {
    password: String,
}

/// POST /api/public/shares/{token}/unlock：密碼正確時給這個連結的 Cookie
async fn unlock(
    State(state): State<AppState>,
    ClientIp(client): ClientIp,
    CookieSecure(secure): CookieSecure,
    UrlPath(token): UrlPath<String>,
    jar: CookieJar,
    Json(req): Json<UnlockRequest>,
) -> Result<(CookieJar, StatusCode), LoginError> {
    let AppState {
        pool,
        data_dir,
        login_limits: limits,
        ..
    } = state;
    let (share, _, _) = open(&pool, &data_dir, &token).await?;
    let Some(pass) = pass_for(&share) else {
        return Ok((jar, StatusCode::NO_CONTENT));
    };
    // 與登入相同的次數限制：同一個 IP、同一個連結猜錯太多次就暫時鎖定
    let mut attempt = LoginAttempt::begin(&limits, client, &format!("share:{}", share.id))
        .map_err(LoginError::Locked)?;
    if req.password.chars().count() > PASSWORD_MAX
        || !verify_password(req.password, share.password_hash.clone()).await?
    {
        return Err(AppError::Unauthorized("The password isn't correct".to_string()).into());
    }
    attempt.succeeded();
    let cookie = Cookie::build((SHARE_COOKIE, pass))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(secure)
        .path(format!("/api/public/shares/{token}"));
    Ok((jar.add(cookie), StatusCode::NO_CONTENT))
}

#[derive(Deserialize)]
struct SubPathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct ItemResponse {
    name: String,
    kind: &'static str,
    size: Option<u64>,
    modified: i64,
}

/// GET /api/public/shares/{token}/items?path=
async fn items(
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(token): UrlPath<String>,
    Query(query): Query<SubPathQuery>,
    jar: CookieJar,
) -> Result<Json<Vec<ItemResponse>>, AppError> {
    let (share, space, rel) = open(&pool, &data_dir, &token).await?;
    if !unlocked(&share, &jar) {
        return Err(locked());
    }
    if share.kind != "folder" {
        return Err(AppError::NotFound("Item not found".to_string()));
    }
    let abs = inside(&share, &space, &rel, &query.path)?;
    let root = space.root.clone();
    let entries = blocking(move || tree::list(&root, &abs)).await?;
    Ok(Json(
        entries
            .into_iter()
            .filter(|e| !tree::is_hidden_name(&e.name))
            .map(|e| ItemResponse {
                kind: if e.is_dir { "folder" } else { "file" },
                size: (!e.is_dir).then_some(e.size),
                modified: e.modified,
                name: e.name,
            })
            .collect(),
    ))
}

/// GET /api/public/shares/{token}/file?path=
async fn file(
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(token): UrlPath<String>,
    Query(query): Query<SubPathQuery>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (share, space, rel) = open(&pool, &data_dir, &token).await?;
    if !unlocked(&share, &jar) {
        return Err(locked());
    }
    let abs = inside(&share, &space, &rel, &query.path)?;
    let name = abs
        .name()
        .ok_or_else(|| AppError::NotFound("Item not found".to_string()))?
        .to_string();
    let root = space.root.clone();
    let (file, meta, etag) = blocking(move || {
        let (file, meta) = tree::open_file(&root, &abs)?;
        let etag = tree::etag(&tree::resolve(&root, &abs, true)?, &meta);
        Ok((file, meta, etag))
    })
    .await?;
    // 只能下載一次：檔案確定可以讀取之後、開始傳送之前刪除分享。
    // 同時有兩個請求時只有一個刪得掉，另一個當成失效。整個檔案從頭傳送，不接受 Range
    let headers = if share.one_time {
        if !shares_db::consume(&pool, share.id).await? {
            return Err(gone());
        }
        HeaderMap::new()
    } else {
        // 續傳的後續請求（Range 不是從頭開始）不重複計算
        if !headers.contains_key(header::RANGE) {
            shares_db::count_download(&pool, share.id).await?;
        }
        headers
    };
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
    #[serde(default)]
    path: String,
    /// zip（預設）、tar、tar.gz 或 tar.zst
    #[serde(default)]
    format: ArchiveFormat,
}

/// GET /api/public/shares/{token}/archive?path=&format=：資料夾（含子資料夾）打包下載
async fn archive(
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(token): UrlPath<String>,
    Query(query): Query<ArchiveQuery>,
    jar: CookieJar,
) -> Result<Response, AppError> {
    let (share, space, rel) = open(&pool, &data_dir, &token).await?;
    if !unlocked(&share, &jar) {
        return Err(locked());
    }
    if share.kind != "folder" {
        return Err(AppError::NotFound("Item not found".to_string()));
    }
    let abs = inside(&share, &space, &rel, &query.path)?;
    let name = abs.name().unwrap_or("files").to_string();
    let root = space.root.clone();
    let entries = blocking(move || {
        tree::resolve_dir(&root, &abs)?;
        tree::walk(&root, &abs, &|name: &str| !tree::is_hidden_name(name))
    })
    .await?;
    shares_db::count_download(&pool, share.id).await?;
    let items = entries
        .into_iter()
        .map(|item| ArchiveItem {
            path: item.path,
            mtime: item.modified,
            source: (!item.is_dir).then_some(item.source),
        })
        .collect();
    archive_response(items, &[name], query.format)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request};
    use axum::http::{Method, StatusCode, header};
    use serde_json::json;
    use sqlx::SqlitePool;

    #[sqlx::test]
    async fn a_shared_folder_can_be_browsed_and_downloaded_without_logging_in(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir_all(home.join("相片/旅行")).unwrap();
        std::fs::write(home.join("相片/a.jpg"), b"aaa").unwrap();
        std::fs::write(home.join("相片/.secret"), b"x").unwrap();
        std::fs::write(home.join("other.txt"), b"x").unwrap();

        let (status, body) = t
            .json(Method::POST, "/api/shares", Some(json!({"path": "相片"})))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let token = body["token"].as_str().unwrap().to_string();
        let base = format!("/api/public/shares/{token}");
        // 沒有 Cookie 的請求：拿到連結的陌生人
        let get = |uri: String| t.send(empty_request(Method::GET, &uri, None));

        let (status, _, body) = get(base.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            (info["name"].as_str(), info["locked"].as_bool()),
            (Some("相片"), Some(false))
        );

        let (_, _, body) = get(format!("{base}/items")).await;
        let items: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let mut names: Vec<&str> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["a.jpg", "旅行"]);

        let (status, _, body) = get(format!("{base}/file?path=a.jpg")).await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"aaa"[..]));
        // 分享範圍以外、隱藏檔、往上走都不行
        for path in [".secret", "../other.txt", "%2E%2E/other.txt"] {
            let (status, _, _) = get(format!("{base}/file?path={path}")).await;
            assert!(status.is_client_error(), "{path}: {status}");
        }
        let (status, headers, _) = get(format!("{base}/archive")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/zip");

        // 停止分享之後失效
        let id = t.json(Method::GET, "/api/shares", None).await.1[0]["id"].clone();
        assert_eq!(
            t.json(Method::GET, "/api/shares", None).await.1[0]["downloads"],
            2
        );
        let (status, _) = t
            .json(Method::DELETE, &format!("/api/shares/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = get(base).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn a_password_protects_the_link(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        std::fs::write(t.home("carol").join("a.txt"), b"hello").unwrap();
        let (_, body) = t
            .json(
                Method::POST,
                "/api/shares",
                Some(json!({"path": "a.txt", "password": "open sesame"})),
            )
            .await;
        let token = body["token"].as_str().unwrap().to_string();
        let base = format!("/api/public/shares/{token}");

        let (_, _, body) = t.send(empty_request(Method::GET, &base, None)).await;
        let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(info["locked"], true);
        assert!(info["name"].is_null());
        let (status, _, _) = t
            .send(empty_request(Method::GET, &format!("{base}/file"), None))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let unlock = |password: &str| {
            super::super::test_support::request(Method::POST, &format!("{base}/unlock"), None)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    json!({ "password": password }).to_string(),
                ))
                .unwrap()
        };
        let (status, _, _) = t.send(unlock("wrong")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, headers, _) = t.send(unlock("open sesame")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let cookie = headers[header::SET_COOKIE].to_str().unwrap();
        let cookie = cookie.split(';').next().unwrap().to_string();

        let (status, _, body) = t
            .send(empty_request(
                Method::GET,
                &format!("{base}/file"),
                Some(&cookie),
            ))
            .await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"hello"[..]));
    }

    #[sqlx::test]
    async fn sharing_is_validated(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        std::fs::write(t.home("carol").join("a.txt"), b"x").unwrap();
        for body in [
            json!({"path": ""}),
            json!({"path": "nope.txt"}),
            json!({"path": "a.txt", "expires_at": 1}),
        ] {
            let (status, _) = t
                .json(Method::POST, "/api/shares", Some(body.clone()))
                .await;
            assert!(status.is_client_error(), "{body}: {status}");
        }
        // 過期的連結
        let (_, body) = t
            .json(
                Method::POST,
                "/api/shares",
                Some(json!({"path": "a.txt", "expires_at": 4_000_000_000_i64})),
            )
            .await;
        sqlx::query("UPDATE shares SET expires_at = 1")
            .execute(&t.pool)
            .await
            .unwrap();
        let uri = format!("/api/public/shares/{}", body["token"].as_str().unwrap());
        let (status, _, _) = t.send(empty_request(Method::GET, &uri, None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    #[sqlx::test]
    async fn a_one_time_link_works_once(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::write(home.join("a.txt"), b"once").unwrap();
        std::fs::create_dir(home.join("docs")).unwrap();

        let (status, _) = t
            .json(
                Method::POST,
                "/api/shares",
                Some(json!({"path": "docs", "one_time": true})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (_, body) = t
            .json(
                Method::POST,
                "/api/shares",
                Some(json!({"path": "a.txt", "one_time": true})),
            )
            .await;
        assert_eq!(body["one_time"], true);
        let uri = format!(
            "/api/public/shares/{}/file",
            body["token"].as_str().unwrap()
        );
        let (status, _, body) = t.send(empty_request(Method::GET, &uri, None)).await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"once"[..]));
        let (status, _, _) = t.send(empty_request(Method::GET, &uri, None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(t.json(Method::GET, "/api/shares", None).await.1, json!([]));
    }
}
