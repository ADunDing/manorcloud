//! 管理員修改整個伺服器的設定：
//!
//!   GET /api/admin/settings                  目前的設定
//!   PUT /api/admin/settings                  上傳上限：{"max_upload_bytes": 位元組數，或 null 使用預設值}
//!   PUT /api/admin/settings/pending-uploads  每人未完成的上傳數：{"max_pending_uploads": 數量，或 null}
//!   PUT /api/admin/settings/trusted-proxies  反向代理：{"trusted_proxies": "192.168.10.200, 127.0.0.1"}
//!   PUT /api/admin/settings/webdav           開關 WebDAV：{"enabled": true}
//!
//! 修改立即生效，不需要重新啟動。啟動時由 `load_settings` 從資料庫載入。

use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::{AdminUser, TrustedProxies},
    state::{
        AppState, DEFAULT_PENDING_UPLOAD_LIMIT, PendingUploadLimit, UploadLimit, WebDavSwitch,
    },
};
use crate::{
    config::{self, Legacy, format_ip_ranges, parse_ip_ranges},
    db::settings,
};

/// 上傳上限最小可以設定成多少：太小的值多半是單位輸入錯了
const MIN_UPLOAD_BYTES: u64 = 1024 * 1024;
/// 每人未完成的上傳數最多可以設定成多少
const MAX_PENDING_UPLOADS: u32 = 100;
/// 反向代理的設定最長幾個字元
const TRUSTED_PROXIES_MAX: usize = 1000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/settings", get(show).put(update))
        .route(
            "/api/admin/settings/pending-uploads",
            put(set_pending_uploads),
        )
        .route(
            "/api/admin/settings/trusted-proxies",
            put(set_trusted_proxies),
        )
        .route("/api/admin/settings/webdav", put(set_webdav))
}

// ==================== 啟動時載入 ====================

/// 從資料庫載入管理頁面的設定，並在日誌中列出。
///
/// 升級後第一次啟動時，設定檔中還留著的舊設定（見 config::Legacy）搬進資料庫：
/// 管理頁面還沒有設定的項目採用設定檔的值，之後設定檔的那幾行就沒有作用了，日誌提醒可以刪掉
pub async fn load_settings(state: &AppState, legacy: &Legacy) {
    let pool = &state.pool;
    if let Err(e) = import_legacy(pool, legacy).await {
        eprintln!("cannot move the old settings from the configuration file: {e}");
    }

    match settings::max_upload_bytes(pool).await {
        Ok(bytes) => state.upload_limit.set(bytes),
        Err(e) => eprintln!("cannot read the upload limit: {e}"),
    }
    println!("upload limit: {} bytes", state.upload_limit.get());

    match settings::max_pending_uploads(pool).await {
        Ok(limit) => state.pending_upload_limit.set(limit),
        Err(e) => eprintln!("cannot read the limit of unfinished uploads: {e}"),
    }
    println!(
        "unfinished uploads per user: {}",
        state.pending_upload_limit.get()
    );

    match settings::trusted_proxies(pool).await {
        // 存進資料庫之前已經檢查過；萬一不合法，當作沒有設定（只是登入次數算在代理頭上）
        Ok(text) => match parse_ip_ranges(&text) {
            Ok(ranges) => state.trusted_proxies.set(ranges),
            Err(e) => eprintln!("ignoring the reverse proxy setting: {e}"),
        },
        Err(e) => eprintln!("cannot read the reverse proxy setting: {e}"),
    }
    let proxies = state.trusted_proxies.list();
    if !proxies.is_empty() {
        println!("trusted proxies: {}", format_ip_ranges(&proxies));
    }

    match settings::webdav_enabled(pool).await {
        Ok(enabled) => {
            state.webdav.set(enabled);
            if enabled {
                println!("WebDAV: on, at /dav/");
            }
        }
        Err(e) => eprintln!("cannot read whether WebDAV is on: {e}"),
    }
}

async fn import_legacy(pool: &SqlitePool, legacy: &Legacy) -> Result<(), sqlx::Error> {
    if let Some(bytes) = legacy.max_upload_bytes {
        if settings::max_upload_bytes(pool).await?.is_none() {
            settings::set_max_upload_bytes(pool, Some(bytes)).await?;
            moved(config::MAX_UPLOAD_VAR);
        } else {
            unused(config::MAX_UPLOAD_VAR);
        }
    }
    if let Some(limit) = legacy.max_pending_uploads {
        if settings::max_pending_uploads(pool).await?.is_none() {
            settings::set_max_pending_uploads(pool, Some(limit)).await?;
            moved(config::MAX_PENDING_UPLOADS_VAR);
        } else {
            unused(config::MAX_PENDING_UPLOADS_VAR);
        }
    }
    if let Some(ranges) = &legacy.trusted_proxies {
        if settings::trusted_proxies(pool).await?.trim().is_empty() {
            settings::set_trusted_proxies(pool, &format_ip_ranges(ranges)).await?;
            moved(config::TRUSTED_PROXIES_VAR);
        } else {
            unused(config::TRUSTED_PROXIES_VAR);
        }
    }
    if legacy.secure_cookies {
        println!(
            "note: {} is no longer needed (ManorCloud now detects HTTPS itself); \
             you can delete that line from the configuration file",
            config::SECURE_COOKIES_VAR
        );
    }
    Ok(())
}

fn moved(name: &str) {
    println!(
        "note: moved {name} from the configuration file to Administration; \
         you can delete that line from the configuration file"
    );
}

fn unused(name: &str) {
    println!(
        "note: {name} in the configuration file is no longer used (it is set in Administration); \
         you can delete that line"
    );
}

// ==================== API ====================

#[derive(Serialize)]
struct SettingsResponse {
    /// 目前的上傳上限
    max_upload_bytes: u64,
    /// 管理頁面沒有設定時使用的值（10 GiB）
    default_max_upload_bytes: u64,
    /// 是否在管理頁面另外設定了
    custom_max_upload: bool,
    /// 每人最多幾個未完成的上傳
    max_pending_uploads: u32,
    default_max_pending_uploads: u32,
    /// 反向代理的位址或網段，以逗號分隔
    trusted_proxies: String,
    /// 這個請求的連線來自哪裡。放在反向代理後面時就是代理的位址：管理員照著填就對了
    connection_from: String,
    /// 是否開放 WebDAV
    webdav_enabled: bool,
}

async fn show(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(limit): State<UploadLimit>,
    State(pending): State<PendingUploadLimit>,
    State(proxies): State<TrustedProxies>,
    State(webdav): State<WebDavSwitch>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Json<SettingsResponse>, AppError> {
    let custom = settings::max_upload_bytes(&pool).await?;
    Ok(Json(SettingsResponse {
        max_upload_bytes: limit.get(),
        default_max_upload_bytes: limit.default_value(),
        custom_max_upload: custom.is_some(),
        max_pending_uploads: pending.get(),
        default_max_pending_uploads: DEFAULT_PENDING_UPLOAD_LIMIT,
        trusted_proxies: format_ip_ranges(&proxies.list()),
        connection_from: peer.ip().to_canonical().to_string(),
        webdav_enabled: webdav.enabled(),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingUploadsRequest {
    max_pending_uploads: Option<u32>,
}

async fn set_pending_uploads(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(pending): State<PendingUploadLimit>,
    Json(req): Json<PendingUploadsRequest>,
) -> Result<StatusCode, AppError> {
    if req
        .max_pending_uploads
        .is_some_and(|n| !(1..=MAX_PENDING_UPLOADS).contains(&n))
    {
        return Err(AppError::BadRequest(format!(
            "Enter a number from 1 to {MAX_PENDING_UPLOADS}"
        )));
    }
    settings::set_max_pending_uploads(&pool, req.max_pending_uploads).await?;
    pending.set(req.max_pending_uploads);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedProxiesRequest {
    trusted_proxies: String,
}

#[derive(Serialize)]
struct TrustedProxiesResponse {
    /// 整理過的寫法
    trusted_proxies: String,
}

async fn set_trusted_proxies(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(proxies): State<TrustedProxies>,
    Json(req): Json<TrustedProxiesRequest>,
) -> Result<Json<TrustedProxiesResponse>, AppError> {
    if req.trusted_proxies.chars().count() > TRUSTED_PROXIES_MAX {
        return Err(AppError::BadRequest("That's too long".to_string()));
    }
    let ranges = parse_ip_ranges(&req.trusted_proxies).map_err(AppError::BadRequest)?;
    let text = format_ip_ranges(&ranges);
    settings::set_trusted_proxies(&pool, &text).await?;
    proxies.set(ranges);
    Ok(Json(TrustedProxiesResponse {
        trusted_proxies: text,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebDavRequest {
    enabled: bool,
}

async fn set_webdav(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(webdav): State<WebDavSwitch>,
    Json(req): Json<WebDavRequest>,
) -> Result<StatusCode, AppError> {
    settings::set_webdav_enabled(&pool, req.enabled).await?;
    webdav.set(req.enabled);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use super::*;
    use axum::http::Method;
    use serde_json::json;

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

        // 回到預設值
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

    #[sqlx::test]
    async fn administrators_limit_unfinished_uploads(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let uri = "/api/admin/settings/pending-uploads";

        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(body["max_pending_uploads"], DEFAULT_PENDING_UPLOAD_LIMIT);
        for bad in [0, MAX_PENDING_UPLOADS + 1] {
            let (status, _) = t
                .json(Method::PUT, uri, Some(json!({"max_pending_uploads": bad})))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }

        // 改成 1：第二個未完成的上傳被拒絕
        let (status, _) = t
            .json(Method::PUT, uri, Some(json!({"max_pending_uploads": 1})))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let declare = |name: &'static str| {
            let t = &t;
            async move {
                t.json(
                    Method::POST,
                    "/api/uploads",
                    Some(json!({"path": "", "name": name, "size": 10})),
                )
                .await
            }
        };
        assert_eq!(declare("a.bin").await.0, StatusCode::CREATED);
        let (status, body) = declare("b.bin").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "too_many_uploads");

        // 回到預設值
        t.json(Method::PUT, uri, Some(json!({"max_pending_uploads": null})))
            .await;
        assert_eq!(
            t.state.pending_upload_limit.get(),
            DEFAULT_PENDING_UPLOAD_LIMIT
        );
        assert_eq!(declare("b.bin").await.0, StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn administrators_set_the_reverse_proxy(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let uri = "/api/admin/settings/trusted-proxies";

        // 管理員看得到這個請求的連線從哪裡來（測試中固定是 192.168.1.10）
        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(body["connection_from"], "192.168.1.10");
        assert_eq!(body["trusted_proxies"], "");

        let (status, _) = t
            .json(
                Method::PUT,
                uri,
                Some(json!({"trusted_proxies": "192.168.1.10, nginx"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = t
            .json(
                Method::PUT,
                uri,
                Some(json!({"trusted_proxies": " 192.168.1.10 ,172.18.0.0/16,"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["trusted_proxies"], "192.168.1.10, 172.18.0.0/16");
        assert_eq!(t.state.trusted_proxies.list().len(), 2);
        assert_eq!(
            settings::trusted_proxies(&t.pool).await.unwrap(),
            "192.168.1.10, 172.18.0.0/16"
        );

        // 清空
        let (status, _) = t
            .json(Method::PUT, uri, Some(json!({"trusted_proxies": ""})))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(t.state.trusted_proxies.list().is_empty());
    }

    #[sqlx::test]
    async fn old_settings_move_from_the_configuration_file_once(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let legacy = Legacy {
            max_upload_bytes: Some(3 * MIB),
            max_pending_uploads: Some(9),
            trusted_proxies: Some(vec![config::IpRange::parse("192.168.10.200").unwrap()]),
            secure_cookies: true,
        };

        load_settings(&t.state, &legacy).await;
        assert_eq!(t.state.upload_limit.get(), 3 * MIB);
        assert_eq!(t.state.pending_upload_limit.get(), 9);
        assert_eq!(
            format_ip_ranges(&t.state.trusted_proxies.list()),
            "192.168.10.200"
        );

        // 之後在管理頁面改過：設定檔還留著的舊值不會蓋掉它
        settings::set_max_pending_uploads(&t.pool, Some(2))
            .await
            .unwrap();
        load_settings(&t.state, &legacy).await;
        assert_eq!(t.state.pending_upload_limit.get(), 2);

        // 沒有任何設定時是預設值
        settings::set_max_upload_bytes(&t.pool, None).await.unwrap();
        load_settings(&t.state, &Legacy::default()).await;
        assert_eq!(t.state.upload_limit.get(), MIB);
    }

    #[sqlx::test]
    async fn administrators_turn_webdav_on_and_off(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let on = Some(json!({"enabled": true}));
        let (status, _) = t
            .json(Method::PUT, "/api/admin/settings/webdav", on.clone())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        t.make_admin().await;

        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(body["webdav_enabled"], false);
        let (status, _) = t.json(Method::PUT, "/api/admin/settings/webdav", on).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = t.json(Method::GET, "/api/admin/settings", None).await;
        assert_eq!(body["webdav_enabled"], true);
        assert!(t.state.webdav.enabled());
        let (_, body) = t.json(Method::GET, "/api/config", None).await;
        assert_eq!(body["webdav"], true);
        assert!(settings::webdav_enabled(&t.pool).await.unwrap());
    }
}
