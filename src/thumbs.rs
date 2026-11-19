//! 圖片的縮圖與預覽圖：伺服器讀取原始圖片，縮小後重新壓縮成 JPEG。
//!
//! - 小圖（Small）給檔案列表用，大圖（Large）給點開時的預覽用。原圖只能用下載取得
//! - 回傳的一律是伺服器產生的 JPEG，不會把上傳的內容原封不動顯示在網頁上（見 web/download.rs 的說明）
//! - 依照相片的 EXIF 方向轉正；透明的部分填成白色
//! - 產生過的存在硬碟的 `.manorcloud/thumbs/`，以「路徑、大小、修改時間、尺寸」為鍵。
//!   檔案改變或改名後鍵就不同，舊的放著由垃圾回收在一段時間沒有用到之後刪除
//! - 解碼很耗記憶體與 CPU：限制原始檔案大小、解碼時的記憶體，同時最多產生兩張

use std::fs::File;
use std::io::{self, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use image::{
    DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, codecs::jpeg::JpegEncoder,
};

use crate::{
    storage,
    tree::{self, RelPath},
};

/// 同時最多產生幾張（其他的排隊）。樹莓派等級的機器記憶體不多
pub const MAX_CONCURRENT: usize = 2;

/// 比這個大的檔案不產生縮圖
const MAX_SOURCE_BYTES: u64 = 200 * 1024 * 1024;

/// 解碼時最多使用的記憶體。約等於 7,000 萬像素的 RGBA 圖片
const MAX_DECODE_BYTES: u64 = 300 * 1024 * 1024;

/// 用到快取時，修改時間超過這麼久才更新（垃圾回收依修改時間判斷是否還有在用）
const TOUCH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// 支援的格式
const FORMATS: [ImageFormat; 5] = [
    ImageFormat::Jpeg,
    ImageFormat::Png,
    ImageFormat::Gif,
    ImageFormat::WebP,
    ImageFormat::Bmp,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// 檔案列表
    Small,
    /// 預覽
    Large,
}

impl Size {
    pub fn parse(text: &str) -> Option<Size> {
        match text {
            "small" => Some(Size::Small),
            "large" => Some(Size::Large),
            _ => None,
        }
    }

    /// 長邊最多幾個像素
    fn pixels(self) -> u32 {
        match self {
            Size::Small => 320,
            Size::Large => 2048,
        }
    }

    fn quality(self) -> u8 {
        match self {
            Size::Small => 75,
            Size::Large => 85,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Size::Small => "small",
            Size::Large => "large",
        }
    }
}

/// 快取：`{硬碟}/.manorcloud/thumbs`
pub fn cache_dir(root: &Path) -> PathBuf {
    storage::system_dir(root).join("thumbs")
}

/// 找到的原始圖片：快取的位置，以及已經開啟的檔案（產生時用）
pub struct Source {
    file: File,
    size: u64,
    cached: PathBuf,
}

/// 開啟原始圖片並算出快取的位置。快取已經有了就直接回傳內容
pub fn cached(root: &Path, rel: &RelPath, size: Size) -> io::Result<Result<Vec<u8>, Source>> {
    let (file, meta) = tree::open_file(root, rel)?;
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(unsupported());
    }
    let key = cache_key(&tree::resolve(root, rel, true)?, &meta, size);
    let cached = cache_dir(root).join(&key[..2]).join(format!("{key}.jpg"));
    match std::fs::read(&cached) {
        Ok(bytes) => {
            touch(&cached);
            Ok(Ok(bytes))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Err(Source {
            file,
            size: meta.len(),
            cached,
        })),
        Err(e) => Err(e),
    }
}

/// 產生縮圖並存進快取。存不進去（例如硬碟滿了）不影響回傳
pub fn generate(root: &Path, source: Source, size: Size) -> io::Result<Vec<u8>> {
    let bytes = render(source.file, source.size, size)?;
    if let Err(e) = save(root, &source.cached, &bytes) {
        eprintln!("thumbnail: cannot save {}: {e}", source.cached.display());
    }
    Ok(bytes)
}

/// 不是支援的圖片，或太大
fn unsupported() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "no preview for this file")
}

fn cache_key(path: &Path, meta: &std::fs::Metadata, size: Size) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "{}:{}:{}:{}:{}",
            path.display(),
            meta.len(),
            meta.mtime(),
            meta.mtime_nsec(),
            size.label()
        )
        .as_bytes(),
    );
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn render(file: File, file_size: u64, size: Size) -> io::Result<Vec<u8>> {
    let bad = |e: image::ImageError| match e {
        image::ImageError::IoError(e) => e,
        _ => unsupported(),
    };
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| unsupported())?;
    if !reader.format().is_some_and(|f| FORMATS.contains(&f)) {
        return Err(unsupported());
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES.max(file_size));
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(bad)?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(bad)?;
    image.apply_orientation(orientation);

    let max = size.pixels();
    if image.width() > max || image.height() > max {
        image = image.thumbnail(max, max);
    }
    let rgb = on_white(&image);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, size.quality())
        .encode_image(&rgb)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(out)
}

/// 去掉透明度：透明的部分填成白色
fn on_white(image: &DynamicImage) -> image::RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8;
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

/// 先寫到 tmp/ 再改名，其他請求不會讀到寫到一半的檔案
fn save(root: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp_dir = storage::tmp_dir(root);
    std::fs::create_dir_all(&tmp_dir)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_dir.join(format!("thumb-{}", storage::generate_storage_key()));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 更新修改時間，讓垃圾回收知道它還在使用
fn touch(path: &Path) {
    let now = SystemTime::now();
    let old = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|m| now.duration_since(m).is_ok_and(|age| age > TOUCH_AFTER));
    if old && let Ok(file) = File::options().write(true).open(path) {
        let _ = file.set_modified(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(path: &Path, w: u32, h: u32) {
        let image = image::RgbaImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        image.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn get(root: &Path, rel: &str, size: Size) -> io::Result<(Vec<u8>, bool)> {
        let rel = RelPath::parse(rel).unwrap();
        match cached(root, &rel, size)? {
            Ok(bytes) => Ok((bytes, true)),
            Err(source) => Ok((generate(root, source, size)?, false)),
        }
    }

    #[test]
    fn images_are_shrunk_cached_and_flattened() {
        let dir = tempfile::tempdir().unwrap();
        png(&dir.path().join("a.png"), 1000, 500);

        let (bytes, hit) = get(dir.path(), "a.png", Size::Small).unwrap();
        assert!(!hit);
        let thumb = image::load_from_memory(&bytes).unwrap().to_rgb8();
        assert_eq!((thumb.width(), thumb.height()), (320, 160));
        // 透明的右半邊是白色
        let right = thumb.get_pixel(300, 80).0;
        assert!(right.iter().all(|&c| c > 240), "{right:?}");

        let (again, hit) = get(dir.path(), "a.png", Size::Small).unwrap();
        assert!(hit);
        assert_eq!(again, bytes);

        // 小圖不放大
        png(&dir.path().join("b.png"), 40, 20);
        let (bytes, _) = get(dir.path(), "b.png", Size::Large).unwrap();
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 40);
    }

    #[test]
    fn other_files_have_no_preview() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"not really a picture").unwrap();
        std::fs::write(
            dir.path().join("b.svg"),
            b"<svg xmlns='http://www.w3.org/2000/svg'/>",
        )
        .unwrap();
        for name in ["a.jpg", "b.svg"] {
            let e = get(dir.path(), name, Size::Small).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{name}");
        }
        assert_eq!(
            get(dir.path(), "nope.png", Size::Small).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
