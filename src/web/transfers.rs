//! 跨硬碟的複製與移動（工作本身在 transfer.rs）：
//!
//!   POST   /api/transfers         開始：把一個資料夾中的幾個項目複製或移動到另一個空間
//!   GET    /api/transfers         自己的傳輸，最新的在前
//!   DELETE /api/transfers/{id}    進行中：取消；已經結束：從清單中移除
//!
//! 同一個空間裡的複製、移動用 /api/files（spaces.rs）：只是改名，一瞬間完成。

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::{AppError, Forbidden},
    extract::CurrentUser,
    spaces::{blocking, check_quota},
    state::AppState,
};
use crate::{
    db::transfers::{self as transfers_db, NewTransfer, Transfer},
    space::SpaceError,
    storage,
    transfer::{self, PlanError, TransferSpec},
    tree,
};

/// 一次最多選幾個項目
const MAX_ITEMS: usize = 1000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/transfers", get(list).post(start))
        .route("/api/transfers/{id}", delete(remove))
}

#[derive(Serialize)]
struct TransferResponse {
    id: i64,
    /// copy 或 move
    kind: String,
    names: Vec<String>,
    from_space: String,
    to_space: String,
    /// 目的地資料夾
    to_path: String,
    total_items: usize,
    done_items: i64,
    total_bytes: i64,
    done_bytes: i64,
    /// running、done、failed、cancelled
    state: String,
    error: Option<String>,
    /// 略過的項目與原因
    skipped: Vec<String>,
}

impl From<Transfer> for TransferResponse {
    fn from(t: Transfer) -> Self {
        let names: Vec<String> = serde_json::from_str(&t.names).unwrap_or_default();
        TransferResponse {
            id: t.id,
            kind: t.kind,
            total_items: names.len(),
            names,
            from_space: t.from_space,
            to_space: t.to_space,
            to_path: t.to_dir,
            done_items: t.done_items,
            total_bytes: t.total_bytes,
            done_bytes: t.done_bytes,
            state: t.state,
            error: t.error,
            skipped: serde_json::from_str(&t.skipped).unwrap_or_default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    /// copy 或 move
    kind: String,
    /// 來源的空間（與網址的 space= 相同；me 是自己的 My files）
    from_space: String,
    /// 來源資料夾
    #[serde(default)]
    from_path: String,
    /// 來源資料夾中選取的名稱
    names: Vec<String>,
    to_space: String,
    /// 目的地資料夾
    #[serde(default)]
    to_path: String,
}

fn plan_error(e: PlanError) -> AppError {
    match e {
        PlanError::NotFound => AppError::NotFound("Not found".to_string()),
        PlanError::Invalid(m) => AppError::BadRequest(m),
        PlanError::Denied(m) => AppError::Conflict(m),
        PlanError::Space(SpaceError::NotFound) => AppError::NotFound("Not found".to_string()),
        PlanError::Space(SpaceError::AdminOnly) => AppError::Forbidden(Forbidden::AdminRequired),
        PlanError::Space(SpaceError::Unavailable(m)) => AppError::Unavailable(m),
        PlanError::Space(SpaceError::Db(e)) => AppError::internal(e),
    }
}

/// POST /api/transfers：檢查權限、名稱與空間之後，在背景開始
async fn start(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    Json(req): Json<StartRequest>,
) -> Result<(StatusCode, Json<TransferResponse>), AppError> {
    if req.kind != "copy" && req.kind != "move" {
        return Err(AppError::BadRequest(
            "kind must be copy or move".to_string(),
        ));
    }
    let mut names: Vec<String> = Vec::new();
    for name in &req.names {
        let name = tree::check_new_name(name).map_err(AppError::BadRequest)?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() || names.len() > MAX_ITEMS {
        return Err(AppError::BadRequest(format!("Choose 1–{MAX_ITEMS} items")));
    }
    let spec = TransferSpec {
        kind: &req.kind,
        from_space: &req.from_space,
        from_dir: &req.from_path,
        names: &names,
        to_space: &req.to_space,
        to_dir: &req.to_path,
    };
    let plan = transfer::plan(&pool, &data_dir, user.id, user.role == "admin", &spec)
        .await
        .map_err(plan_error)?;

    // 每個項目都存在；目的地資料夾存在；放得下
    let from_root = plan.from.root.clone();
    let items = names
        .iter()
        .map(|name| plan.from.abs(&plan.from_dir.child(name)))
        .collect::<Result<Vec<_>, _>>()?;
    let to_root = plan.to.root.clone();
    let to_dir = plan.to.abs(&plan.to_dir)?;
    let total = blocking(move || {
        tree::resolve_dir(&to_root, &to_dir)?;
        let mut total = 0;
        for item in &items {
            total += tree::tree_size(&tree::resolve(&from_root, item, true)?);
        }
        if total > storage::available_bytes(&to_root)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "not enough space",
            ));
        }
        Ok(total)
    })
    .await
    .map_err(|e| match e {
        AppError::InsufficientStorage(_) => AppError::InsufficientStorage(
            "There isn't enough free space on the destination".to_string(),
        ),
        other => other,
    })?;

    // 移動到同一個空間不會增加用量；其他情況檢查目的地的配額
    if !(req.kind == "move" && plan.from.key == plan.to.key) {
        check_quota(&pool, &data_dir, &user, &plan.to, total).await?;
    }

    let created = transfers_db::create(
        &pool,
        &NewTransfer {
            owner_id: user.id,
            kind: &req.kind,
            from_space: &plan.from.key.to_param(),
            from_dir: &plan.from_dir.display(),
            names: &names,
            to_space: &plan.to.key.to_param(),
            to_dir: &plan.to_dir.display(),
            total_bytes: i64::try_from(total).unwrap_or(i64::MAX),
        },
    )
    .await?;
    transfer::spawn(pool.clone(), Arc::clone(&data_dir), created.id);
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// GET /api/transfers
async fn list(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<TransferResponse>>, AppError> {
    let items = transfers_db::list_for(&pool, user.id).await?;
    Ok(Json(
        items.into_iter().map(TransferResponse::from).collect(),
    ))
}

/// DELETE /api/transfers/{id}：進行中的取消，已經結束的從清單中移除
async fn remove(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    if transfers_db::cancel(&pool, id, user.id).await?
        || transfers_db::dismiss(&pool, id, user.id).await?
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    Err(AppError::NotFound("Transfer not found".to_string()))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use super::*;
    use crate::db::storage as storage_db;
    use axum::http::Method;
    use serde_json::{Value, json};
    use std::time::Duration;

    /// 管理員 carol 加上一顆 Disk2，回傳它的編號與路徑
    async fn with_disk(t: &TestApp) -> (i64, tempfile::TempDir) {
        t.make_admin().await;
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let id = storage_db::add_volume(&t.pool, "Disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        (id, disk)
    }

    async fn wait(t: &TestApp, id: i64) -> Value {
        for _ in 0..200 {
            let (_, list) = t.json(Method::GET, "/api/transfers", None).await;
            let item = list
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["id"] == id)
                .unwrap()
                .clone();
            if item["state"] != "running" {
                return item;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the transfer did not finish");
    }

    #[sqlx::test]
    async fn moving_to_another_disk_copies_then_deletes(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (disk, dir) = with_disk(&t).await;
        let data = t.data_dir();
        std::fs::create_dir_all(data.join("相片/2026")).unwrap();
        std::fs::write(data.join("相片/2026/a.jpg"), b"aaa").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", data.join("相片/link")).unwrap();
        std::fs::write(dir.path().join("相片"), b"already here").unwrap();

        let (status, body) = t
            .json(
                Method::POST,
                "/api/transfers",
                Some(json!({
                    "kind": "move", "from_space": "me", "names": ["相片"],
                    "to_space": disk.to_string()
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let done = wait(&t, body["id"].as_i64().unwrap()).await;

        assert_eq!(done["state"], "done", "{done}");
        assert_eq!(done["done_bytes"], 3);
        // 同名的檔案已經存在：自動改名
        assert_eq!(
            std::fs::read(dir.path().join("相片 (1)/2026/a.jpg")).unwrap(),
            b"aaa"
        );
        assert!(!data.join("相片").exists());
        assert_eq!(done["skipped"].as_array().unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn copying_keeps_the_source(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let (disk, dir) = with_disk(&t).await;
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();

        let (status, body) = t
            .json(
                Method::POST,
                "/api/transfers",
                Some(json!({
                    "kind": "copy", "from_space": disk.to_string(), "names": ["b.txt"],
                    "to_space": "me"
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(
            wait(&t, body["id"].as_i64().unwrap()).await["state"],
            "done"
        );

        assert!(dir.path().join("b.txt").exists());
        assert_eq!(std::fs::read(t.data_dir().join("b.txt")).unwrap(), b"b");
        let (status, _) = t
            .json(
                Method::DELETE,
                &format!("/api/transfers/{}", body["id"]),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    async fn bad_requests_are_refused_before_starting(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        std::fs::create_dir_all(t.home("carol").join("docs")).unwrap();

        // 一般使用者不能使用硬碟的最上層
        let (status, _) = t
            .json(
                Method::POST,
                "/api/transfers",
                Some(json!({"kind": "copy", "from_space": "me", "names": ["docs"], "to_space": "default"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (disk, _dir) = with_disk(&t).await;
        // 不存在的項目、放進自己裡面、別人的資料夾
        for body in [
            json!({"kind": "copy", "from_space": "me", "names": ["nope"], "to_space": disk.to_string()}),
            json!({"kind": "move", "from_space": "me", "names": ["users"], "to_space": "default",
                   "to_path": "users/carol"}),
            json!({"kind": "move", "from_space": "default", "from_path": "users/carol", "names": ["docs"],
                   "to_space": "me", "to_path": "users/carol/docs"}),
        ] {
            let (status, _) = t
                .json(Method::POST, "/api/transfers", Some(body.clone()))
                .await;
            assert!(status.is_client_error(), "{body}: {status}");
        }
    }
}
