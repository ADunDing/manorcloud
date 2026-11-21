//! 垃圾桶：列出、還原、永久刪除、清空。每個空間各自的垃圾桶，以網址參數 `space=` 區分。
//!
//!   GET    /api/trash                 垃圾桶的內容，最近刪除的在前
//!   POST   /api/trash/{id}/restore    還原到原本的位置（同名時自動改名）
//!   DELETE /api/trash/{id}            永久刪除
//!   DELETE /api/trash                 清空
//!
//! 放進垃圾桶是 DELETE /api/files（見 spaces.rs）。永久刪除先刪檔案、再刪紀錄（見 db::trash）。

use std::path::Path;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::Serialize;
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::InSpace,
    spaces::{blocking, denied},
    state::AppState,
};
use crate::{
    db::trash::{self as trash_db, TrashItem},
    quota,
    space::Space,
    storage,
    tree::{self, OnConflict, RelPath},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/trash", get(list).delete(empty))
        .route("/api/trash/{id}", delete(delete_forever))
        .route("/api/trash/{id}/restore", post(restore))
}

fn not_in_trash() -> AppError {
    AppError::NotFound("Item not found in the trash".to_string())
}

/// 垃圾桶中的一個項目
#[derive(Serialize)]
struct TrashResponse {
    id: i64,
    name: String,
    kind: String,
    /// 檔案的大小；資料夾是裡面所有檔案的總和
    size: i64,
    /// 放進垃圾桶的時間（Unix 秒數）
    trashed_at: i64,
    /// 原本的位置，由上往下的資料夾名稱；空的代表空間的最上層
    location: Vec<String>,
}

impl From<TrashItem> for TrashResponse {
    fn from(item: TrashItem) -> Self {
        let mut location: Vec<String> = item.path.split('/').map(str::to_string).collect();
        let name = location.pop().unwrap_or_default();
        TrashResponse {
            id: item.id,
            name,
            kind: item.kind,
            size: item.size,
            trashed_at: item.trashed_at,
            location,
        }
    }
}

/// GET /api/trash
async fn list(
    InSpace { space, .. }: InSpace,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<TrashResponse>>, AppError> {
    let place = space.trash;
    let items = trash_db::list(&pool, place).await?;
    Ok(Json(items.into_iter().map(TrashResponse::from).collect()))
}

#[derive(Serialize)]
struct RestoreResponse {
    /// 還原後的路徑（同名時可能加上了編號）
    path: String,
}

/// POST /api/trash/{id}/restore：放回原本的位置。原本的資料夾已經不在時重新建立
async fn restore(
    InSpace { space, .. }: InSpace,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<RestoreResponse>, AppError> {
    let place = space.trash;
    let item = trash_db::find(&pool, place, id)
        .await?
        .ok_or_else(not_in_trash)?;
    let original = RelPath::parse(&item.path).map_err(|_| not_in_trash())?;
    let name = original.name().ok_or_else(not_in_trash)?.to_string();
    let parent = original.parent().unwrap_or_default();
    space.check_write_in(&parent).map_err(denied)?;

    let source = storage::trash_path(&space.root, id);
    let root = space.root.clone();
    let abs_parent = space.abs(&parent);
    let placed = blocking(move || {
        if std::fs::symlink_metadata(&source).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the item is no longer in the trash",
            ));
        }
        let dir = tree::create_dirs(&root, &abs_parent)?;
        tree::place(&source, &dir, &name, OnConflict::Rename)
    })
    .await;
    let placed = match placed {
        Ok(placed) => placed,
        Err(AppError::NotFound(_)) => {
            // 檔案已經不在（例如被手動刪除）：紀錄也不需要了
            trash_db::remove(&pool, id).await?;
            return Err(not_in_trash());
        }
        Err(e) => return Err(e),
    };
    trash_db::remove(&pool, id).await?;
    Ok(Json(RestoreResponse {
        path: parent.child(&placed).display(),
    }))
}

/// 永久刪除一個項目：先刪檔案，再刪紀錄
async fn purge(pool: &SqlitePool, space: &Space, id: i64) -> Result<(), AppError> {
    let path = storage::trash_path(&space.root, id);
    blocking(move || match tree::remove_path(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
    .await?;
    trash_db::remove(pool, id).await?;
    Ok(())
}

/// DELETE /api/trash/{id}
async fn delete_forever(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
    UrlPath(id): UrlPath<i64>,
) -> Result<StatusCode, AppError> {
    let place = space.trash;
    trash_db::find(&pool, place, id)
        .await?
        .ok_or_else(not_in_trash)?;
    purge(&pool, &space, id).await?;
    // 配額的用量包含垃圾桶：重新計算
    quota::forget(&data_dir, user.id);
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/trash：清空
async fn empty(
    InSpace { user, space }: InSpace,
    State(pool): State<SqlitePool>,
    State(data_dir): State<Arc<Path>>,
) -> Result<StatusCode, AppError> {
    let place = space.trash;
    for item in trash_db::list(&pool, place).await? {
        purge(&pool, &space, item.id).await?;
    }
    quota::forget(&data_dir, user.id);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use axum::http::{Method, StatusCode};
    use serde_json::Value;
    use sqlx::SqlitePool;

    #[sqlx::test]
    async fn restore_puts_it_back_and_recreates_the_folder(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::create_dir(home.join("docs")).unwrap();
        std::fs::write(home.join("docs/a.txt"), b"a").unwrap();
        t.json(Method::DELETE, "/api/files?path=docs/a.txt", None)
            .await;
        t.json(Method::DELETE, "/api/files?path=docs", None).await;
        // 同名的檔案已經被放回去
        std::fs::create_dir(home.join("docs")).unwrap();
        std::fs::write(home.join("docs/a.txt"), b"new").unwrap();

        let (_, list) = t.json(Method::GET, "/api/trash", None).await;
        assert_eq!(list.as_array().unwrap().len(), 2);
        let file = list
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "a.txt")
            .unwrap();
        assert_eq!(file["location"], serde_json::json!(["docs"]));

        let id = file["id"].as_i64().unwrap();
        let (status, body) = t
            .json(Method::POST, &format!("/api/trash/{id}/restore"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["path"], "docs/a (1).txt");
        assert_eq!(std::fs::read(home.join("docs/a (1).txt")).unwrap(), b"a");
    }

    #[sqlx::test]
    async fn emptying_removes_the_files(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let home = t.home("carol");
        std::fs::write(home.join("a.txt"), b"a").unwrap();
        t.json(Method::DELETE, "/api/files?path=a.txt", None).await;

        let (status, _) = t.json(Method::DELETE, "/api/trash", None).await;

        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, list) = t.json(Method::GET, "/api/trash", None).await;
        assert_eq!(list, Value::Array(vec![]));
        assert_eq!(
            std::fs::read_dir(t.data_dir().join(".manorcloud/trash"))
                .unwrap()
                .count(),
            0
        );
    }

    #[sqlx::test]
    async fn other_trashes_are_not_reachable(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let jerry = t.other_user("jerry").await;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO trash (owner_id, path, kind) VALUES (?, 'x', 'file') RETURNING id",
        )
        .bind(jerry)
        .fetch_one(&t.pool)
        .await
        .unwrap();

        let (status, _) = t
            .json(Method::POST, &format!("/api/trash/{id}/restore"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = t
            .json(Method::DELETE, &format!("/api/trash/{id}"), None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
