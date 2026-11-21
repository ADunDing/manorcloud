//! 管理員設定每個人的儲存空間配額（見 `crate::quota`）：
//!
//!   GET /api/admin/quotas              每個人的配額與用量
//!   PUT /api/admin/users/{id}/quota    設定配額；{"limit_bytes": null} 是不限制
//!
//! 管理員不受配額限制，也不能被設定配額。

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{error::AppError, extract::AdminUser, state::AppState};
use crate::{
    db::user as user_db,
    quota::{self, QuotaError},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/admin/quotas", get(list))
        .route("/api/admin/users/{id}/quota", put(set))
}

#[derive(Serialize)]
struct QuotaItem {
    user_id: i64,
    /// 配額；null 是不限制
    limit_bytes: Option<i64>,
    /// 目前的用量；管理員沒有配額，是 null
    used_bytes: Option<u64>,
}

fn quota_error(e: QuotaError) -> AppError {
    AppError::internal(format!("{e:?}"))
}

/// GET /api/admin/quotas
async fn list(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
) -> Result<Json<Vec<QuotaItem>>, AppError> {
    let roles: Vec<(i64, String)> = sqlx::query_as("SELECT id, role FROM users ORDER BY id")
        .fetch_all(&pool)
        .await?;
    let limits = user_db::quotas(&pool).await?;
    let mut items = Vec::new();
    for (user_id, role) in roles {
        let used = if role == "admin" {
            None
        } else {
            Some(
                quota::usage(&pool, &data_dir, user_id)
                    .await
                    .map_err(quota_error)?,
            )
        };
        items.push(QuotaItem {
            user_id,
            limit_bytes: if role == "admin" {
                None
            } else {
                limits
                    .iter()
                    .find(|(id, _)| *id == user_id)
                    .and_then(|(_, limit)| *limit)
            },
            used_bytes: used,
        });
    }
    Ok(Json(items))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    limit_bytes: Option<i64>,
}

/// PUT /api/admin/users/{id}/quota
async fn set(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<SetRequest>,
) -> Result<StatusCode, AppError> {
    if req.limit_bytes.is_some_and(|bytes| bytes < 0) {
        return Err(AppError::BadRequest(
            "The limit can't be negative".to_string(),
        ));
    }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&pool)
        .await?;
    match role.as_deref() {
        None => return Err(AppError::NotFound("User not found".to_string())),
        Some("admin") => {
            return Err(AppError::Conflict(
                "Administrators don't have a storage limit".to_string(),
            ));
        }
        Some(_) => {}
    }
    user_db::set_quota(&pool, id, req.limit_bytes).await?;
    quota::forget(&data_dir, id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use axum::http::{Method, StatusCode};
    use serde_json::json;
    use sqlx::SqlitePool;

    #[sqlx::test]
    async fn members_cannot_go_over_their_limit(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::write(home.join("a.txt"), vec![b'a'; 600]).unwrap();
        let carol = t.user_id;

        // 管理員才能設定；carol 是一般使用者
        let (status, _) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{carol}/quota"),
                Some(json!({"limit_bytes": 1000})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        crate::db::user::set_quota(&t.pool, carol, Some(1000))
            .await
            .unwrap();

        // 容量顯示的是配額
        let (_, body) = t.json(Method::GET, "/api/storage", None).await;
        assert_eq!(body["total_bytes"], 1000);
        assert_eq!(body["available_bytes"], 400);

        // 分段上傳：建立時就拒絕
        let (status, _) = t
            .json(
                Method::POST,
                "/api/uploads",
                Some(json!({"path": "", "name": "big.bin", "size": 500})),
            )
            .await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        // 複製會超過
        let (status, _) = t
            .json(
                Method::POST,
                "/api/files/copy",
                Some(json!({"path": "a.txt", "parent": ""})),
            )
            .await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        // 放得下的可以
        let (status, _) = t
            .json(
                Method::POST,
                "/api/uploads",
                Some(json!({"path": "", "name": "small.bin", "size": 100})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn administrators_set_limits_for_members(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        t.make_admin().await;
        let alice = t.other_user("alice").await;

        let (status, _) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{alice}/quota"),
                Some(json!({"limit_bytes": 5000})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{}/quota", t.user_id),
                Some(json!({"limit_bytes": 5000})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (_, body) = t.json(Method::GET, "/api/admin/quotas", None).await;
        let items = body.as_array().unwrap();
        let alice_item = items.iter().find(|i| i["user_id"] == alice).unwrap();
        assert_eq!(alice_item["limit_bytes"], 5000);
        assert_eq!(alice_item["used_bytes"], 0);
        let carol_item = items.iter().find(|i| i["user_id"] == t.user_id).unwrap();
        assert!(carol_item["used_bytes"].is_null());
    }
}
