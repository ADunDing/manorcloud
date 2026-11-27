//! 儲存空間配額：管理員可以限制一般使用者最多使用多少空間。
//!
//! - 用量 = 自己的 My files 裡所有檔案的大小 + 垃圾桶中自己的項目（刪除後仍佔空間，清空才會釋放）
//! - 在新增內容之前檢查：上傳、複製、從其他硬碟複製或移動進來。超過時拒絕，不會寫到一半
//! - 管理員不受限制：管理員的 My files 是整顆硬碟的最上層，由所有管理員共用
//!
//! 計算用量要走訪整個資料夾，所以結果保留一分鐘。這段期間新增的內容直接加上去，
//! 連續上傳很多個小檔案時不會每個都重新計算；刪除的部分要等一分鐘後才會反映（寧可多算）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use crate::{
    db::{trash as trash_db, user as user_db},
    space::{self, SpaceError, SpaceKey},
    tree,
};

/// 計算出的用量保留多久
const CACHE_FOR: Duration = Duration::from_secs(60);

/// (資料目錄, 使用者) → (計算的時間, 用量)。鍵包含資料目錄：測試時每個測試有自己的資料目錄
type Cache = HashMap<(PathBuf, i64), (Instant, u64)>;

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
pub enum QuotaError {
    /// 加上這些內容會超過配額
    Exceeded,
    Space(SpaceError),
    Db(sqlx::Error),
}

impl From<SpaceError> for QuotaError {
    fn from(e: SpaceError) -> Self {
        QuotaError::Space(e)
    }
}

impl From<sqlx::Error> for QuotaError {
    fn from(e: sqlx::Error) -> Self {
        QuotaError::Db(e)
    }
}

/// 一般使用者目前的用量（位元組）
pub async fn usage(pool: &SqlitePool, data_dir: &Path, user_id: i64) -> Result<u64, QuotaError> {
    if let Some(used) = cached(data_dir, user_id) {
        return Ok(used);
    }
    let space = space::resolve(pool, data_dir, user_id, false, SpaceKey::Me).await?;
    let root = space.root.clone();
    // My files 本身的位置（不經過 abs：My files 不是掛載，abs 不會拒絕它）
    let base = space.base.clone();
    let files = tokio::task::spawn_blocking(move || {
        tree::resolve(&root, &base, true)
            .map(|path| tree::tree_size(&path))
            .unwrap_or(0)
    })
    .await
    .unwrap_or(0);
    let trash: u64 = trash_db::of_owner(pool, user_id)
        .await?
        .iter()
        .map(|item| u64::try_from(item.size).unwrap_or(0))
        .sum();
    let used = files + trash;
    store(data_dir, user_id, used);
    Ok(used)
}

/// 確認還放得下 `extra` 位元組。管理員、沒有設定配額的人一律放得下。
/// 通過時先把這些位元組算進用量，同時進行的上傳不會一起超過
pub async fn reserve(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    admin: bool,
    extra: u64,
) -> Result<(), QuotaError> {
    if admin {
        return Ok(());
    }
    let Some(limit) = user_db::quota(pool, user_id).await? else {
        return Ok(());
    };
    let used = usage(pool, data_dir, user_id).await?;
    if used.saturating_add(extra) > limit {
        return Err(QuotaError::Exceeded);
    }
    store(data_dir, user_id, used + extra);
    Ok(())
}

/// 用量可能大幅改變時（清空垃圾桶、管理員改了配額）重新計算
pub fn forget(data_dir: &Path, user_id: i64) {
    if let Ok(mut cache) = CACHE.lock() {
        cache.remove(&(data_dir.to_path_buf(), user_id));
    }
}

fn cached(data_dir: &Path, user_id: i64) -> Option<u64> {
    let cache = CACHE.lock().ok()?;
    let (at, used) = cache.get(&(data_dir.to_path_buf(), user_id))?;
    (at.elapsed() < CACHE_FOR).then_some(*used)
}

fn store(data_dir: &Path, user_id: i64, used: u64) {
    if let Ok(mut cache) = CACHE.lock() {
        let key = (data_dir.to_path_buf(), user_id);
        let at = cache
            .get(&key)
            .filter(|(at, _)| at.elapsed() < CACHE_FOR)
            .map_or_else(Instant::now, |(at, _)| *at);
        cache.insert(key, (at, used));
    }
}
