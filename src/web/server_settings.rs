//! 管理員修改整個伺服器的設定：
//!
//!   GET /api/admin/settings    目前的設定
//!   PUT /api/admin/settings    修改：{"max_upload_bytes": 位元組數，或 null 使用設定檔的值}
//!
//! 修改立即生效，不需要重新啟動。

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::AdminUser,
    state::{AppState, UploadLimit},
};
use crate::db::settings;

/// 上傳上限最小可以設定成多少：太小的值多半是單位輸入錯了
const MIN_UPLOAD_BYTES: u64 = 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/admin/settings", get(show).put(update))
}

#[derive(Serialize)]
struct SettingsResponse {
    /// 目前的上傳上限
    max_upload_bytes: u64,
    /// 管理頁面沒有設定時使用的值（設定檔的 MANORCLOUD_MAX_UPLOAD_BYTES，預設 10 GiB）
    default_max_upload_bytes: u64,
    /// 是否在管理頁面另外設定了
    custom_max_upload: bool,
}

async fn show(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(limit): State<UploadLimit>,
) -> Result<Json<SettingsResponse>, AppError> {
    let custom = settings::max_upload_bytes(&pool).await?;
    Ok(Json(SettingsResponse {
        max_upload_bytes: limit.get(),
        default_max_upload_bytes: limit.config(),
        custom_max_upload: custom.is_some(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateRequest {
    max_upload_bytes: Option<u64>,
}

async fn update(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(limit): State<UploadLimit>,
    Json(req): Json<UpdateRequest>,
) -> Result<StatusCode, AppError> {
    if req
        .max_upload_bytes
        .is_some_and(|bytes| bytes < MIN_UPLOAD_BYTES)
    {
        return Err(AppError::BadRequest(
            "The upload limit must be at least 1 MB".to_string(),
        ));
    }
    settings::set_max_upload_bytes(&pool, req.max_upload_bytes).await?;
    limit.set(req.max_upload_bytes);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use axum::http::{Method, StatusCode};
    use serde_json::json;
    use sqlx::SqlitePool;

    #[sqlx::test]
    async fn administrators_change_the_upload_limit_without_restarting(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (status, _) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        t.make_admin().await;

        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(body["max_upload_bytes"], MIB);
        assert_eq!(body["custom_max_upload"], false);

        // 太小的值拒絕
        let (status, _) = t
            .json(
                Method::PUT,
                "/api/admin/settings",
                Some(json!({"max_upload_bytes": 10})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = t
            .json(
                Method::PUT,
                "/api/admin/settings",
                Some(json!({"max_upload_bytes": 2 * MIB})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = t.json(Method::GET, "/api/config", None).await;
        assert_eq!(body["max_upload_bytes"], 2 * MIB);
        // 原本 1 MiB 的上限擋掉的大小，現在可以了
        let (status, body) = t
            .json(
                Method::POST,
                "/api/uploads",
                Some(json!({"path": "", "name": "big.bin", "size": MIB + 1})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // 回到設定檔的值
        t.json(
            Method::PUT,
            "/api/admin/settings",
            Some(json!({"max_upload_bytes": null})),
        )
        .await;
        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(
            (
                body["max_upload_bytes"].as_u64(),
                body["custom_max_upload"].as_bool()
            ),
            (Some(MIB), Some(false))
        );
    }
}
