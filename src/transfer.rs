//! 跨硬碟的複製與移動，在背景一個項目接著一個項目進行。
//!
//! 每個項目：
//! 1. 複製到目的地硬碟的 `.manorcloud/tmp/transfer-{編號}/`
//! 2. 完整複製之後放到目的地資料夾（同名時自動加上編號）
//! 3. 移動：先記下「已經放好」，再刪除來源。來源在複製期間被改變（大小不同）時保留來源，並說明原因
//!
//! 中斷都是安全的：重新啟動時，已經完成的項目跳過；放好但還沒刪除來源的項目只刪除來源；
//! 正在複製的項目丟掉暫存，重新複製。取消之後，已經完成的項目留在目的地。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::{
    db::transfers::{self as transfers_db, Transfer},
    space::{self, Space, SpaceKey},
    storage,
    tree::{self, OnConflict, RelPath},
};

/// 進度多久寫進資料庫一次
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// 在背景執行一筆傳輸
pub fn spawn(pool: SqlitePool, data_dir: Arc<Path>, id: i64) {
    tokio::spawn(async move {
        if let Err(e) = run(&pool, &data_dir, id).await {
            eprintln!("transfer {id}: failed: {e}");
            if let Err(db) = transfers_db::finish(&pool, id, "failed", Some(&e)).await {
                eprintln!("transfer {id}: cannot record the failure: {db}");
            }
        }
    });
}

/// 伺服器啟動時，接著做完上次沒有做完的傳輸
pub async fn resume_all(pool: &SqlitePool, data_dir: &Arc<Path>) {
    match transfers_db::running(pool).await {
        Ok(ids) => {
            for id in ids {
                println!("transfer {id}: resuming");
                spawn(pool.clone(), Arc::clone(data_dir), id);
            }
        }
        Err(e) => eprintln!("cannot read the unfinished transfers: {e}"),
    }
}

/// 兩個空間，以及來源、目的地資料夾（建立時與每次執行時都用它再檢查一次權限）
pub struct Plan {
    pub from: Space,
    pub to: Space,
    pub from_dir: RelPath,
    pub to_dir: RelPath,
}

/// 找出兩個空間並確認權限：目的地可以寫入；移動時，每個來源項目都可以刪除；
/// 而且不能放進自己裡面（兩個空間在同一顆硬碟時才可能發生）
pub async fn plan(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    admin: bool,
    t: &TransferSpec<'_>,
) -> Result<Plan, PlanError> {
    let key = |text: &str| SpaceKey::parse(Some(text)).ok_or(PlanError::NotFound);
    let from = space::resolve(pool, data_dir, user_id, admin, key(t.from_space)?).await?;
    let to = space::resolve(pool, data_dir, user_id, admin, key(t.to_space)?).await?;
    let path =
        |text: &str| RelPath::parse(text).map_err(|_| PlanError::Invalid("Invalid path".into()));
    let (from_dir, to_dir) = (path(t.from_dir)?, path(t.to_dir)?);

    to.check_write_in(&to_dir).map_err(PlanError::Denied)?;
    let same_disk = from.root == to.root;
    for name in t.names {
        let item = from_dir.child(name);
        if t.kind == "move" {
            from.check_modify(&item).map_err(PlanError::Denied)?;
        }
        if same_disk && to.abs(&to_dir).starts_with(&from.abs(&item)) {
            return Err(PlanError::Invalid(
                "A folder can't be copied or moved into itself".into(),
            ));
        }
    }
    Ok(Plan {
        from,
        to,
        from_dir,
        to_dir,
    })
}

/// 一筆傳輸要做什麼
pub struct TransferSpec<'a> {
    pub kind: &'a str,
    pub from_space: &'a str,
    pub from_dir: &'a str,
    pub names: &'a [String],
    pub to_space: &'a str,
    pub to_dir: &'a str,
}

#[derive(Debug)]
pub enum PlanError {
    NotFound,
    Invalid(String),
    Denied(String),
    Space(space::SpaceError),
}

impl From<space::SpaceError> for PlanError {
    fn from(e: space::SpaceError) -> Self {
        PlanError::Space(e)
    }
}

impl PlanError {
    fn message(&self) -> String {
        match self {
            PlanError::NotFound => "the folder is no longer available to you".to_string(),
            PlanError::Invalid(m) | PlanError::Denied(m) => m.clone(),
            PlanError::Space(space::SpaceError::Unavailable(m)) => m.clone(),
            PlanError::Space(e) => format!("{e:?}"),
        }
    }
}

async fn run(pool: &SqlitePool, data_dir: &Path, id: i64) -> Result<(), String> {
    let db = |e: sqlx::Error| format!("database error: {e}");
    let Some(t) = transfers_db::find(pool, id).await.map_err(db)? else {
        return Ok(());
    };
    if t.state != "running" {
        return Ok(());
    }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(t.owner_id)
        .fetch_optional(pool)
        .await
        .map_err(db)?;
    let names: Vec<String> = serde_json::from_str(&t.names).map_err(|e| e.to_string())?;
    let spec = TransferSpec {
        kind: &t.kind,
        from_space: &t.from_space,
        from_dir: &t.from_dir,
        names: &names,
        to_space: &t.to_space,
        to_dir: &t.to_dir,
    };
    let plan = plan(
        pool,
        data_dir,
        t.owner_id,
        role.as_deref() == Some("admin"),
        &spec,
    )
    .await
    .map_err(|e| e.message())?;

    let staging = storage::tmp_dir(&plan.to.root).join(format!("transfer-{id}"));
    remove_quietly(staging.clone()).await;
    let mut skipped: Vec<String> = serde_json::from_str(&t.skipped).unwrap_or_default();
    let mut done_bytes = t.done_bytes;

    let start = usize::try_from(t.done_items).unwrap_or(0);
    for (index, name) in names.iter().enumerate().skip(start) {
        let rel = plan.from.abs(&plan.from_dir.child(name));
        let root = plan.from.root.clone();
        let source = tokio::task::spawn_blocking(move || tree::resolve(&root, &rel, true))
            .await
            .map_err(|e| e.to_string())?;

        // 重新啟動：這個項目已經放到目的地，只剩刪除來源
        if t.placed && index == start {
            if let Ok(source) = source {
                remove_quietly(source).await;
            }
            if !transfers_db::finish_item(pool, id, done_bytes, &skipped)
                .await
                .map_err(db)?
            {
                return Ok(());
            }
            continue;
        }

        let source = match source {
            Ok(source) => source,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                skipped.push(format!("{name}: it no longer exists"));
                if !transfers_db::finish_item(pool, id, done_bytes, &skipped)
                    .await
                    .map_err(db)?
                {
                    return Ok(());
                }
                continue;
            }
            Err(e) => return Err(format!("cannot open {name}: {e}")),
        };

        // 1. 複製到暫存，一邊回報進度
        let Some(copy) = copy_item(pool, id, &source, &staging, name, done_bytes).await? else {
            remove_quietly(staging).await;
            return Ok(());
        };
        skipped.extend(copy.skipped);

        // 2. 放到目的地
        let dest_rel = plan.to.abs(&plan.to_dir);
        let to_root = plan.to.root.clone();
        let staged = staging.join(name);
        let placed_name = name.clone();
        let placed = tokio::task::spawn_blocking(move || {
            let dir = tree::resolve_dir(&to_root, &dest_rel)?;
            tree::place(&staged, &dir, &placed_name, OnConflict::Rename)
        })
        .await
        .map_err(|e| e.to_string())?;
        if let Err(e) = placed {
            remove_quietly(staging).await;
            return Err(match e.kind() {
                io::ErrorKind::NotFound => "the destination folder no longer exists".to_string(),
                _ => format!("cannot put {name} in place: {e}"),
            });
        }
        done_bytes += copy.bytes;

        // 3. 移動：來源沒有在複製期間改變，才刪除它
        if t.kind == "move" {
            let measured = source.clone();
            let now = tokio::task::spawn_blocking(move || tree::tree_size(&measured))
                .await
                .map_err(|e| e.to_string())?;
            if now == copy.source_size {
                if !transfers_db::set_placed(pool, id).await.map_err(db)? {
                    return Ok(());
                }
                remove_quietly(source).await;
            } else {
                skipped.push(format!(
                    "{name}: it changed while being moved, so the original was kept"
                ));
            }
        }
        if !transfers_db::finish_item(pool, id, done_bytes, &skipped)
            .await
            .map_err(db)?
        {
            return Ok(());
        }
    }

    remove_quietly(staging).await;
    transfers_db::finish(pool, id, "done", None)
        .await
        .map_err(db)?;
    println!("transfer {id}: done");
    Ok(())
}

struct Copied {
    /// 複製的位元組數
    bytes: i64,
    /// 開始複製前來源的大小（移動時用來確認來源沒有改變）
    source_size: u64,
    skipped: Vec<String>,
}

/// 把一個項目複製到暫存資料夾。被取消時回傳 None
async fn copy_item(
    pool: &SqlitePool,
    id: i64,
    source: &Path,
    staging: &Path,
    name: &str,
    done_before: i64,
) -> Result<Option<Copied>, String> {
    let progress = Arc::new(tree::Progress::default());
    let task = tokio::task::spawn_blocking({
        let (progress, source, target) = (
            Arc::clone(&progress),
            source.to_path_buf(),
            staging.join(name),
        );
        let staging = staging.to_path_buf();
        move || -> io::Result<u64> {
            std::fs::create_dir_all(&staging)?;
            let size = tree::tree_size(&source);
            if size > storage::available_bytes(&staging)? {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "not enough space",
                ));
            }
            tree::copy_tracked(&source, &target, &progress)?;
            Ok(size)
        }
    });
    tokio::pin!(task);
    let result = loop {
        tokio::select! {
            result = &mut task => break result.map_err(|e| e.to_string())?,
            () = tokio::time::sleep(PROGRESS_INTERVAL) => {
                let copied = i64::try_from(progress.copied.load(Ordering::Relaxed)).unwrap_or(0);
                let alive = transfers_db::set_progress(pool, id, done_before + copied)
                    .await
                    .map_err(|e| format!("database error: {e}"))?;
                if !alive {
                    progress.stop.store(true, Ordering::Relaxed);
                }
            }
        }
    };
    if progress.stop.load(Ordering::Relaxed) || !still_running(pool, id).await? {
        return Ok(None);
    }
    let source_size = result.map_err(|e| match e.kind() {
        io::ErrorKind::StorageFull => {
            "there isn't enough free space on the destination".to_string()
        }
        _ => format!("cannot copy {name}: {e}"),
    })?;
    let skipped = progress
        .skipped
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|path| format!("{path}: not a regular file or folder (for example a link)"))
        .collect();
    Ok(Some(Copied {
        bytes: i64::try_from(progress.copied.load(Ordering::Relaxed)).unwrap_or(0),
        source_size,
        skipped,
    }))
}

async fn still_running(pool: &SqlitePool, id: i64) -> Result<bool, String> {
    Ok(transfers_db::find(pool, id)
        .await
        .map_err(|e| format!("database error: {e}"))?
        .is_some_and(|t: Transfer| t.state == "running"))
}

async fn remove_quietly(path: PathBuf) {
    let result = tokio::task::spawn_blocking(move || match tree::remove_path(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {e}", path.display()))
        }
        _ => Ok(()),
    })
    .await;
    if let Ok(Err(e)) = result {
        eprintln!("transfer: {e}");
    }
}
