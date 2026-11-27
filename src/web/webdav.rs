//! WebDAV（RFC 4918）：Finder、Windows 檔案總管、rclone，以及 Obsidian、Joplin、KeePass 這類 App
//! 把自己的 My files 當成一個網路磁碟。
//!
//!   網址 https://主機/dav/   帳號：ManorCloud 的帳號   密碼：應用程式密碼（見 app_passwords.rs）
//!
//! 管理員在管理頁面開啟之前，/dav/ 一律是 404。
//!
//! 和網頁遵守同樣的規則：
//! - 刪除：放進垃圾桶，可以還原。MOVE、COPY 蓋掉的資料夾也是
//! - 蓋掉同名的檔案（PUT，或把檔案 MOVE、COPY 到同名的檔案上）：直接取代，和網頁上傳時選「取代」一樣。
//!   筆記與密碼管理 App 每次儲存都會這樣做，放進垃圾桶只會塞滿垃圾桶
//! - 配額、上傳上限、檔名的規則；管理員看到的 users 資料夾唯讀
//!
//! 鎖定（LOCK）只是形式：Finder 與 Windows 必須看到伺服器支援鎖定才肯寫入，
//! 但 ManorCloud 不記錄鎖、也不會因為鎖而拒絕請求。網頁和 WebDAV 同時修改同一個檔案時，後寫的贏。

// 這裡的函式直接以 Response 當作錯誤，交給 axum 回應給用戶端。
// 新版 clippy 嫌它太大（128 位元組），但每個請求只會產生一次，裝箱沒有好處
#![allow(clippy::result_large_err)]

use std::io;
use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::any,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use super::{
    app_passwords,
    auth::{LoginAttempt, unix_now},
    csrf,
    download::file_response,
    error::AppError,
    extract::{ClientIp, CurrentUser, resolve_space},
    spaces::{blocking, check_name, check_quota, move_to_trash},
    state::AppState,
    webdav_xml::{self as xml, MultiStatus, PropFind, PropName},
};
use crate::{
    db::app_passwords as db,
    space::{Space, SpaceKey},
    storage,
    tree::{self, OnConflict, RelPath},
};

/// WebDAV 的網址都在這底下
const PREFIX: &str = "/dav";
/// PROPFIND、PROPPATCH、LOCK 的 XML 最多多大
const MAX_XML_BYTES: usize = 64 * 1024;
/// 回應給用戶端的鎖定時間（秒）
const LOCK_SECONDS: u64 = 3600;
/// 支援的方法
const ALLOW: &str =
    "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, COPY, MOVE, LOCK, UNLOCK";

/// 網址中每一段名稱的編碼：除了英數字與 - . _ ~ 之外都編碼
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/dav", any(handle))
        .route("/dav/", any(handle))
        .route("/dav/{*path}", any(handle))
        // 上傳的大小由上傳上限決定，一邊接收一邊檢查（tree::Upload）
        .layer(DefaultBodyLimit::disable())
        // WebDAV 用戶端不會送 Origin；瀏覽器從其他網站送來的請求一律拒絕
        .layer(middleware::from_fn(csrf::check_origin))
}

type DavResult = Result<Response, Response>;

/// 一個已經通過驗證的請求
struct Ctx {
    state: AppState,
    user: CurrentUser,
    space: Space,
    headers: HeaderMap,
}

async fn handle(
    State(state): State<AppState>,
    ClientIp(client): ClientIp,
    request: Request,
) -> Response {
    match serve(state, client, request).await {
        Ok(response) | Err(response) => response,
    }
}

async fn serve(state: AppState, client: IpAddr, request: Request) -> DavResult {
    if !state.webdav.enabled() {
        return Err(status(StatusCode::NOT_FOUND));
    }
    let (parts, body) = request.into_parts();
    let rel = dav_path(parts.uri.path()).ok_or_else(|| status(StatusCode::NOT_FOUND))?;
    // OPTIONS 不需要登入：Windows 先用它確認這是不是 WebDAV 伺服器
    if parts.method == Method::OPTIONS {
        return Ok(options());
    }
    let user = authenticate(&state, &parts.headers, client).await?;
    let space = resolve_space(&state.pool, &state.data_dir, &user, SpaceKey::Me)
        .await
        .map_err(app_error)?;
    let cx = Ctx {
        state,
        user,
        space,
        headers: parts.headers,
    };
    match parts.method.as_str() {
        "GET" => get(&cx, &rel, false).await,
        "HEAD" => get(&cx, &rel, true).await,
        "PROPFIND" => propfind(&cx, &rel, body).await,
        "PROPPATCH" => proppatch(&cx, &rel, body).await,
        "PUT" => put(&cx, &rel, body).await,
        "DELETE" => delete(&cx, &rel).await,
        "MKCOL" => mkcol(&cx, &rel, body).await,
        "COPY" => copy_or_move(&cx, &rel, false).await,
        "MOVE" => copy_or_move(&cx, &rel, true).await,
        "LOCK" => lock(&cx, &rel, body).await,
        "UNLOCK" => Ok(status(StatusCode::NO_CONTENT)),
        _ => Err((StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, ALLOW)]).into_response()),
    }
}

// ==================== 共用 ====================

fn status(code: StatusCode) -> Response {
    code.into_response()
}

/// WebDAV 只提供自己的 My files，abs 不會拒絕；萬一拒絕，當成不存在
fn not_found(_: crate::space::OutOfReach) -> Response {
    status(StatusCode::NOT_FOUND)
}

fn app_error(e: AppError) -> Response {
    e.into_response()
}

fn io_failure(e: io::Error) -> Response {
    app_error(super::spaces::io_error(e))
}

/// 不能修改的地方（管理員看到的 users、正在搬到另一顆硬碟）
fn forbidden(reason: String) -> Response {
    (StatusCode::FORBIDDEN, reason).into_response()
}

/// 網址的路徑 → 空間中的路徑。不在 /dav 底下、或不是合法的路徑時是 None
fn dav_path(path: &str) -> Option<RelPath> {
    let rest = path.strip_prefix(PREFIX)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let decoded = percent_decode_str(rest).decode_utf8().ok()?;
    RelPath::parse(&decoded).ok()
}

/// 空間中的路徑 → 回應中的網址。資料夾以 / 結尾
fn href(rel: &RelPath, is_dir: bool) -> String {
    let mut out = String::from(PREFIX);
    for part in rel.parts() {
        out.push('/');
        out.extend(utf8_percent_encode(part, SEGMENT));
    }
    if is_dir || rel.is_root() {
        out.push('/');
    }
    out
}

fn http_date(seconds: i64) -> String {
    httpdate::fmt_http_date(tree::unix_time(seconds))
}

/// 硬碟上存在的一個檔案或資料夾
struct Found {
    path: PathBuf,
    meta: std::fs::Metadata,
}

/// 找出一個項目；不存在（或是符號連結、特殊檔案）時是 None
async fn find(space: &Space, rel: &RelPath) -> Result<Option<Found>, Response> {
    let root = space.root.clone();
    let abs = space.abs(rel).map_err(not_found)?;
    blocking(move || match tree::resolve(&root, &abs, true) {
        Ok(path) => {
            let meta = std::fs::symlink_metadata(&path)?;
            Ok((meta.is_file() || meta.is_dir()).then_some(Found { path, meta }))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    })
    .await
    .map_err(app_error)
}

/// 必須存在的資料夾（放新項目的地方）。不存在時是 409：RFC 4918 規定上層不存在要回 409
async fn find_dir(space: &Space, rel: &RelPath) -> Result<Found, Response> {
    find(space, rel)
        .await?
        .filter(|found| found.meta.is_dir())
        .ok_or_else(|| status(StatusCode::CONFLICT))
}

fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// 讀取 XML 的請求內容
async fn read_xml(body: Body) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, MAX_XML_BYTES)
        .await
        .map_err(|_| status(StatusCode::PAYLOAD_TOO_LARGE))
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

// ==================== 登入 ====================

fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"ManorCloud\", charset=\"UTF-8\"",
        )],
    )
        .into_response()
}

/// Authorization: Basic 的帳號與密碼
fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let value = header_text(headers, header::AUTHORIZATION.as_str())?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (username, password) = text.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// 用帳號與應用程式密碼登入。
///
/// 應用程式密碼是 100 位元的隨機字串，比對只需要一次 SHA-256：不必像登入密碼那樣
/// 為每個請求佔用登入的名額（Finder 會同時送出很多請求）。
/// 猜錯照樣計入登入次數限制，被鎖定的 IP 或帳號回 429
async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    client: IpAddr,
) -> Result<CurrentUser, Response> {
    let Some((username, password)) = basic_credentials(headers) else {
        return Err(challenge());
    };
    let found = db::find_login(&state.pool, &username, &app_passwords::hash(&password))
        .await
        .map_err(|e| app_error(e.into()))?;
    let Some(login) = found else {
        // 記一次失敗：attempt 被丟棄時記錄
        return match LoginAttempt::begin(&state.login_limits, client, &username) {
            Ok(_attempt) => Err(challenge()),
            Err(wait) => Err((
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, wait.as_secs().max(1).to_string())],
            )
                .into_response()),
        };
    };
    if login.blocked {
        return Err(forbidden(
            "Log in to ManorCloud in a web browser first: there's something you need to do there"
                .to_string(),
        ));
    }
    let now = unix_now();
    if login
        .last_used_at
        .is_none_or(|last| last < now - db::TOUCH_EVERY_SECONDS)
        && let Err(e) = db::touch(&state.pool, login.password_id, now).await
    {
        eprintln!("cannot record when an app password was used: {e}");
    }
    Ok(CurrentUser {
        id: login.user_id,
        username: login.username,
        role: login.role,
    })
}

// ==================== OPTIONS、GET ====================

fn options() -> Response {
    (
        StatusCode::OK,
        [
            (header::HeaderName::from_static("dav"), "1, 2"),
            (header::ALLOW, ALLOW),
            // Windows 的舊版 Office 用它判斷可以用 WebDAV 存檔
            (header::HeaderName::from_static("ms-author-via"), "DAV"),
        ],
    )
        .into_response()
}

/// 用瀏覽器打開 /dav/ 時看到的說明
const FOLDER_TEXT: &str = "This is ManorCloud's WebDAV address. Add it as a network drive in Finder, File Explorer or a WebDAV app, and sign in with an app password from Settings.\n";

async fn get(cx: &Ctx, rel: &RelPath, head: bool) -> DavResult {
    let found = find(&cx.space, rel)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND))?;
    if found.meta.is_dir() {
        let text = if head { "" } else { FOLDER_TEXT };
        return Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response());
    }
    let name = rel.name().unwrap_or_default().to_string();
    let root = cx.space.root.clone();
    let abs = cx.space.abs(rel).map_err(not_found)?;
    let (file, meta, etag) = blocking(move || {
        let (file, meta) = tree::open_file(&root, &abs)?;
        let etag = tree::etag(&tree::resolve(&root, &abs, true)?, &meta);
        Ok((file, meta, etag))
    })
    .await
    .map_err(app_error)?;
    let response = file_response(
        tokio::fs::File::from_std(file),
        meta.len(),
        &name,
        &etag,
        &cx.headers,
    )
    .await
    .map_err(app_error)?;
    let (mut parts, body) = response.into_parts();
    if let Ok(value) = HeaderValue::from_str(&http_date(tree::modified(&meta))) {
        parts.headers.insert(header::LAST_MODIFIED, value);
    }
    let body = if head { Body::empty() } else { body };
    Ok(Response::from_parts(parts, body))
}

// ==================== PROPFIND、PROPPATCH ====================

/// PROPFIND 回應中的一個項目
struct Info {
    rel: RelPath,
    is_dir: bool,
    size: u64,
    modified: i64,
    etag: Option<String>,
}

impl Info {
    fn new(rel: RelPath, path: &Path, meta: &std::fs::Metadata) -> Self {
        Info {
            rel,
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: tree::modified(meta),
            etag: meta.is_file().then(|| tree::etag(path, meta)),
        }
    }

    /// 這個項目所有的屬性與內容（內容已經跳脫）
    fn props(&self) -> Vec<(PropName, String)> {
        let name = self.rel.name().unwrap_or("ManorCloud");
        let mut props = vec![
            (
                PropName::dav("resourcetype"),
                if self.is_dir {
                    "<D:collection/>".to_string()
                } else {
                    String::new()
                },
            ),
            (PropName::dav("displayname"), xml::escape(name)),
            (PropName::dav("getlastmodified"), http_date(self.modified)),
            (
                PropName::dav("supportedlock"),
                xml::SUPPORTED_LOCK.to_string(),
            ),
            (PropName::dav("lockdiscovery"), String::new()),
        ];
        if !self.is_dir {
            let media = mime_guess::from_path(name).first_or_octet_stream();
            props.push((PropName::dav("getcontentlength"), self.size.to_string()));
            props.push((
                PropName::dav("getcontenttype"),
                xml::escape(media.essence_str()),
            ));
            if let Some(etag) = &self.etag {
                props.push((PropName::dav("getetag"), xml::escape(etag)));
            }
        }
        props
    }
}

fn multi_status(body: String) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

fn bad_xml() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "The XML in the request isn't valid",
    )
        .into_response()
}

async fn propfind(cx: &Ctx, rel: &RelPath, body: Body) -> DavResult {
    let body = read_xml(body).await?;
    let wanted = xml::propfind(&body).map_err(|_| bad_xml())?;
    // Depth: 0 只看自己；1 與 infinity 都只列出一層（infinity 在大的資料夾會非常慢）
    let children = header_text(&cx.headers, "depth").map(str::trim) != Some("0");

    let found = find(&cx.space, rel)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND))?;
    let mut items = vec![Info::new(rel.clone(), &found.path, &found.meta)];
    if found.meta.is_dir() && children {
        let space = cx.space.clone();
        let dir = rel.clone();
        let path = found.path.clone();
        let abs_dir = space.abs(&dir).map_err(not_found)?;
        let listed = blocking(move || {
            let mut items = Vec::new();
            for entry in tree::list(&space.root, &abs_dir)? {
                if space.is_ignored(&dir, &entry.name) {
                    continue;
                }
                let child = path.join(&entry.name);
                // 列出之後才被刪除：略過
                let Ok(meta) = std::fs::symlink_metadata(&child) else {
                    continue;
                };
                items.push(Info::new(dir.child(&entry.name), &child, &meta));
            }
            Ok(items)
        })
        .await
        .map_err(app_error)?;
        items.extend(listed);
    }

    let mut out = MultiStatus::new();
    for item in &items {
        let href = href(&item.rel, item.is_dir);
        let all = item.props();
        match &wanted {
            PropFind::All => out.response(&href, &all, &[]),
            PropFind::Names => {
                let names: Vec<(PropName, String)> = all
                    .into_iter()
                    .map(|(name, _)| (name, String::new()))
                    .collect();
                out.response(&href, &names, &[]);
            }
            PropFind::Props(names) => {
                let mut found = Vec::new();
                let mut missing = Vec::new();
                for name in names {
                    match all.iter().find(|(have, _)| have == name) {
                        Some(prop) => found.push(prop.clone()),
                        None => missing.push(name.clone()),
                    }
                }
                out.response(&href, &found, &missing);
            }
        }
    }
    Ok(multi_status(out.finish()))
}

/// PROPPATCH：ManorCloud 不保存自訂的屬性。Windows 複製檔案後會設定建立時間等屬性，
/// 回應「成功」讓它不顯示錯誤；實際上什麼都沒有改變
async fn proppatch(cx: &Ctx, rel: &RelPath, body: Body) -> DavResult {
    let body = read_xml(body).await?;
    let names = xml::proppatch(&body).map_err(|_| bad_xml())?;
    let found = find(&cx.space, rel)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND))?;
    let props: Vec<(PropName, String)> = names.into_iter().map(|n| (n, String::new())).collect();
    let mut out = MultiStatus::new();
    out.response(&href(rel, found.meta.is_dir()), &props, &[]);
    Ok(multi_status(out.finish()))
}

// ==================== PUT、MKCOL、DELETE ====================

/// 新項目的位置與名稱：確認可以在上層資料夾中新增、名稱合法
async fn new_item(cx: &Ctx, rel: &RelPath) -> Result<(RelPath, String, Found), Response> {
    let (Some(parent), Some(name)) = (rel.parent(), rel.name()) else {
        return Err(status(StatusCode::METHOD_NOT_ALLOWED));
    };
    cx.space.check_write_in(&parent).map_err(forbidden)?;
    let name = check_name(&cx.space, &parent, name).map_err(app_error)?;
    let dir = find_dir(&cx.space, &parent).await?;
    Ok((parent, name, dir))
}

async fn put(cx: &Ctx, rel: &RelPath, body: Body) -> DavResult {
    let (_, name, dir) = new_item(cx, rel).await?;
    let (dir_path, wanted) = (dir.path.clone(), name.clone());
    let existing = blocking(move || {
        Ok(match tree::find_same_name(&dir_path, &wanted, None)? {
            Some(existing) => Some(std::fs::symlink_metadata(dir_path.join(&existing))?.is_dir()),
            None => None,
        })
    })
    .await
    .map_err(app_error)?;
    if existing == Some(true) {
        return Err(status(StatusCode::METHOD_NOT_ALLOWED));
    }

    let declared: Option<u64> =
        header_text(&cx.headers, header::CONTENT_LENGTH.as_str()).and_then(|v| v.parse().ok());
    let quota = |size: u64| {
        check_quota(
            &cx.state.pool,
            &cx.state.data_dir,
            &cx.user,
            &cx.space,
            size,
        )
    };
    quota(declared.unwrap_or(0)).await.map_err(app_error)?;

    let mut upload = tree::Upload::create(&cx.space.tmp_dir(), cx.state.upload_limit.get())
        .await
        .map_err(io_failure)?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| status(StatusCode::BAD_REQUEST))?;
        upload.write_chunk(&chunk).await.map_err(io_failure)?;
    }
    match declared {
        Some(size) if size != upload.size() => return Err(status(StatusCode::BAD_REQUEST)),
        Some(_) => {}
        // 沒有事先說大小（分段傳送）：收完之後再確認一次配額
        None => quota(upload.size()).await.map_err(app_error)?,
    }

    let placed = upload
        .finish(dir.path.clone(), name, OnConflict::Replace)
        .await
        .map_err(io_failure)?;

    let code = if existing.is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    // rclone 等用戶端（ownCloud、Nextcloud 模式）用 X-OC-Mtime 告訴伺服器檔案原本的修改時間
    let mtime = header_text(&cx.headers, "x-oc-mtime").and_then(|v| v.trim().parse::<i64>().ok());
    if let Some(seconds) = mtime.filter(|s| *s > 0) {
        let path = dir.path.join(&placed);
        blocking(move || {
            tree::set_modified(&path, tree::unix_time(seconds));
            Ok(())
        })
        .await
        .map_err(app_error)?;
        return Ok((code, [("x-oc-mtime", "accepted")]).into_response());
    }
    Ok(status(code))
}

async fn mkcol(cx: &Ctx, rel: &RelPath, body: Body) -> DavResult {
    // 不支援帶內容的 MKCOL（RFC 4918 9.3）
    let body = read_xml(body).await?;
    if !body.is_empty() {
        return Err(status(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }
    if rel.is_root() || find(&cx.space, rel).await?.is_some() {
        return Err(status(StatusCode::METHOD_NOT_ALLOWED));
    }
    let (parent, name, _) = new_item(cx, rel).await?;
    let root = cx.space.root.clone();
    let abs = cx.space.abs(&parent).map_err(not_found)?;
    match blocking(move || tree::create_dir(&root, &abs, &name)).await {
        Ok(_) => Ok(status(StatusCode::CREATED)),
        // 只差大小寫的名稱已經存在
        Err(AppError::Conflict(_)) => Err(status(StatusCode::METHOD_NOT_ALLOWED)),
        Err(e) => Err(app_error(e)),
    }
}

async fn delete(cx: &Ctx, rel: &RelPath) -> DavResult {
    cx.space.check_modify(rel).map_err(forbidden)?;
    if find(&cx.space, rel).await?.is_none() {
        return Err(status(StatusCode::NOT_FOUND));
    }
    move_to_trash(&cx.state.pool, &cx.space, rel)
        .await
        .map_err(app_error)?;
    Ok(status(StatusCode::NO_CONTENT))
}

// ==================== COPY、MOVE ====================

/// Destination 標頭：完整的網址或路徑，必須在 /dav 底下
fn destination(headers: &HeaderMap) -> Result<RelPath, StatusCode> {
    let value = header_text(headers, "destination").ok_or(StatusCode::BAD_REQUEST)?;
    let path = match value.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |at| &rest[at..]),
        None => value,
    };
    let path = path.split(['?', '#']).next().unwrap_or_default();
    if !path.starts_with(PREFIX) {
        // 其他伺服器、或這台伺服器上 WebDAV 以外的地方
        return Err(StatusCode::BAD_GATEWAY);
    }
    dav_path(path).ok_or(StatusCode::BAD_REQUEST)
}

async fn copy_or_move(cx: &Ctx, from: &RelPath, moving: bool) -> DavResult {
    let to = destination(&cx.headers).map_err(status)?;
    let overwrite =
        !header_text(&cx.headers, "overwrite").is_some_and(|v| v.trim().eq_ignore_ascii_case("f"));
    if from.is_root() || to.is_root() || to == *from || to.starts_with(from) {
        return Err(forbidden(
            "A folder can't be copied or moved into itself".to_string(),
        ));
    }
    if moving {
        cx.space.check_modify(from).map_err(forbidden)?;
    }
    let source = find(&cx.space, from)
        .await?
        .ok_or_else(|| status(StatusCode::NOT_FOUND))?;
    let (to_parent, name, dir) = new_item(cx, &to).await?;

    // 目的地已經有同名（不分大小寫）的項目。改名只差大小寫時，那就是來源自己
    let (dir_path, wanted, except) = (
        dir.path.clone(),
        name.clone(),
        moving.then(|| source.path.clone()),
    );
    let existing = blocking(move || {
        Ok(
            match tree::find_same_name(&dir_path, &wanted, except.as_deref())? {
                Some(existing) => {
                    let meta = std::fs::symlink_metadata(dir_path.join(&existing))?;
                    Some((existing, meta))
                }
                None => None,
            },
        )
    })
    .await
    .map_err(app_error)?;

    // 檔案蓋過同名的檔案時直接取代；牽涉到資料夾時，舊的放進垃圾桶
    let mut replace = false;
    if let Some((existing, meta)) = &existing {
        if !overwrite {
            return Err(status(StatusCode::PRECONDITION_FAILED));
        }
        if same_file(meta, &source.meta) {
            return Err(forbidden("That's the same item".to_string()));
        }
        let existing_rel = to_parent.child(existing);
        cx.space.check_modify(&existing_rel).map_err(forbidden)?;
        if meta.is_dir() || source.meta.is_dir() {
            move_to_trash(&cx.state.pool, &cx.space, &existing_rel)
                .await
                .map_err(app_error)?;
        } else {
            replace = true;
        }
    }

    if moving {
        let target = match &existing {
            Some((existing, _)) if replace => dir.path.join(existing),
            _ => dir.path.join(&name),
        };
        let root = cx.space.root.clone();
        let (abs_from, abs_parent) = (
            cx.space.abs(from).map_err(not_found)?,
            cx.space.abs(&to_parent).map_err(not_found)?,
        );
        blocking(move || {
            if replace {
                // 同一個資料夾系統內的 rename 會一次換掉舊的檔案
                std::fs::rename(&source.path, &target)
            } else {
                tree::rename(&root, &abs_from, &abs_parent, &name).map(|_| ())
            }
        })
        .await
        .map_err(app_error)?;
    } else {
        let measured = source.path.clone();
        let size = blocking(move || Ok(tree::tree_size(&measured)))
            .await
            .map_err(app_error)?;
        check_quota(
            &cx.state.pool,
            &cx.state.data_dir,
            &cx.user,
            &cx.space,
            size,
        )
        .await
        .map_err(app_error)?;
        let (root, tmp) = (cx.space.root.clone(), cx.space.tmp_dir());
        let conflict = if replace {
            OnConflict::Replace
        } else {
            OnConflict::Fail
        };
        blocking(move || {
            if size > storage::available_bytes(&root)? {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "not enough space",
                ));
            }
            std::fs::create_dir_all(&tmp)?;
            let staging = tmp.join(format!("copy-{}", storage::generate_storage_key()));
            let result = tree::copy_into(&source.path, &staging)
                .and_then(|()| tree::place(&staging, &dir.path, &name, conflict));
            if result.is_err() && std::fs::symlink_metadata(&staging).is_ok() {
                let _ = tree::remove_path(&staging);
            }
            result.map(|_| ())
        })
        .await
        .map_err(app_error)?;
    }

    Ok(status(if existing.is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    }))
}

// ==================== LOCK ====================

/// 鎖的代碼：opaquelocktoken: 加上 UUID 格式的隨機值
fn new_lock_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("failed to get secure random bytes from the OS");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "opaquelocktoken:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 延長鎖定時 If 標頭中的代碼。只接受自己產生的格式，避免把任意的文字寫回回應中
fn lock_token_in(headers: &HeaderMap) -> Option<String> {
    let value = header_text(headers, "if")?;
    let start = value.find("<opaquelocktoken:")? + 1;
    let end = start + value[start..].find('>')?;
    let token = &value[start..end];
    let id = token.strip_prefix("opaquelocktoken:")?;
    (id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .then(|| token.to_string())
}

/// LOCK：只是形式（見模組說明）。對還不存在的名稱鎖定時，先建立一個空的檔案（RFC 4918 9.10.4），
/// Finder 存新檔案時就是這樣做
async fn lock(cx: &Ctx, rel: &RelPath, body: Body) -> DavResult {
    let body = read_xml(body).await?;
    let refresh = body.iter().all(u8::is_ascii_whitespace);
    let depth = if header_text(&cx.headers, "depth").map(str::trim) == Some("0") {
        "0"
    } else {
        "infinity"
    };

    let (is_dir, code) = match find(&cx.space, rel).await? {
        Some(found) => (found.meta.is_dir(), StatusCode::OK),
        None if refresh => return Err(status(StatusCode::PRECONDITION_FAILED)),
        None => {
            let (_, name, dir) = new_item(cx, rel).await?;
            check_quota(&cx.state.pool, &cx.state.data_dir, &cx.user, &cx.space, 0)
                .await
                .map_err(app_error)?;
            let upload = tree::Upload::create(&cx.space.tmp_dir(), cx.state.upload_limit.get())
                .await
                .map_err(io_failure)?;
            match upload.finish(dir.path, name, OnConflict::Fail).await {
                // 同時被別的請求建立了：一樣可以鎖
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_failure(e)),
            }
            (false, StatusCode::CREATED)
        }
    };

    let token = if refresh {
        lock_token_in(&cx.headers).unwrap_or_else(new_lock_token)
    } else {
        new_lock_token()
    };
    let body = xml::lock_discovery(&token, &href(rel, is_dir), depth, LOCK_SECONDS);
    let lock_header = HeaderValue::from_str(&format!("<{token}>"))
        .map_err(|e| app_error(AppError::internal(e)))?;
    Ok((
        code,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/xml; charset=utf-8"),
            ),
            (header::HeaderName::from_static("lock-token"), lock_header),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{HeaderMap, Method, Request, StatusCode, header};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sqlx::SqlitePool;

    use super::super::app_passwords;
    use super::super::test_support::{MIB, TestApp};
    use crate::db::app_passwords as db;

    const PASSWORD: &str = "abcde-fghij-kmnpq-rstuv";

    /// WebDAV 已開放、carol 有一組應用程式密碼
    async fn setup(pool: SqlitePool) -> TestApp {
        let t = TestApp::new(pool, MIB).await;
        t.state.webdav.set(true);
        db::create(&t.pool, t.user_id, "test", &app_passwords::hash(PASSWORD))
            .await
            .unwrap();
        t
    }

    fn basic(username: &str, password: &str) -> String {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{username}:{password}"))
        )
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: String,
    }

    async fn send(
        t: &TestApp,
        method: &str,
        uri: &str,
        auth: Option<&str>,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Reply {
        let mut builder = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(uri)
            .header(header::HOST, "localhost:3000");
        if let Some(auth) = auth {
            builder = builder.header(header::AUTHORIZATION, auth);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (status, headers, body) = t.send(builder.body(body.into()).unwrap()).await;
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    /// 用 carol 的應用程式密碼送出
    async fn dav(
        t: &TestApp,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: impl Into<Body>,
    ) -> Reply {
        let auth = basic("carol", PASSWORD);
        send(t, method, uri, Some(&auth), headers, body).await
    }

    async fn trash_count(t: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM trash")
            .fetch_one(&t.pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn webdav_is_off_until_an_administrator_turns_it_on(pool: SqlitePool) {
        let t = setup(pool).await;
        t.state.webdav.set(false);
        for method in ["OPTIONS", "PROPFIND", "GET"] {
            let reply = dav(&t, method, "/dav/", &[], Body::empty()).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method}");
        }
    }

    #[sqlx::test]
    async fn options_answer_without_signing_in(pool: SqlitePool) {
        let t = setup(pool).await;
        let reply = send(&t, "OPTIONS", "/dav/", None, &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers["dav"], "1, 2");
        assert!(
            reply.headers["allow"]
                .to_str()
                .unwrap()
                .contains("PROPFIND")
        );
    }

    #[sqlx::test]
    async fn signing_in_needs_an_app_password(pool: SqlitePool) {
        let t = setup(pool).await;
        let propfind = |auth: Option<String>| {
            let t = &t;
            async move {
                send(
                    t,
                    "PROPFIND",
                    "/dav/",
                    auth.as_deref(),
                    &[("depth", "0")],
                    Body::empty(),
                )
                .await
            }
        };

        let reply = propfind(None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert!(
            reply.headers[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Basic")
        );

        // 大小寫、空白與 - 不影響比對
        let typed = PASSWORD.to_uppercase().replace('-', " ");
        let reply = propfind(Some(basic("carol", &typed))).await;
        assert_eq!(reply.status, StatusCode::MULTI_STATUS, "{}", reply.body);
        let used: Option<i64> = sqlx::query_scalar("SELECT last_used_at FROM app_passwords")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert!(used.is_some());

        // 別人的帳號名稱配上 carol 的密碼
        t.other_user("jerry").await;
        let reply = propfind(Some(basic("jerry", PASSWORD))).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);

        // 臨時密碼還沒改：要先到網頁完成
        sqlx::query("UPDATE password_credentials SET must_change_password = 1 WHERE user_id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
        let reply = propfind(Some(basic("carol", PASSWORD))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);

        // 停用的帳號
        sqlx::query("UPDATE users SET disabled = 1 WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
        let reply = propfind(Some(basic("carol", PASSWORD))).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn wrong_passwords_count_toward_the_login_limit(pool: SqlitePool) {
        let t = setup(pool).await;
        let wrong = basic("carol", "wrong-password");
        for _ in 0..crate::auth::limiter::MAX_IP_FAILURES {
            let reply = send(&t, "PROPFIND", "/dav/", Some(&wrong), &[], Body::empty()).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        }
        let reply = send(&t, "PROPFIND", "/dav/", Some(&wrong), &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert!(reply.headers.contains_key(header::RETRY_AFTER));
    }

    #[sqlx::test]
    async fn revoked_app_passwords_stop_working(pool: SqlitePool) {
        let t = setup(pool).await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM app_passwords")
            .fetch_one(&t.pool)
            .await
            .unwrap();
        assert!(db::delete(&t.pool, t.user_id, id).await.unwrap());
        let reply = dav(&t, "PROPFIND", "/dav/", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn propfind_lists_a_folder(pool: SqlitePool) {
        let t = setup(pool).await;
        let home = t.home("carol");
        std::fs::create_dir(home.join("相片")).unwrap();
        std::fs::write(home.join("a b.txt"), b"hello").unwrap();
        std::fs::write(home.join("相片/1.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink("/etc", home.join("link")).unwrap();

        let reply = dav(&t, "PROPFIND", "/dav/", &[("depth", "1")], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::MULTI_STATUS);
        let body = &reply.body;
        assert_eq!(body.matches("<D:response>").count(), 3, "{body}");
        assert!(body.contains("<D:href>/dav/</D:href>"));
        assert!(body.contains("<D:href>/dav/a%20b.txt</D:href>"));
        assert!(body.contains("<D:href>/dav/%E7%9B%B8%E7%89%87/</D:href>"));
        assert!(body.contains("<D:getcontentlength>5</D:getcontentlength>"));
        assert!(body.contains("<D:collection/>"));
        assert!(body.contains("<D:getetag>"));
        // 符號連結不會出現，也打不開
        assert!(!body.contains("link"));
        let reply = dav(&t, "GET", "/dav/link/passwd", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);

        // Depth: 0 只有自己
        let reply = dav(
            &t,
            "PROPFIND",
            "/dav/%E7%9B%B8%E7%89%87",
            &[("depth", "0")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.body.matches("<D:response>").count(), 1);

        // 指定的屬性：沒有的放在 404
        let wanted = r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop>
            <D:getcontentlength/><D:quota-used-bytes/></D:prop></D:propfind>"#;
        let reply = dav(&t, "PROPFIND", "/dav/a%20b.txt", &[("depth", "0")], wanted).await;
        assert_eq!(reply.status, StatusCode::MULTI_STATUS);
        assert!(
            reply
                .body
                .contains("<D:getcontentlength>5</D:getcontentlength>")
        );
        assert!(reply.body.contains("<D:quota-used-bytes/>"));
        assert!(reply.body.contains("404 Not Found"));
        assert!(!reply.body.contains("getlastmodified"));

        let reply = dav(&t, "PROPFIND", "/dav/nothing", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = dav(&t, "PROPFIND", "/dav/", &[], "<not xml").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn paths_never_leave_my_files(pool: SqlitePool) {
        let t = setup(pool).await;
        std::fs::write(t.data_dir().join("secret.txt"), b"admin only").unwrap();
        t.home("carol");
        for uri in [
            "/dav/..%2Fsecret.txt",
            "/dav/%2e%2e/secret.txt",
            "/dav/..%2F..%2Fsecret.txt",
            "/dav/.manorcloud",
            "/dav/a%00b",
            "/davx",
        ] {
            let reply = dav(&t, "GET", uri, &[], Body::empty()).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        }
        // 搬移的目的地也一樣
        std::fs::write(t.home("carol").join("a.txt"), b"x").unwrap();
        let reply = dav(
            &t,
            "MOVE",
            "/dav/a.txt",
            &[("destination", "/dav/%2e%2e/a.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let reply = dav(
            &t,
            "MOVE",
            "/dav/a.txt",
            &[("destination", "http://localhost:3000/api/files")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
    }

    #[sqlx::test]
    async fn files_are_uploaded_downloaded_and_replaced(pool: SqlitePool) {
        let t = setup(pool).await;
        let home = t.home("carol");

        let reply = dav(&t, "PUT", "/dav/%E7%AD%86%E8%A8%98.md", &[], "first").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(std::fs::read(home.join("筆記.md")).unwrap(), b"first");

        let reply = dav(&t, "GET", "/dav/%E7%AD%86%E8%A8%98.md", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body, "first");
        assert!(reply.headers.contains_key(header::LAST_MODIFIED));
        let reply = dav(
            &t,
            "GET",
            "/dav/%E7%AD%86%E8%A8%98.md",
            &[("range", "bytes=1-2")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(reply.body, "ir");
        let reply = dav(&t, "HEAD", "/dav/%E7%AD%86%E8%A8%98.md", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body, "");

        // 蓋掉同名的檔案：直接取代，不放進垃圾桶（App 每次儲存都會這樣做）
        let reply = dav(
            &t,
            "PUT",
            "/dav/%E7%AD%86%E8%A8%98.md",
            &[("x-oc-mtime", "1700000000")],
            "second",
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(reply.headers["x-oc-mtime"], "accepted");
        assert_eq!(std::fs::read(home.join("筆記.md")).unwrap(), b"second");
        assert_eq!(trash_count(&t).await, 0);
        let modified = std::fs::metadata(home.join("筆記.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(modified, crate::tree::unix_time(1_700_000_000));

        // 上層不存在、資料夾本身、不合法的名稱、超過上傳上限
        let reply = dav(&t, "PUT", "/dav/missing/a.txt", &[], "x").await;
        assert_eq!(reply.status, StatusCode::CONFLICT);
        std::fs::create_dir(home.join("docs")).unwrap();
        let reply = dav(&t, "PUT", "/dav/docs", &[], "x").await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        let reply = dav(&t, "PUT", "/dav/", &[], "x").await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        let reply = dav(&t, "PUT", "/dav/%20space.txt", &[], "x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let big = vec![0u8; usize::try_from(MIB).unwrap() + 1];
        let reply = dav(&t, "PUT", "/dav/big.bin", &[], big).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!home.join("big.bin").exists());
        // 宣告的大小與收到的不符：沒收完
        let reply = dav(
            &t,
            "PUT",
            "/dav/short.txt",
            &[("content-length", "10")],
            "abc",
        )
        .await;
        assert!(reply.status.is_client_error());
        assert!(!home.join("short.txt").exists());
    }

    #[sqlx::test]
    async fn uploads_respect_the_storage_limit(pool: SqlitePool) {
        let t = setup(pool).await;
        t.home("carol");
        sqlx::query("UPDATE users SET quota_bytes = 10 WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();
        let reply = dav(&t, "PUT", "/dav/small.txt", &[], "12345").await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let reply = dav(&t, "PUT", "/dav/big.txt", &[], "1234567890").await;
        assert_eq!(reply.status, StatusCode::INSUFFICIENT_STORAGE);
        let reply = dav(
            &t,
            "PUT",
            "/dav/declared.txt",
            &[("content-length", "100")],
            vec![0u8; 100],
        )
        .await;
        assert_eq!(reply.status, StatusCode::INSUFFICIENT_STORAGE);
    }

    #[sqlx::test]
    async fn folders_are_created_copied_moved_and_deleted(pool: SqlitePool) {
        let t = setup(pool).await;
        let home = t.home("carol");

        assert_eq!(
            dav(&t, "MKCOL", "/dav/docs", &[], Body::empty())
                .await
                .status,
            StatusCode::CREATED
        );
        assert!(home.join("docs").is_dir());
        assert_eq!(
            dav(&t, "MKCOL", "/dav/docs", &[], Body::empty())
                .await
                .status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            dav(&t, "MKCOL", "/dav/DOCS", &[], Body::empty())
                .await
                .status,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            dav(&t, "MKCOL", "/dav/a/b", &[], Body::empty())
                .await
                .status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            dav(&t, "MKCOL", "/dav/x", &[], "<body/>").await.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        std::fs::write(home.join("a.txt"), b"aaa").unwrap();
        let copy = dav(
            &t,
            "COPY",
            "/dav/a.txt",
            &[("destination", "http://localhost:3000/dav/docs/a.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(copy.status, StatusCode::CREATED);
        assert_eq!(std::fs::read(home.join("docs/a.txt")).unwrap(), b"aaa");

        // 改名，包括只改大小寫
        let reply = dav(
            &t,
            "MOVE",
            "/dav/docs/a.txt",
            &[("destination", "/dav/docs/b.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let reply = dav(
            &t,
            "MOVE",
            "/dav/docs/b.txt",
            &[("destination", "/dav/docs/B.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert!(home.join("docs/B.txt").is_file());

        // 目的地有同名的檔案
        std::fs::write(home.join("c.txt"), b"ccc").unwrap();
        let reply = dav(
            &t,
            "MOVE",
            "/dav/c.txt",
            &[("destination", "/dav/docs/B.txt"), ("overwrite", "F")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::PRECONDITION_FAILED);
        let reply = dav(
            &t,
            "MOVE",
            "/dav/c.txt",
            &[("destination", "/dav/docs/b.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(!home.join("c.txt").exists());
        assert_eq!(std::fs::read(home.join("docs/B.txt")).unwrap(), b"ccc");
        let reply = dav(
            &t,
            "COPY",
            "/dav/a.txt",
            &[("destination", "/dav/docs/B.txt")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read(home.join("docs/B.txt")).unwrap(), b"aaa");
        assert_eq!(trash_count(&t).await, 0);

        // 資料夾被蓋掉時，舊的放進垃圾桶
        std::fs::create_dir(home.join("old")).unwrap();
        std::fs::write(home.join("old/keep.txt"), b"k").unwrap();
        let reply = dav(
            &t,
            "COPY",
            "/dav/docs",
            &[("destination", "/dav/old")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(home.join("old/B.txt").is_file());
        assert!(!home.join("old/keep.txt").exists());
        assert_eq!(trash_count(&t).await, 1);

        // 不能放進自己底下
        let reply = dav(
            &t,
            "MOVE",
            "/dav/docs",
            &[("destination", "/dav/docs/inner")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        let reply = dav(
            &t,
            "MOVE",
            "/dav/nothing",
            &[("destination", "/dav/x")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        let reply = dav(&t, "MOVE", "/dav/a.txt", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        // 刪除：放進垃圾桶
        let reply = dav(&t, "DELETE", "/dav/docs", &[], Body::empty()).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert!(!home.join("docs").exists());
        assert_eq!(trash_count(&t).await, 2);
        assert_eq!(
            dav(&t, "DELETE", "/dav/docs", &[], Body::empty())
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            dav(&t, "DELETE", "/dav/", &[], Body::empty()).await.status,
            StatusCode::FORBIDDEN
        );
    }

    #[sqlx::test]
    async fn locks_are_granted_and_create_missing_files(pool: SqlitePool) {
        let t = setup(pool).await;
        let home = t.home("carol");
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
            <D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope>
            <D:locktype><D:write/></D:locktype><D:owner><D:href>carol</D:href></D:owner></D:lockinfo>"#;

        let reply = dav(&t, "LOCK", "/dav/new.txt", &[("depth", "0")], body).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(std::fs::read(home.join("new.txt")).unwrap(), b"");
        let token = reply.headers["lock-token"].to_str().unwrap().to_string();
        assert!(token.starts_with("<opaquelocktoken:"));
        assert!(reply.body.contains(&token[1..token.len() - 1]));

        // 延長：同一個代碼
        let condition = format!("({token})");
        let reply = dav(
            &t,
            "LOCK",
            "/dav/new.txt",
            &[("if", &condition)],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.headers["lock-token"].to_str().unwrap(), token);

        // 鎖定後照樣可以寫入、解除
        assert_eq!(
            dav(&t, "PUT", "/dav/new.txt", &[("if", &condition)], "data")
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            dav(
                &t,
                "UNLOCK",
                "/dav/new.txt",
                &[("lock-token", &token)],
                Body::empty()
            )
            .await
            .status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            dav(&t, "LOCK", "/dav/missing/a.txt", &[], body)
                .await
                .status,
            StatusCode::CONFLICT
        );
    }

    #[sqlx::test]
    async fn proppatch_is_accepted_but_changes_nothing(pool: SqlitePool) {
        let t = setup(pool).await;
        std::fs::write(t.home("carol").join("a.txt"), b"x").unwrap();
        let body = r#"<?xml version="1.0" encoding="utf-8" ?>
            <D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop>
            <Z:Win32LastAccessTime>Mon, 23 Nov 2026 12:00:00 GMT</Z:Win32LastAccessTime>
            </D:prop></D:set></D:propertyupdate>"#;
        let reply = dav(&t, "PROPPATCH", "/dav/a.txt", &[], body).await;
        assert_eq!(reply.status, StatusCode::MULTI_STATUS);
        assert!(reply.body.contains("Win32LastAccessTime"));
        assert!(reply.body.contains("200 OK"));
    }

    #[sqlx::test]
    async fn administrators_cannot_change_other_peoples_folders(pool: SqlitePool) {
        let t = setup(pool).await;
        t.make_admin().await;
        t.other_user("jerry").await;
        std::fs::write(t.home("jerry").join("j.txt"), b"j").unwrap();
        t.home("carol");

        // 管理員的 My files 是整顆硬碟的最上層：看得到、下載得到別人的資料夾
        let reply = dav(
            &t,
            "PROPFIND",
            "/dav/users/",
            &[("depth", "1")],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::MULTI_STATUS);
        assert!(reply.body.contains("/dav/users/jerry/"));
        assert_eq!(
            dav(&t, "GET", "/dav/users/jerry/j.txt", &[], Body::empty())
                .await
                .body,
            "j"
        );

        // 但不能修改
        for (method, uri, headers) in [
            ("PUT", "/dav/users/jerry/x.txt", vec![]),
            ("DELETE", "/dav/users/jerry/j.txt", vec![]),
            ("DELETE", "/dav/users", vec![]),
            ("MKCOL", "/dav/users/jerry/new", vec![]),
            (
                "MOVE",
                "/dav/users/jerry/j.txt",
                vec![("destination", "/dav/j.txt")],
            ),
            (
                "COPY",
                "/dav/a.txt",
                vec![("destination", "/dav/users/jerry/a.txt")],
            ),
        ] {
            if method == "COPY" {
                std::fs::write(t.data_dir().join("a.txt"), b"a").unwrap();
            }
            let reply = dav(&t, method, uri, &headers, Body::empty()).await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert!(t.home("jerry").join("j.txt").exists());
        // 自己的資料夾可以
        assert_eq!(
            dav(&t, "PUT", "/dav/users/carol/x.txt", &[], "x")
                .await
                .status,
            StatusCode::CREATED
        );
    }

    #[sqlx::test]
    async fn browsers_on_other_sites_are_refused(pool: SqlitePool) {
        let t = setup(pool).await;
        t.home("carol");
        let reply = dav(
            &t,
            "PUT",
            "/dav/a.txt",
            &[("origin", "http://evil.example")],
            "x",
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert!(!t.home("carol").join("a.txt").exists());
    }
}
