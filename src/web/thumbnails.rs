//! 圖片的縮圖與預覽圖（產生與快取在 thumbs.rs）：
//!
//!   GET /api/files/thumbnail?path=&size=small|large&v=
//!
//! `v` 由前端帶上檔案的大小與修改時間：檔案改變時網址就不同，所以回應可以讓瀏覽器長期快取。

use axum::{Router, body::Body, extract::Query, http::header, response::Response, routing::get};
use serde::Deserialize;
use tokio::sync::Semaphore;

use super::{
    error::AppError,
    extract::InSpace,
    spaces::{io_error, parse_path},
    state::AppState,
};
use crate::thumbs::{self, Size};

/// 同時產生的數量上限；用到快取的請求不受限制
static SLOTS: Semaphore = Semaphore::const_new(thumbs::MAX_CONCURRENT);

pub fn router() -> Router<AppState> {
    Router::new().route("/api/files/thumbnail", get(thumbnail))
}

#[derive(Deserialize)]
struct ThumbnailQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: Option<String>,
}

async fn thumbnail(
    InSpace { space, .. }: InSpace,
    Query(query): Query<ThumbnailQuery>,
) -> Result<Response, AppError> {
    let rel = space.abs(&parse_path(&query.path)?);
    let size = match query.size.as_deref() {
        None => Size::Small,
        Some(text) => Size::parse(text)
            .ok_or_else(|| AppError::BadRequest("size must be small or large".to_string()))?,
    };
    let root = space.root.clone();
    let found = run(move || thumbs::cached(&root, &rel, size)).await?;
    let bytes = match found {
        Ok(bytes) => bytes,
        Err(source) => {
            let _slot = SLOTS.acquire().await.map_err(AppError::internal)?;
            let root = space.root.clone();
            run(move || thumbs::generate(&root, source, size)).await?
        }
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        )
        .header(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        )
        .body(Body::from(bytes))
        .map_err(AppError::internal)
}

async fn run<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            Err(AppError::NotFound("This file has no preview".to_string()))
        }
        Ok(result) => result.map_err(io_error),
        Err(e) => Err(AppError::internal(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, empty_request};
    use axum::http::{Method, StatusCode};
    use sqlx::SqlitePool;

    #[sqlx::test]
    async fn pictures_get_a_thumbnail_and_other_files_do_not(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir_all(&home).unwrap();
        image::RgbImage::new(600, 300)
            .save(home.join("a.png"))
            .unwrap();
        std::fs::write(home.join("b.txt"), b"hello").unwrap();

        let get = |uri: &str| t.send(empty_request(Method::GET, uri, Some(&t.cookie)));
        let (status, headers, body) = get("/api/files/thumbnail?path=a.png&size=large&v=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/jpeg");
        assert_eq!(image::load_from_memory(&body).unwrap().width(), 600);

        for (uri, status) in [
            ("/api/files/thumbnail?path=b.txt", StatusCode::NOT_FOUND),
            ("/api/files/thumbnail?path=nope.png", StatusCode::NOT_FOUND),
            (
                "/api/files/thumbnail?path=a.png&size=huge",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/files/thumbnail?path=../a.png",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            assert_eq!(get(uri).await.0, status, "{uri}");
        }
    }
}
