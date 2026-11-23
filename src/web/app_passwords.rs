//! 應用程式密碼：給 WebDAV 用戶端使用，每台裝置一組（見 webdav.rs）。
//!
//!   GET    /api/me/app-passwords        自己的清單
//!   POST   /api/me/app-passwords        建立：{"name": "MacBook Finder"}，密碼只在回應中出現這一次
//!   DELETE /api/me/app-passwords/{id}   撤銷

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    error::AppError,
    extract::CurrentUser,
    state::{AppState, WebDavSwitch},
};
use crate::{auth::session::hash_token, db::app_passwords as db};

/// 名稱最多幾個字元
const NAME_MAX: usize = 100;

/// 密碼的字元：小寫英文與數字，去掉容易看錯的 l、o、0、1。剛好 32 個，每個字元 5 位元
const ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
/// 每組幾個字元，總共 GROUPS × GROUP_LEN × 5 = 100 位元
const GROUPS: usize = 4;
const GROUP_LEN: usize = 5;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/me/app-passwords", get(list).post(create))
        .route("/api/me/app-passwords/{id}", delete(remove))
}

/// 產生一組新的密碼，例如 `k7m3q-x9ptr-a2hwe-5ncvd`
fn generate() -> String {
    let mut bytes = [0u8; GROUPS * GROUP_LEN];
    getrandom::fill(&mut bytes).expect("failed to get secure random bytes from the OS");
    bytes
        .chunks(GROUP_LEN)
        .map(|group| {
            group
                .iter()
                // 256 是 32 的倍數：取餘數不會讓某些字元比較常出現
                .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// 存進資料庫、用來比對的雜湊。忽略大小寫、空白與 `-`：照著畫面手動輸入時不容易出錯
pub(super) fn hash(password: &str) -> Vec<u8> {
    let normalized: String = password
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    hash_token(&normalized)
}

#[derive(Serialize)]
struct Item {
    id: i64,
    name: String,
    created_at: i64,
    last_used_at: Option<i64>,
}

impl From<db::AppPassword> for Item {
    fn from(p: db::AppPassword) -> Self {
        Item {
            id: p.id,
            name: p.name,
            created_at: p.created_at,
            last_used_at: p.last_used_at,
        }
    }
}

#[derive(Serialize)]
struct ListResponse {
    /// 管理員是否開放了 WebDAV
    webdav_enabled: bool,
    /// WebDAV 用戶端要填的帳號
    username: String,
    items: Vec<Item>,
}

async fn list(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(webdav): State<WebDavSwitch>,
) -> Result<Json<ListResponse>, AppError> {
    let items = db::list(&pool, user.id).await?;
    Ok(Json(ListResponse {
        webdav_enabled: webdav.enabled(),
        username: user.username,
        items: items.into_iter().map(Item::from).collect(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    name: String,
}

#[derive(Serialize)]
struct CreateResponse {
    #[serde(flatten)]
    item: Item,
    /// 只出現這一次：資料庫只存雜湊
    password: String,
}

async fn create(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    State(webdav): State<WebDavSwitch>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CreateResponse>), AppError> {
    if !webdav.enabled() {
        return Err(AppError::Conflict(
            "WebDAV is turned off. Ask your administrator to turn it on".to_string(),
        ));
    }
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(format!(
            "Enter a name of up to {NAME_MAX} characters"
        )));
    }
    if db::count(&pool, user.id).await? >= db::MAX_PER_USER {
        return Err(AppError::Conflict(format!(
            "You can have up to {} app passwords. Revoke one you no longer use first",
            db::MAX_PER_USER
        )));
    }
    let password = generate();
    let created = db::create(&pool, user.id, name, &hash(&password)).await?;
    Ok((
        StatusCode::CREATED,
        Json(CreateResponse {
            item: created.into(),
            password,
        }),
    ))
}

async fn remove(
    user: CurrentUser,
    State(pool): State<SqlitePool>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    if db::delete(&pool, user.id, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("App password not found".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{MIB, TestApp};
    use super::*;
    use axum::http::Method;
    use serde_json::json;

    #[test]
    fn passwords_are_random_and_easy_to_type() {
        let a = generate();
        assert_eq!(a.len(), GROUPS * GROUP_LEN + GROUPS - 1);
        assert!(a.chars().all(|c| c == '-' || ALPHABET.contains(&(c as u8))));
        assert_ne!(a, generate());
        // 大小寫、空白與 - 不影響比對
        let typed = a.to_uppercase().replace('-', " ");
        assert_eq!(hash(&a), hash(&typed));
        assert_ne!(hash(&a), hash(&generate()));
    }

    #[sqlx::test]
    async fn people_create_list_and_revoke_their_app_passwords(pool: SqlitePool) {
        let t = TestApp::new(pool, MIB).await;
        let new = Some(json!({"name": "MacBook Finder"}));

        // WebDAV 沒有開放時不能建立
        let (status, _) = t
            .json(Method::POST, "/api/me/app-passwords", new.clone())
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        t.state.webdav.set(true);

        let (status, created) = t
            .json(Method::POST, "/api/me/app-passwords", new.clone())
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["name"], "MacBook Finder");
        assert_eq!(created["password"].as_str().unwrap().len(), 23);

        for bad in ["", "   ", &"x".repeat(NAME_MAX + 1), "a\nb"] {
            let (status, _) = t
                .json(
                    Method::POST,
                    "/api/me/app-passwords",
                    Some(json!({"name": bad})),
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        }

        let (_, list) = t.json(Method::GET, "/api/me/app-passwords", None).await;
        assert_eq!(list["webdav_enabled"], true);
        assert_eq!(list["username"], "carol");
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        // 清單中不會再出現密碼
        assert!(list["items"][0].get("password").is_none());

        // 別人不能撤銷
        let id = created["id"].as_i64().unwrap();
        let other = t.other_user("jerry").await;
        assert!(!db::delete(&t.pool, other, id).await.unwrap());

        let uri = format!("/api/me/app-passwords/{id}");
        let (status, _) = t.json(Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = t.json(Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, list) = t.json(Method::GET, "/api/me/app-passwords", None).await;
        assert!(list["items"].as_array().unwrap().is_empty());
    }
}
