//! 檔案下載，支援 HTTP Range（斷點續傳、影片拖曳）。
//!
//! 安全重點：上傳的內容完全不可信。絕不能讓瀏覽器在本站的 origin 上「顯示」它，
//! 否則一個上傳的 HTML 檔裡的腳本，就能以受害者的身分呼叫所有 API
//! （Cookie 會自動帶上，而且同源請求能通過 Origin 檢查）。
//! 防線見 `base_response` 中各 header 的說明；200、206、416 三種回應全部由它建構。

use std::io::{self, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header, response::Builder},
    response::Response,
    routing::get,
};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use super::{
    error::AppError,
    extract::CurrentUser,
    range::{self, RangeRequest},
    state::AppState,
};
use crate::{db::files as files_db, storage};

/// 「不存在」「屬於別人」「是資料夾」對外一律使用這個訊息
const FILE_NOT_FOUND: &str = "找不到檔案";

/// 每次從磁碟讀取的大小。預設的 4 KiB 會讓大檔案產生大量系統呼叫；
/// 64 KiB 仍是固定的一小塊，記憶體用量不隨檔案大小成長。
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/files/{id}/content", get(download))
}

/// GET /api/files/{id}/content：下載檔案內容，支援 Range 與 If-Range。
async fn download(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    // 1. 只查得到自己的項目；資料夾沒有內容可下載。三種情況都是同一個 404。
    let entry = files_db::find_file(&pool, user.id, id)
        .await?
        .filter(|e| e.kind == "file")
        .ok_or_else(file_not_found)?;
    let file_id = entry.id;

    // 2. 找到 blob。CHECK 約束保證檔案一定有合法的 storage_key，
    //    走到錯誤分支代表資料庫被竄改或程式有 bug，屬於伺服器問題。
    let key = entry
        .storage_key
        .as_deref()
        .ok_or_else(|| AppError::internal(format!("檔案 {file_id} 沒有 storage_key")))?;
    let path = storage::blob_path(&data_dir, key)
        .ok_or_else(|| AppError::internal(format!("檔案 {file_id} 的 storage_key 格式不正確")))?;

    // 3. 開檔。資料庫有紀錄、磁碟卻沒有檔案是「資料不一致」：
    //    伺服器的資料損壞了，不是使用者的錯，回 500 並記錄，而不是 404。
    let mut file = File::open(&path).await.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            AppError::internal(format!(
                "資料不一致：檔案 {file_id} 的 blob 不存在（{}）",
                path.display()
            ))
        } else {
            AppError::internal(format!("無法開啟檔案 {file_id} 的 blob：{e}"))
        }
    })?;

    // 以磁碟上的實際大小計算範圍與 Content-Length，確保與實際送出的位元組數一致
    let total = file.metadata().await.map_err(AppError::internal)?.len();
    let etag = etag_for(key);

    // 4. 決定回傳整個檔案、一段範圍，還是 416。
    //    header 值不是合法 UTF-8 時視為沒帶（Range 語法錯誤本來就該忽略）。
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let if_range = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok());
    let decision = range::evaluate(range_header, if_range, &etag, total);

    let response = match decision {
        RangeRequest::Full => base_response(&entry.name, &etag)
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, total)
            .body(stream_body(file)),

        RangeRequest::Partial(r) => {
            // seek 直接移動檔案位置，不會讀過前面的資料；take 限制最多讀這一段的長度
            file.seek(SeekFrom::Start(r.start()))
                .await
                .map_err(AppError::internal)?;
            base_response(&entry.name, &etag)
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, r.content_range(total))
                .header(header::CONTENT_LENGTH, r.byte_count())
                .body(stream_body(file.take(r.byte_count())))
        }

        RangeRequest::Unsatisfiable => base_response(&entry.name, &etag)
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(
                header::CONTENT_RANGE,
                range::unsatisfied_content_range(total),
            )
            .body(Body::empty()),
    };

    response.map_err(AppError::internal)
}

/// 所有下載回應（200、206、416）共用的 header。
///
/// 安全 header 集中在這裡，各分支只補上自己特有的 header，
/// 新增回應類型時不可能漏掉任何一個。
fn base_response(name: &str, etag: &str) -> Builder {
    Response::builder()
        // 這個端點的目的就是下載，一律宣告成不明的二進位資料，不使用資料庫裡的 MIME。
        // 就算 attachment 因故被忽略，瀏覽器也沒有理由把它當成 HTML 顯示。
        .header(header::CONTENT_TYPE, "application/octet-stream")
        // 主要防線：讓瀏覽器存檔，而不是在本站 origin 開啟
        .header(header::CONTENT_DISPOSITION, content_disposition(name))
        // 禁止瀏覽器無視 Content-Type、自行猜測內容類型
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        // 最後一道：就算被顯示，也處在獨立的不透明 origin 且不能執行腳本
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        // 隱私：不讓反向代理等共用快取保存別人的檔案，也不在瀏覽器磁碟留副本
        .header(header::CACHE_CONTROL, "private, no-store")
        // 告訴客戶端這個端點支援 Range，可以斷點續傳
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
}

/// 把非同步讀取器包成串流回應本體：一次只讀一小塊
fn stream_body<R>(reader: R) -> Body
where
    R: AsyncRead + Send + 'static,
{
    Body::from_stream(ReaderStream::with_capacity(reader, STREAM_CHUNK_BYTES))
}

/// 由 storage_key 衍生 ETag：`"<SHA-256 前 16 位元組的 hex>"`。
///
/// 為什麼從 storage_key 衍生，而不是從 id：
/// - 同一筆紀錄的內容永遠不變（storage_key 寫入後不再修改；改名、搬移不影響它）。
/// - 但「同一個 id 數字」不保證指向同一筆紀錄：files.id 沒有 AUTOINCREMENT，
///   刪除 id 最大的項目後，下一個新項目會重新使用同一個 id。
///   若 ETag 來自 id，斷點續傳就可能把舊檔的前半與新檔的後半拼在一起。
///   storage_key 是每份內容獨有的隨機值，id 被重用時 ETag 必然不同。
///
/// 為什麼不直接用 storage_key：它是內部資訊，一路以來不讓它出現在任何回應裡。
/// 128 位元的隨機值經過雜湊後無法反推。`manorcloud-etag:` 前綴是領域分隔，
/// 確保這個雜湊只在 ETag 這個用途中有意義。
///
/// 是強 ETag（沒有 `W/`）：內容逐位元組不變，而 If-Range 規定只能用強比對。
fn etag_for(storage_key: &str) -> String {
    let digest = Sha256::digest(format!("manorcloud-etag:{storage_key}").as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

fn file_not_found() -> AppError {
    AppError::NotFound(FILE_NOT_FOUND.to_string())
}

// ==================== Content-Disposition ====================

/// 產生下載用的 Content-Disposition header 值（RFC 6266 + RFC 5987）：
///
/// `attachment; filename="<ASCII 備案>"; filename*=UTF-8''<百分比編碼>`
///
/// - `filename*` 是正式的值，現代瀏覽器優先使用。不屬於 RFC 5987 attr-char 的位元組
///   全部編碼成 %XX，所以中文、引號、分號、空白都不可能破壞 header 格式。
/// - `filename` 是給舊客戶端的備案：可見 ASCII 保留，其餘換成 `_`。
///   另外 `"`（會提早結束引號）、`\`（引號內的跳脫字元）、
///   `%`（RFC 6266 提到部分客戶端會對此參數做百分比解碼）也換成 `_`。
///
/// 回傳值保證是可見 ASCII，可以安全地放進 HTTP header。
pub fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| if is_fallback_safe(c) { c } else { '_' })
        .collect();

    let mut encoded = String::with_capacity(name.len() * 3);
    for b in name.bytes() {
        if is_attr_char(b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }

    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// ASCII 備案中可以保留的字元：可見 ASCII 與空白，但排除 `"`、`\`、`%`
fn is_fallback_safe(c: char) -> bool {
    matches!(c, ' '..='~') && !matches!(c, '"' | '\\' | '%')
}

/// RFC 5987 的 attr-char：可以不經編碼直接出現在 filename* 中的位元組
fn is_attr_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, request};
    use super::*;
    use axum::{
        body::Bytes,
        http::{HeaderValue, Method},
    };

    // ==================== content_disposition ====================

    /// 測試用：把 filename* 的百分比編碼還原成字串，用來驗證往返一致
    fn decode_filename_star(header: &str) -> String {
        let encoded = header.split("filename*=UTF-8''").nth(1).unwrap();
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_ascii() {
        assert_eq!(
            content_disposition("report.pdf"),
            "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf"
        );
    }

    #[test]
    fn chinese() {
        // 相 = E7 9B B8，片 = E7 89 87；每個中文字在備案中變成一個 _
        assert_eq!(
            content_disposition("相片.jpg"),
            "attachment; filename=\"__.jpg\"; filename*=UTF-8''%E7%9B%B8%E7%89%87.jpg"
        );
    }

    #[test]
    fn double_quote() {
        assert_eq!(
            content_disposition("a\"b.txt"),
            "attachment; filename=\"a_b.txt\"; filename*=UTF-8''a%22b.txt"
        );
    }

    #[test]
    fn semicolon() {
        // 分號在引號內合法，備案中保留；filename* 中編碼
        assert_eq!(
            content_disposition("a;b.txt"),
            "attachment; filename=\"a;b.txt\"; filename*=UTF-8''a%3Bb.txt"
        );
    }

    #[test]
    fn space() {
        assert_eq!(
            content_disposition("my file.txt"),
            "attachment; filename=\"my file.txt\"; filename*=UTF-8''my%20file.txt"
        );
    }

    #[test]
    fn percent() {
        assert_eq!(
            content_disposition("100%.txt"),
            "attachment; filename=\"100_.txt\"; filename*=UTF-8''100%25.txt"
        );
    }

    #[test]
    fn backslash() {
        // 檔名驗證不允許反斜線，但這個函式不應依賴呼叫端的驗證
        assert_eq!(
            content_disposition("a\\b"),
            "attachment; filename=\"a_b\"; filename*=UTF-8''a%5Cb"
        );
    }

    #[test]
    fn output_is_always_a_valid_ascii_header() {
        for name in [
            "report.pdf",
            "相片.jpg",
            "a\"b.txt",
            "a;b.txt",
            "my file.txt",
            "100%.txt",
            "會議 10:30.docx",
            "👨\u{200D}👩\u{200D}👧.jpg",
            "attachment; filename=\"evil.html\"",
        ] {
            let value = content_disposition(name);

            assert!(value.is_ascii(), "{name}");
            assert!(!value.chars().any(|c| c.is_ascii_control()), "{name}");
            assert!(HeaderValue::from_str(&value).is_ok(), "{name}");
            // filename* 解碼後必須與原名完全相同
            assert_eq!(decode_filename_star(&value), name);
        }
    }

    // ==================== etag_for ====================

    #[test]
    fn etag_is_quoted_hex_and_deterministic() {
        let key = "0123456789abcdef0123456789abcdef";
        let etag = etag_for(key);

        // 引號 + 32 個 hex 字元 + 引號
        assert_eq!(etag.len(), 34);
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        assert!(etag[1..33].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(etag, etag_for(key));
    }

    #[test]
    fn etag_differs_per_key_and_hides_it() {
        let a = "0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210";

        assert_ne!(etag_for(a), etag_for(b));
        assert!(!etag_for(a).contains(a));
    }

    // ==================== 下載 handler ====================

    /// 測試內容：1000 個位元組，每個位置的值都不同（以 251 為週期），方便確認切片位置正確
    fn sample() -> Vec<u8> {
        (0..1000u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect()
    }

    fn content_uri(id: i64) -> String {
        format!("/api/files/{id}/content")
    }

    async fn get(
        t: &TestApp,
        id: i64,
        cookie: Option<&str>,
        extra: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Bytes) {
        let mut builder = request(Method::GET, &content_uri(id), cookie);
        for (name, value) in extra {
            builder = builder.header(*name, *value);
        }
        t.send(builder.body(Body::empty()).unwrap()).await
    }

    fn header_str(headers: &HeaderMap, name: header::HeaderName) -> &str {
        headers.get(name).unwrap().to_str().unwrap()
    }

    /// 每一種下載回應都必須帶著的 header
    fn assert_security_headers(headers: &HeaderMap) {
        assert_eq!(
            header_str(headers, header::CONTENT_TYPE),
            "application/octet-stream"
        );
        assert!(header_str(headers, header::CONTENT_DISPOSITION).starts_with("attachment;"));
        assert_eq!(
            header_str(headers, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert!(header_str(headers, header::CONTENT_SECURITY_POLICY).contains("sandbox"));
        assert!(header_str(headers, header::CACHE_CONTROL).contains("private"));
        assert_eq!(header_str(headers, header::ACCEPT_RANGES), "bytes");
        assert!(headers.contains_key(header::ETAG));
    }

    #[sqlx::test]
    async fn full_download(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let content = sample();
        let (id, key) = t.store_file(t.user_id, None, "data.bin", &content).await;

        let (status, headers, body) = get(&t, id, Some(&t.cookie), &[]).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], &content[..]);
        assert_eq!(header_str(&headers, header::CONTENT_LENGTH), "1000");
        assert_security_headers(&headers);
        assert!(!header_str(&headers, header::ETAG).contains(&key));
    }

    #[sqlx::test]
    async fn closed_range(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let content = sample();
        let (id, _) = t.store_file(t.user_id, None, "data.bin", &content).await;

        let (status, headers, body) =
            get(&t, id, Some(&t.cookie), &[("range", "bytes=0-99")]).await;

        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(&body[..], &content[0..100]);
        assert_eq!(
            header_str(&headers, header::CONTENT_RANGE),
            "bytes 0-99/1000"
        );
        assert_eq!(header_str(&headers, header::CONTENT_LENGTH), "100");
    }

    #[sqlx::test]
    async fn suffix_and_open_ranges(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let content = sample();
        let (id, _) = t.store_file(t.user_id, None, "data.bin", &content).await;

        let (_, headers, body) = get(&t, id, Some(&t.cookie), &[("range", "bytes=-100")]).await;
        assert_eq!(&body[..], &content[900..]);
        assert_eq!(
            header_str(&headers, header::CONTENT_RANGE),
            "bytes 900-999/1000"
        );

        let (_, headers, body) = get(&t, id, Some(&t.cookie), &[("range", "bytes=100-")]).await;
        assert_eq!(&body[..], &content[100..]);
        assert_eq!(
            header_str(&headers, header::CONTENT_RANGE),
            "bytes 100-999/1000"
        );

        // 從中間開始的一段：確認 seek 與 take 的位置都正確
        let (_, _, body) = get(&t, id, Some(&t.cookie), &[("range", "bytes=500-509")]).await;
        assert_eq!(&body[..], &content[500..510]);
    }

    #[sqlx::test]
    async fn unsatisfiable_range(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "data.bin", &sample()).await;

        let (status, headers, body) =
            get(&t, id, Some(&t.cookie), &[("range", "bytes=99999999-")]).await;

        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header_str(&headers, header::CONTENT_RANGE), "bytes */1000");
        assert!(body.is_empty());
    }

    #[sqlx::test]
    async fn garbage_range_returns_full_file(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let content = sample();
        let (id, _) = t.store_file(t.user_id, None, "data.bin", &content).await;

        let (status, _, body) = get(&t, id, Some(&t.cookie), &[("range", "garbage")]).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], &content[..]);
    }

    #[sqlx::test]
    async fn security_headers_on_every_status(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "hack.html", &sample()).await;

        for (range, expected) in [
            (None, StatusCode::OK),
            (Some("bytes=0-9"), StatusCode::PARTIAL_CONTENT),
            (Some("bytes=99999-"), StatusCode::RANGE_NOT_SATISFIABLE),
        ] {
            let extra: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
            let (status, headers, _) = get(&t, id, Some(&t.cookie), &extra).await;

            assert_eq!(status, expected);
            assert_security_headers(&headers);
        }
    }

    #[sqlx::test]
    async fn if_range_controls_partial_response(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let content = sample();
        let (id, _) = t.store_file(t.user_id, None, "data.bin", &content).await;
        let (_, headers, _) = get(&t, id, Some(&t.cookie), &[]).await;
        let etag = header_str(&headers, header::ETAG).to_string();

        // 相符 → 206
        let (status, _, body) = get(
            &t,
            id,
            Some(&t.cookie),
            &[("range", "bytes=0-9"), ("if-range", &etag)],
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(&body[..], &content[0..10]);

        // 不相符、弱 ETag、日期 → 200 整個檔案
        let weak = format!("W/{etag}");
        for if_range in [
            "\"ffffffffffffffffffffffffffffffff\"",
            weak.as_str(),
            "Wed, 21 Oct 2026 07:28:00 GMT",
        ] {
            let (status, _, body) = get(
                &t,
                id,
                Some(&t.cookie),
                &[("range", "bytes=0-9"), ("if-range", if_range)],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{if_range}");
            assert_eq!(body.len(), 1000, "{if_range}");
        }
    }

    #[sqlx::test]
    async fn etag_survives_rename(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.bin", &sample()).await;
        let (_, before, _) = get(&t, id, Some(&t.cookie), &[]).await;

        files_db::update_item(&t.pool, t.user_id, id, Some("renamed.bin"), None)
            .await
            .unwrap();
        let (_, after, _) = get(&t, id, Some(&t.cookie), &[]).await;

        // 內容沒變，續傳不該被迫重新下載整個檔案
        assert_eq!(before.get(header::ETAG), after.get(header::ETAG));
    }

    /// id 被重新使用時，舊的 ETag 必須讓續傳失效，否則會把兩個檔案拼在一起
    #[sqlx::test]
    async fn reused_id_gets_a_different_etag(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (old_id, _) = t
            .store_file(t.user_id, None, "old.bin", b"old content")
            .await;
        let (_, headers, _) = get(&t, old_id, Some(&t.cookie), &[]).await;
        let old_etag = header_str(&headers, header::ETAG).to_string();

        files_db::delete_tree(&t.pool, t.user_id, old_id)
            .await
            .unwrap();
        let (new_id, _) = t
            .store_file(t.user_id, None, "new.bin", b"NEW CONTENT")
            .await;

        // SQLite 沒有 AUTOINCREMENT 時，刪掉最大的 id 後會重新使用它
        assert_eq!(new_id, old_id);

        // 用舊的 ETag 從第 4 個位元組續傳：必須拿到整個新檔案，而不是新檔案的後半段
        let (status, _, body) = get(
            &t,
            new_id,
            Some(&t.cookie),
            &[("range", "bytes=4-"), ("if-range", &old_etag)],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&body[..], b"NEW CONTENT");
    }

    #[sqlx::test]
    async fn empty_file_ignores_range(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "empty.bin", b"").await;

        let (status, headers, body) = get(&t, id, Some(&t.cookie), &[("range", "bytes=0-")]).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
        assert_eq!(header_str(&headers, header::CONTENT_LENGTH), "0");
    }

    #[sqlx::test]
    async fn folder_foreign_and_missing_are_indistinguishable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let folder = t.make_folder(t.user_id, None, "docs").await;
        let alice = t.other_user("alice").await;
        let (foreign, _) = t.store_file(alice, None, "secret.txt", b"secret").await;

        let folder_res = get(&t, folder, Some(&t.cookie), &[]).await;
        let foreign_res = get(&t, foreign, Some(&t.cookie), &[]).await;
        let missing_res = get(&t, 999_999, Some(&t.cookie), &[]).await;

        assert_eq!(missing_res.0, StatusCode::NOT_FOUND);
        assert_eq!(
            (folder_res.0, &folder_res.2),
            (missing_res.0, &missing_res.2)
        );
        assert_eq!(
            (foreign_res.0, &foreign_res.2),
            (missing_res.0, &missing_res.2)
        );
    }

    #[sqlx::test]
    async fn missing_blob_is_a_server_error(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, key) = t.store_file(t.user_id, None, "a.txt", b"x").await;
        // 模擬資料損壞：資料庫有紀錄，磁碟上的 blob 卻被刪掉了
        std::fs::remove_file(storage::blob_path(t.dir.path(), &key).unwrap()).unwrap();

        let (status, _, body) = get(&t, id, Some(&t.cookie), &[]).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // 對外只有通用訊息，不洩漏路徑
        let body = std::str::from_utf8(&body).unwrap();
        assert!(!body.contains("blobs"), "{body}");
    }

    #[sqlx::test]
    async fn unauthenticated_download_returns_401(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (id, _) = t.store_file(t.user_id, None, "a.txt", b"x").await;

        let (status, _, _) = get(&t, id, None, &[]).await;

        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
