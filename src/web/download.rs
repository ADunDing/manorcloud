//! 檔案下載，支援 HTTP Range（斷點續傳、影片拖曳），以及把多個項目打包成 .zip 的串流下載。
//!
//! 安全重點：上傳的內容完全不可信。絕不能讓瀏覽器在本站的 origin 上「顯示」它，
//! 否則一個上傳的 HTML 檔裡的腳本，就能以受害者的身分呼叫所有 API
//! （Cookie 會自動帶上，而且同源請求能通過 Origin 檢查）。
//! 防線見 `base_response` 中各 header 的說明；200、206、416 三種回應全部由它建構。
//! 打包下載的回應（application/zip）使用相同的防線。

use std::io::{self, SeekFrom};
use std::path::PathBuf;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, header, response::Builder},
    response::Response,
};
use futures_util::stream;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;

use super::{
    error::AppError,
    range::{self, RangeRequest},
};
use crate::storage::archive::ZipWriter;

/// 每次從磁碟讀取的大小。預設的 4 KiB 會讓大檔案產生大量系統呼叫；
/// 64 KiB 仍是固定的一小塊，記憶體用量不隨檔案大小成長。
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// 一次打包下載最多選幾個項目（資料夾裡面的內容不算在內）
pub(super) const MAX_ARCHIVE_ITEMS: usize = 1000;

/// 打包時，送往網路的資料最多排隊幾塊。讀磁碟比網路快時在這裡等待，記憶體用量固定
const ARCHIVE_QUEUE: usize = 8;

/// 回傳一個已經開啟的檔案：依 Range 與 If-Range 決定整個檔案、一段範圍，還是 416。
pub(super) async fn file_response(
    mut file: File,
    total: u64,
    name: &str,
    etag: &str,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    // 決定回傳整個檔案、一段範圍，還是 416。
    //    header 值不是合法 UTF-8 時視為沒帶（Range 語法錯誤本來就該忽略）。
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let if_range = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok());
    let decision = range::evaluate(range_header, if_range, etag, total);

    let response = match decision {
        RangeRequest::Full => base_response(name, etag)
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, total)
            .body(stream_body(file)),

        RangeRequest::Partial(r) => {
            // seek 直接移動檔案位置，不會讀過前面的資料；take 限制最多讀這一段的長度
            file.seek(SeekFrom::Start(r.start()))
                .await
                .map_err(AppError::internal)?;
            base_response(name, etag)
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, r.content_range(total))
                .header(header::CONTENT_LENGTH, r.byte_count())
                .body(stream_body(file.take(r.byte_count())))
        }

        RangeRequest::Unsatisfiable => base_response(name, etag)
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
        // 宣告成不明的二進位資料，就算 attachment 因故被忽略，瀏覽器也沒有理由把它當成 HTML 顯示。
        // 唯一的例外是影音：預覽時 <video>、<audio> 直接播放這個網址，Safari 需要正確的類型。
        // 影音的類型不會被當成網頁或腳本執行
        .header(header::CONTENT_TYPE, media_type(name))
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

/// 影音檔依副檔名回傳類型，其他一律是 application/octet-stream
fn media_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "aac") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        _ => "application/octet-stream",
    }
}

/// 把非同步讀取器包成串流回應本體：一次只讀一小塊
fn stream_body<R>(reader: R) -> Body
where
    R: AsyncRead + Send + 'static,
{
    Body::from_stream(ReaderStream::with_capacity(reader, STREAM_CHUNK_BYTES))
}

// ==================== 打包下載 ====================

/// 打包下載中的一個項目
pub(super) struct ArchiveItem {
    /// zip 中的路徑
    pub path: String,
    /// 修改時間（Unix 秒）
    pub mtime: i64,
    /// 檔案內容在磁碟上的位置；資料夾是 None
    pub source: Option<PathBuf>,
}

/// 把項目邊讀邊打包成 .zip 送出。
/// `roots`：選取的項目的名稱，決定下載的檔名
pub(super) fn archive_response(
    items: Vec<ArchiveItem>,
    roots: &[String],
) -> Result<Response, AppError> {
    let (sender, receiver) = mpsc::channel(ARCHIVE_QUEUE);
    tokio::spawn(write_archive(items, sender));
    let body = Body::from_stream(stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    }));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(
            header::CONTENT_DISPOSITION,
            content_disposition(&archive_name(roots)),
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(body)
        .map_err(AppError::internal)
}

/// 下載的檔名：只選一個項目時用它的名稱，否則是 ManorCloud-日期.zip
fn archive_name(roots: &[String]) -> String {
    match roots {
        [single] => format!("{single}.zip"),
        _ => format!("ManorCloud-{}.zip", time::OffsetDateTime::now_utc().date()),
    }
}

/// 在背景依序讀取每個檔案、產生 zip 並送出。
///
/// 中途失敗時送出一個錯誤：連線以失敗結束，瀏覽器顯示「下載失敗」，
/// 而不是留下一個看似完成、其實缺了內容的 zip。
/// 使用者取消下載時，接收端被丟棄，送出失敗，這裡就停止讀取磁碟。
async fn write_archive(items: Vec<ArchiveItem>, sender: mpsc::Sender<io::Result<Bytes>>) {
    if let Err(e) = stream_entries(&items, &sender).await
        && !sender.is_closed()
    {
        eprintln!("archive download failed: {e}");
        let _ = sender.send(Err(e)).await;
    }
}

async fn stream_entries(
    items: &[ArchiveItem],
    sender: &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let mut zip = ZipWriter::new();
    let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];

    for item in items {
        let Some(source) = &item.source else {
            let header = zip
                .directory(&item.path, item.mtime)
                .map_err(io::Error::other)?;
            send(sender, header).await?;
            continue;
        };

        // 不跟隨符號連結：走訪與開檔之間被換成連結，也不會讀到空間以外的檔案
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)
            .await?;
        let size = file.metadata().await?.len();

        let header = zip
            .start_file(&item.path, item.mtime, size)
            .map_err(io::Error::other)?;
        send(sender, header).await?;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            zip.update(&buffer[..read]);
            send(sender, buffer[..read].to_vec()).await?;
        }
        let descriptor = zip.finish_file().map_err(io::Error::other)?;
        send(sender, descriptor).await?;
    }

    let tail = zip.finish().map_err(io::Error::other)?;
    send(sender, tail).await
}

async fn send(sender: &mpsc::Sender<io::Result<Bytes>>, bytes: Vec<u8>) -> io::Result<()> {
    sender
        .send(Ok(Bytes::from(bytes)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the download was cancelled"))
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
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn only_audio_and_video_get_their_own_type() {
        assert_eq!(media_type("影片.MP4"), "video/mp4");
        assert_eq!(media_type("a.mp3"), "audio/mpeg");
        for name in ["a.html", "a.svg", "a.pdf", "mp4", "a.js", ""] {
            assert_eq!(media_type(name), "application/octet-stream", "{name}");
        }
    }

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
}
