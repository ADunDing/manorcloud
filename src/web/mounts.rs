//! 掛載：把整個儲存位置交給指定的一般使用者（見 db::mounts）。
//!
//!   GET /api/mounts                         自己可以使用的掛載（側邊欄）
//!   GET /api/admin/storage/{id}/mounts      這個位置掛載給了誰
//!   PUT /api/admin/storage/{id}/mounts      設定掛載給誰（整份取代）
//!
//! 掛載之後，檔案的 API 以 `space={id}` 使用它，與管理員打開一顆硬碟相同；
//! 權限檢查在 `space::resolve`。

use axum::{
    Json, Router,
    extract::{Path as UrlPath, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::{AdminUser, CurrentUser},
    state::AppState,
};
use crate::db::mounts::{self as mounts_db, Grant, SetMountsError};

/// 一個位置最多掛載給幾個人：設定是整份取代，這個上限讓請求的大小有界
const MAX_GRANTS: usize = 1000;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/mounts", get(mine)).route(
        "/api/admin/storage/{id}/mounts",
        get(of_volume).put(set_for_volume),
    )
}

#[derive(Serialize)]
struct MountItem {
    /// 儲存位置的編號：檔案的 API 用 space={id}
    id: i64,
    name: String,
    /// false：只能瀏覽與下載
    writable: bool,
}

/// GET /api/mounts：管理員本來就看得到每一顆硬碟，一律是空的
async fn mine(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
) -> Result<Json<Vec<MountItem>>, AppError> {
    if user.role == "admin" {
        return Ok(Json(Vec::new()));
    }
    let mounts = mounts_db::of_user(&pool, user.id).await?;
    Ok(Json(
        mounts
            .into_iter()
            .map(|m| MountItem {
                id: m.volume_id,
                name: m.name,
                writable: m.can_write,
            })
            .collect(),
    ))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantItem {
    user_id: i64,
    writable: bool,
}

/// GET /api/admin/storage/{id}/mounts
async fn of_volume(
    _admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<Vec<GrantItem>>, AppError> {
    let grants = mounts_db::of_volume(&pool, id).await?;
    Ok(Json(
        grants
            .into_iter()
            .map(|g| GrantItem {
                user_id: g.user_id,
                writable: g.can_write,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetRequest {
    grants: Vec<GrantItem>,
}

/// PUT /api/admin/storage/{id}/mounts：整份取代。同一個人出現兩次時，以最後一次為準
async fn set_for_volume(
    admin: AdminUser,
    State(pool): State<SqlitePool>,
    UrlPath(id): UrlPath<i64>,
    Json(req): Json<SetRequest>,
) -> Result<StatusCode, AppError> {
    if req.grants.len() > MAX_GRANTS {
        return Err(AppError::BadRequest(format!(
            "A storage can be shared with at most {MAX_GRANTS} people"
        )));
    }
    let grants: Vec<Grant> = req
        .grants
        .iter()
        .map(|g| Grant {
            user_id: g.user_id,
            can_write: g.writable,
        })
        .collect();
    mounts_db::set_for_volume(&pool, id, &grants)
        .await
        .map_err(|e| match e {
            SetMountsError::VolumeNotFound => {
                AppError::NotFound("Storage not found".to_string())
            }
            SetMountsError::NotShared => AppError::Conflict(
                "Set this storage to Share with members first".to_string(),
            ),
            SetMountsError::NotAMember => AppError::BadRequest(
                "A storage can only be shared with members. Administrators can already see every storage"
                    .to_string(),
            ),
            SetMountsError::Db(e) => AppError::internal(e),
        })?;
    println!(
        "storage: location {id} shared with {} people by administrator {}",
        grants.len(),
        admin.id
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp, request};
    use super::*;
    use axum::{body::Body, http::Method};
    use serde_json::{Value, json};

    /// 管理員 alice 新增一個位置（裡面已經有檔案），回傳位置的編號。
    /// 目前登入的 carol 是一般使用者
    /// 用途是分享給成員的位置，裡面已經有檔案
    async fn disk_with_files(t: &TestApp, disk: &std::path::Path) -> i64 {
        std::fs::create_dir(disk.join("iso")).unwrap();
        std::fs::write(disk.join("iso/debian.iso"), b"debian").unwrap();
        crate::storage::mark_volume(disk).unwrap();
        crate::storage::init_dirs(disk).unwrap();
        let id = crate::db::storage::add_volume(&t.pool, "iso", &disk.display().to_string())
            .await
            .unwrap()
            .id;
        crate::db::storage::set_shared(&t.pool, id, true)
            .await
            .unwrap();
        id
    }

    async fn grant(t: &TestApp, volume: i64, writable: bool) {
        mounts_db::set_for_volume(
            &t.pool,
            volume,
            &[Grant {
                user_id: t.user_id,
                can_write: writable,
            }],
        )
        .await
        .unwrap();
    }

    fn names(body: &Value) -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap().to_string())
            .collect()
    }

    async fn upload(t: &TestApp, volume: i64) -> StatusCode {
        let boundary = "XBOUNDARY";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\
             Content-Type: text/plain\r\n\r\nhello\r\n--{boundary}--\r\n"
        );
        let req = request(
            Method::POST,
            &format!("/api/files?path=iso&name=a.txt&space={volume}"),
            Some(&t.cookie),
        )
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap();
        t.send(req).await.0
    }

    #[sqlx::test]
    async fn only_people_it_is_mounted_for_can_open_it(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        let listing = format!("/api/files?path=iso&space={volume}");

        let (status, _) = t.json(Method::GET, &listing, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (_, body) = t.json(Method::GET, "/api/mounts", None).await;
        assert_eq!(body, json!([]));

        grant(&t, volume, false).await;
        let (status, body) = t.json(Method::GET, &listing, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(names(&body), ["debian.iso"]);
        let (_, body) = t.json(Method::GET, "/api/mounts", None).await;
        assert_eq!(
            body,
            json!([{"id": volume, "name": "iso", "writable": false}])
        );

        // 資料目錄永遠不能用 space= 打開：那裡是每個人的 My files
        let (status, _) = t.json(Method::GET, "/api/files?space=default", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 取消掛載之後立刻打不開
        mounts_db::set_for_volume(&t.pool, volume, &[])
            .await
            .unwrap();
        let (status, _) = t.json(Method::GET, &listing, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    async fn read_only_mounts_can_be_read_but_not_changed(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        grant(&t, volume, false).await;

        let (status, body) = t
            .json(
                Method::GET,
                &format!("/api/files?path=iso&space={volume}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["read_only"].is_string(), "{body}");
        let (status, _, content) = t
            .send(
                request(
                    Method::GET,
                    &format!("/api/files/content?path=iso/debian.iso&space={volume}"),
                    Some(&t.cookie),
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(&content[..], b"debian");

        assert_eq!(upload(&t, volume).await, StatusCode::CONFLICT);
        let (status, _) = t
            .json(
                Method::DELETE,
                &format!("/api/files?path=iso/debian.iso&space={volume}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = t
            .json(Method::DELETE, &format!("/api/trash?space={volume}"), None)
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // 唯讀的人不能把它分享給任何拿到連結的人
        let (status, _) = t
            .json(
                Method::POST,
                &format!("/api/shares?space={volume}"),
                Some(json!({"path": "iso/debian.iso"})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(disk.path().join("iso/debian.iso").exists());
    }

    #[sqlx::test]
    async fn writable_mounts_can_be_changed_without_using_the_quota(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        grant(&t, volume, true).await;
        // 配額只有 1 位元組：掛載中的檔案不算進去
        sqlx::query("UPDATE users SET quota_bytes = 1 WHERE id = ?")
            .bind(t.user_id)
            .execute(&t.pool)
            .await
            .unwrap();

        assert_eq!(upload(&t, volume).await, StatusCode::CREATED);
        assert_eq!(
            std::fs::read(disk.path().join("iso/a.txt")).unwrap(),
            b"hello"
        );

        // 分享的位置沒有垃圾桶：畫面先確認，刪除就是永久刪除
        let (_, listing) = t
            .json(
                Method::GET,
                &format!("/api/files?path=iso&space={volume}"),
                None,
            )
            .await;
        assert_eq!(listing["trash"], false);
        let (status, body) = t
            .json(
                Method::DELETE,
                &format!("/api/files?path=iso&space={volume}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["trash_id"], Value::Null);
        assert!(!disk.path().join("iso").exists());
        let (_, trash) = t
            .json(Method::GET, &format!("/api/trash?space={volume}"), None)
            .await;
        assert_eq!(trash, json!([]));
        let in_trash = std::fs::read_dir(crate::storage::trash_dir(disk.path()))
            .map_or(0, |items| items.count());
        assert_eq!(in_trash, 0);
    }

    #[sqlx::test]
    async fn the_users_folder_is_hidden_in_a_mount(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        std::fs::create_dir(disk.path().join("users")).unwrap();
        grant(&t, volume, true).await;

        let (_, body) = t
            .json(Method::GET, &format!("/api/files?space={volume}"), None)
            .await;
        assert_eq!(names(&body), ["iso"]);
        let (status, _) = t
            .json(
                Method::POST,
                &format!("/api/folders?space={volume}"),
                Some(json!({"path": "", "name": "users"})),
            )
            .await;
        assert_ne!(status, StatusCode::CREATED);
    }

    #[sqlx::test]
    async fn administrators_manage_who_it_is_mounted_for(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        let jerry = t.other_user("jerry").await;
        let uri = format!("/api/admin/storage/{volume}/mounts");
        let body = json!({"grants": [{"user_id": jerry, "writable": true}]});

        // 一般使用者不能設定
        let (status, _) = t.json(Method::PUT, &uri, Some(body.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        t.make_admin().await;
        let (status, _) = t.json(Method::PUT, &uri, Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, grants) = t.json(Method::GET, &uri, None).await;
        assert_eq!(grants, json!([{"user_id": jerry, "writable": true}]));

        // 管理員（carol 自己）不能被掛載：他本來就看得到每一顆硬碟
        let (status, _) = t
            .json(
                Method::PUT,
                &uri,
                Some(json!({"grants": [{"user_id": t.user_id, "writable": true}]})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // 分享的位置不能放任何人的 My files
        let (status, body) = t
            .json(
                Method::PUT,
                &format!("/api/admin/users/{jerry}/storage"),
                Some(json!({"volume_id": volume})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// 分享的位置上不會有人的 My files；萬一硬碟上還留著 users（例如手動放回去），
    /// 掛載的人用任何方式都碰不到（Space::abs 這道防線）
    #[sqlx::test]
    async fn a_mount_cannot_reach_peoples_own_files(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let disk = tempfile::tempdir().unwrap();
        let volume = disk_with_files(&t, disk.path()).await;
        std::fs::create_dir_all(disk.path().join("users/jerry/私人")).unwrap();
        std::fs::write(disk.path().join("users/jerry/私人/日記.txt"), b"secret").unwrap();
        std::fs::write(disk.path().join("users/jerry/photo.png"), b"png").unwrap();
        grant(&t, volume, true).await;

        let get = |uri: String| {
            let t = &t;
            async move {
                t.send(
                    request(Method::GET, &uri, Some(&t.cookie))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .0
            }
        };
        let not_found = StatusCode::NOT_FOUND;
        for uri in [
            format!("/api/files?path=users&space={volume}"),
            format!("/api/files?path=users/jerry&space={volume}"),
            format!("/api/files/content?path=users/jerry/私人/日記.txt&space={volume}"),
            format!("/api/archive?names=users&space={volume}"),
            format!("/api/archive?path=users&names=jerry&space={volume}"),
            format!(
                "/api/files/thumbnail?path=users/jerry/photo.png&size=small&v=1&space={volume}"
            ),
            format!("/api/search?path=users&q=日記&space={volume}"),
        ] {
            assert_eq!(get(uri.clone()).await, not_found, "{uri}");
        }

        // 從最上層搜尋：找不到 users 裡面的檔案
        let (status, body) = t
            .json(
                Method::GET,
                &format!("/api/search?q=日記&space={volume}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains("日記"), "{body}");

        // 複製到自己的 My files、分享連結：都拒絕
        let (status, _) = t
            .json(
                Method::POST,
                "/api/transfers",
                Some(
                    json!({"kind": "copy", "from_space": volume.to_string(), "from_path": "users",
                            "names": ["jerry"], "to_space": "me"}),
                ),
            )
            .await;
        assert_eq!(status, not_found);
        let (status, _) = t
            .json(
                Method::POST,
                &format!("/api/shares?space={volume}"),
                Some(json!({"path": "users/jerry"})),
            )
            .await;
        assert_ne!(status, StatusCode::CREATED);
        // 改名、刪除也碰不到
        let (status, _) = t
            .json(
                Method::DELETE,
                &format!("/api/files?path=users/jerry/photo.png&space={volume}"),
                None,
            )
            .await;
        assert_ne!(status, StatusCode::OK);
        assert!(disk.path().join("users/jerry/photo.png").exists());

        // 他自己照常使用 My files
        assert!(disk.path().join("users/jerry/私人/日記.txt").exists());
    }
}
