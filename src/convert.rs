//! 升級到真實檔名：把舊版以隨機名稱存放的檔案（blob），放到硬碟上真實的資料夾與檔名。
//!
//! 每次啟動都會檢查，資料庫裡還有舊的 `legacy_files` 表才需要轉換。
//!
//! 步驟（任何時候中斷，下次啟動都會接著做完）：
//! 1. 把每個人舊的資料夾（`{硬碟}/{名稱}/blobs/…`）整個移到 `{硬碟}/.manorcloud/old/`，
//!    讓出位置給新的資料夾，也不會和名為 users 之類的舊資料夾混在一起
//! 2. 規劃每個檔案的新位置，寫進 `{資料目錄}/.manorcloud/convert-plan.json`。
//!    之後都照這份計畫做：重新啟動時不會因為「已經放好的檔案」而規劃出不同的名稱
//! 3. 照計畫把每個 blob 改名到新的位置（同一顆硬碟內只是改名，很快，也不需要額外的空間）
//! 4. 在一個交易中寫入垃圾桶的紀錄，並刪除舊的表
//! 5. 清掉舊資料夾中剩下的暫存檔
//!
//! 一般使用者的檔案放到 `{硬碟}/users/{名稱}/`；管理員（以及舊版的「管理員在某顆硬碟上的空間」）的檔案
//! 放到硬碟的最上層，同名時加上編號。垃圾桶中的項目放到 `{硬碟}/.manorcloud/trash/{編號}`。
//!
//! 有任何檔案搬不過去（權限不足、目的地已經有別的東西）就回傳錯誤，伺服器不會啟動，也不會刪除任何東西。
//! 硬碟上本來就找不到的 blob（舊版就已經遺失）記錄下來後略過。

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

use crate::{
    db::storage as storage_db,
    files::name::numbered_name,
    storage::{self, USERS_DIR},
    tree,
};

const PLAN_FILE: &str = "convert-plan.json";

#[derive(Debug, Clone, FromRow)]
struct Row {
    id: i64,
    owner_id: i64,
    parent_id: Option<i64>,
    name: String,
    kind: String,
    size: Option<i64>,
    storage_key: Option<String>,
    updated_at: i64,
    trashed_at: Option<i64>,
    trash_root: i64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct FileStep {
    blob: PathBuf,
    target: PathBuf,
    mtime: i64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct DirStep {
    path: PathBuf,
    mtime: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct TrashRow {
    id: i64,
    volume_id: Option<i64>,
    owner_id: Option<i64>,
    path: String,
    kind: String,
    size: i64,
    trashed_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Plan {
    dirs: Vec<DirStep>,
    files: Vec<FileStep>,
    trash: Vec<TrashRow>,
    /// 用到的硬碟：完成後整理它們的 .manorcloud/
    roots: Vec<PathBuf>,
}

/// 一個人的檔案要放到哪裡
struct Owner {
    id: i64,
    volume_id: Option<i64>,
    root: PathBuf,
    /// 舊的資料夾名稱
    folder: String,
    /// 一般使用者：Some（users/{名稱}）；管理員：None（硬碟的最上層）
    member: bool,
}

impl Owner {
    fn old_dir(&self) -> PathBuf {
        storage::system_dir(&self.root)
            .join("old")
            .join(&self.folder)
    }

    fn base(&self) -> PathBuf {
        if self.member {
            self.root.join(USERS_DIR).join(&self.folder)
        } else {
            self.root.clone()
        }
    }
}

/// 需要的話執行轉換。回傳錯誤時伺服器不應該啟動
pub async fn run(pool: &SqlitePool, data_dir: &Path) -> Result<(), String> {
    let pending: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'legacy_files')",
    )
    .fetch_one(pool)
    .await
    .map_err(db)?;
    if !pending {
        return Ok(());
    }

    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, owner_id, parent_id, name, kind, size, storage_key, updated_at, \
                trashed_at, trash_root FROM legacy_files ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;

    let plan_path = storage::system_dir(data_dir).join(PLAN_FILE);
    let plan = if rows.is_empty() {
        Plan::default()
    } else if plan_path.is_file() {
        let text = std::fs::read(&plan_path)
            .map_err(|e| format!("cannot read {}: {e}", plan_path.display()))?;
        serde_json::from_slice(&text)
            .map_err(|e| format!("cannot read {}: {e}", plan_path.display()))?
    } else {
        println!(
            "convert: moving {} files and folders to their real names",
            rows.len()
        );
        let owners = owners(pool, data_dir, &rows).await?;
        for owner in owners.values() {
            move_old_folder(owner)?;
        }
        let plan = make_plan(&owners, &rows)?;
        write_plan(&plan_path, &plan)?;
        plan
    };

    let (missing, plan) = tokio::task::spawn_blocking(move || execute(&plan).map(|m| (m, plan)))
        .await
        .map_err(|e| e.to_string())??;
    for blob in &missing {
        eprintln!(
            "convert: {} was already missing before the upgrade; skipped",
            blob.display()
        );
    }

    let mut tx = pool.begin().await.map_err(db)?;
    for row in &plan.trash {
        sqlx::query(
            "INSERT INTO trash (id, volume_id, owner_id, path, kind, size, trashed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.id)
        .bind(row.volume_id)
        .bind(row.owner_id)
        .bind(&row.path)
        .bind(&row.kind)
        .bind(row.size)
        .bind(row.trashed_at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    // 舊版「管理員在某顆硬碟上的空間」是隱藏的帳號：檔案已經放到硬碟的最上層，帳號不再需要
    sqlx::query("DELETE FROM users WHERE id IN (SELECT user_id FROM legacy_storage_spaces)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("DROP TABLE legacy_files")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("DROP TABLE legacy_storage_spaces")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;

    let _ = std::fs::remove_file(&plan_path);
    for root in &plan.roots {
        clean_old(root);
    }
    if !rows.is_empty() {
        println!("convert: done");
    }
    Ok(())
}

fn db(e: sqlx::Error) -> String {
    format!("database error: {e}")
}

/// 每個擁有檔案的人，以及他的檔案要放到哪一顆硬碟
async fn owners(
    pool: &SqlitePool,
    data_dir: &Path,
    rows: &[Row],
) -> Result<HashMap<i64, Owner>, String> {
    let used: HashSet<i64> = rows.iter().map(|r| r.owner_id).collect();
    let users: Vec<(i64, String, Option<i64>, String)> = sqlx::query_as(
        "SELECT id, role, storage_volume_id, COALESCE(storage_folder, username) FROM users",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let spaces: Vec<(i64, i64)> =
        sqlx::query_as("SELECT volume_key, user_id FROM legacy_storage_spaces")
            .fetch_all(pool)
            .await
            .map_err(db)?;
    let volumes = storage_db::list_volumes(pool).await.map_err(db)?;

    let mut owners = HashMap::new();
    for (id, role, volume_id, folder) in users {
        if !used.contains(&id) {
            continue;
        }
        let space = spaces.iter().find(|(_, user)| *user == id);
        let volume_id = match space {
            Some((0, _)) => None,
            Some((key, _)) => Some(*key),
            None => volume_id,
        };
        let root = match volume_id {
            None => data_dir.to_path_buf(),
            Some(vid) => {
                let volume = volumes
                    .iter()
                    .find(|v| v.id == vid)
                    .ok_or_else(|| format!("storage location {vid} no longer exists"))?;
                let root = PathBuf::from(&volume.path);
                if !storage::volume_ready(&root) {
                    return Err(format!(
                        "{} ({}) isn't available. Connect the disk and start ManorCloud again",
                        volume.name, volume.path
                    ));
                }
                root
            }
        };
        if !storage::is_valid_folder_name(&folder) {
            return Err(format!("invalid storage folder name {folder:?}"));
        }
        owners.insert(
            id,
            Owner {
                id,
                volume_id,
                root,
                folder,
                member: space.is_none() && role == "member",
            },
        );
    }
    Ok(owners)
}

/// 步驟 1：把舊的資料夾移到 .manorcloud/old/。已經移過的不再處理
fn move_old_folder(owner: &Owner) -> Result<(), String> {
    let from = owner.root.join(&owner.folder);
    let to = owner.old_dir();
    if to.exists() || !from.join("blobs").is_dir() {
        return Ok(());
    }
    let fail = |e: io::Error| format!("cannot move {} to {}: {e}", from.display(), to.display());
    std::fs::create_dir_all(to.parent().expect("old_dir has a parent")).map_err(fail)?;
    std::fs::rename(&from, &to).map_err(fail)
}

/// 步驟 2：規劃每個檔案的新位置
fn make_plan(owners: &HashMap<i64, Owner>, rows: &[Row]) -> Result<Plan, String> {
    let by_id: HashMap<i64, &Row> = rows.iter().map(|r| (r.id, r)).collect();
    let mut children: HashMap<(i64, Option<i64>), Vec<&Row>> = HashMap::new();
    for row in rows {
        children
            .entry((row.owner_id, row.parent_id))
            .or_default()
            .push(row);
    }

    let mut plan = Plan::default();
    // 每顆硬碟最上層已經被使用的名稱（不分大小寫）
    let mut taken: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    let mut ids: Vec<&i64> = owners.keys().collect();
    ids.sort();
    for id in ids {
        let owner = &owners[id];
        if !plan.roots.contains(&owner.root) {
            plan.roots.push(owner.root.clone());
        }
        let base = owner.base();
        if owner.member {
            if std::fs::read_dir(&base).is_ok_and(|mut items| items.next().is_some()) {
                return Err(format!(
                    "{} already exists and isn't empty. Move it away and start ManorCloud again",
                    base.display()
                ));
            }
            plan.dirs.push(DirStep {
                path: base.clone(),
                mtime: None,
            });
        }
        let names = taken
            .entry(owner.root.clone())
            .or_insert_with(|| existing_names(&owner.root));

        // 最上層的名稱：管理員的檔案放在硬碟的最上層，同名時加上編號
        let mut top: HashMap<i64, String> = HashMap::new();
        let mut roots: Vec<&Row> = children.get(&(owner.id, None)).cloned().unwrap_or_default();
        roots.sort_by_key(|r| r.id);
        for row in &roots {
            let name = if owner.member {
                row.name.clone()
            } else {
                free_name(names, &row.name)
            };
            top.insert(row.id, name);
        }

        // 不在垃圾桶的檔案
        for row in roots.iter().filter(|r| r.trashed_at.is_none()) {
            let path = base.join(&top[&row.id]);
            add_tree(&mut plan, owner, &children, row, path, None);
        }

        // 垃圾桶：每一次刪除的那一個項目，連同同一次一起被刪除的內容
        for row in rows
            .iter()
            .filter(|r| r.owner_id == owner.id && r.trash_root == 1)
        {
            let Some(trashed_at) = row.trashed_at else {
                continue;
            };
            let target = storage::trash_path(&owner.root, row.id);
            let size = add_tree(&mut plan, owner, &children, row, target, Some(trashed_at));
            plan.trash.push(TrashRow {
                id: row.id,
                volume_id: owner.volume_id,
                owner_id: owner.member.then_some(owner.id),
                path: original_path(&by_id, &top, row),
                kind: row.kind.clone(),
                size,
                trashed_at,
            });
        }
    }
    Ok(plan)
}

/// 硬碟最上層已經有的名稱，以及保留的名稱
fn existing_names(root: &Path) -> HashSet<String> {
    let mut names: HashSet<String> = std::fs::read_dir(root)
        .map(|items| {
            items
                .filter_map(Result::ok)
                .filter_map(|item| item.file_name().into_string().ok())
                .filter(|name| !tree::is_system_name(name))
                .map(|name| name.to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    names.insert(USERS_DIR.to_string());
    names.insert("lost+found".to_string());
    names
}

fn free_name(taken: &mut HashSet<String>, name: &str) -> String {
    let mut candidate = name.to_string();
    let mut n = 0;
    while taken.contains(&candidate.to_lowercase()) {
        n += 1;
        candidate = numbered_name(name, n);
    }
    taken.insert(candidate.to_lowercase());
    candidate
}

/// 規劃一個項目與它底下的內容，回傳檔案大小的總和。
/// `batch` 是垃圾桶中同一次被刪除的時間：只包含同一次刪除的內容；None 是不在垃圾桶的內容
fn add_tree(
    plan: &mut Plan,
    owner: &Owner,
    children: &HashMap<(i64, Option<i64>), Vec<&Row>>,
    row: &Row,
    path: PathBuf,
    batch: Option<i64>,
) -> i64 {
    if row.kind == "folder" {
        plan.dirs.push(DirStep {
            path: path.clone(),
            mtime: Some(row.updated_at),
        });
        let mut total = 0;
        for child in children
            .get(&(owner.id, Some(row.id)))
            .into_iter()
            .flatten()
        {
            let belongs = match batch {
                None => child.trashed_at.is_none(),
                Some(at) => child.trash_root == 0 && child.trashed_at == Some(at),
            };
            if belongs {
                total += add_tree(plan, owner, children, child, path.join(&child.name), batch);
            }
        }
        total
    } else {
        let Some(key) = row
            .storage_key
            .as_deref()
            .filter(|k| storage::is_valid_key(k))
        else {
            return 0;
        };
        plan.files.push(FileStep {
            blob: owner.old_dir().join("blobs").join(&key[..2]).join(key),
            target: path,
            mtime: row.updated_at,
        });
        row.size.unwrap_or(0)
    }
}

/// 垃圾桶中的項目原本在哪裡：從最上層一路往下的名稱
fn original_path(by_id: &HashMap<i64, &Row>, top: &HashMap<i64, String>, row: &Row) -> String {
    let mut names = Vec::new();
    let mut current = Some(row);
    while let Some(item) = current {
        match item.parent_id {
            None => names.push(top.get(&item.id).cloned().unwrap_or(item.name.clone())),
            Some(_) => names.push(item.name.clone()),
        }
        current = item.parent_id.and_then(|p| by_id.get(&p).copied());
    }
    names.reverse();
    names.join("/")
}

fn write_plan(path: &Path, plan: &Plan) -> Result<(), String> {
    let fail = |e: io::Error| format!("cannot write {}: {e}", path.display());
    std::fs::create_dir_all(path.parent().expect("the plan has a parent")).map_err(fail)?;
    let temp = path.with_extension("tmp");
    let text = serde_json::to_vec(plan).map_err(|e| e.to_string())?;
    std::fs::write(&temp, text).map_err(fail)?;
    std::fs::File::open(&temp)
        .and_then(|f| f.sync_all())
        .map_err(fail)?;
    std::fs::rename(&temp, path).map_err(fail)
}

/// 步驟 3：照計畫放好每個檔案。回傳找不到的 blob
fn execute(plan: &Plan) -> Result<Vec<PathBuf>, String> {
    for dir in &plan.dirs {
        std::fs::create_dir_all(&dir.path)
            .map_err(|e| format!("cannot create {}: {e}", dir.path.display()))?;
    }
    let mut missing = Vec::new();
    for step in &plan.files {
        let blob_here = std::fs::symlink_metadata(&step.blob).is_ok();
        let target_here = std::fs::symlink_metadata(&step.target).is_ok();
        match (blob_here, target_here) {
            // 上一次已經放好
            (false, true) => {}
            (false, false) => {
                missing.push(step.blob.clone());
                continue;
            }
            (true, true) => {
                return Err(format!(
                    "{} already exists. Move it away and start ManorCloud again",
                    step.target.display()
                ));
            }
            (true, false) => {
                if let Some(parent) = step.target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                std::fs::rename(&step.blob, &step.target).map_err(|e| {
                    format!(
                        "cannot move {} to {}: {e}",
                        step.blob.display(),
                        step.target.display()
                    )
                })?;
            }
        }
        tree::set_modified(&step.target, tree::unix_time(step.mtime));
    }
    // 放進檔案會改變資料夾的修改時間：最後才設定，由深到淺
    let mut dirs: Vec<&DirStep> = plan.dirs.iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.path.components().count()));
    for dir in dirs {
        if let Some(mtime) = dir.mtime {
            tree::set_modified(&dir.path, tree::unix_time(mtime));
        }
    }
    Ok(missing)
}

/// 步驟 5：舊資料夾中只剩下暫存檔與空的資料夾時整個刪除；還有 blob 時留著，並記錄下來
fn clean_old(root: &Path) {
    let old = storage::system_dir(root).join("old");
    if !old.exists() {
        let _ = storage::init_dirs(root);
        return;
    }
    let mut left = 0usize;
    if let Ok(folders) = std::fs::read_dir(&old) {
        for folder in folders.filter_map(Result::ok) {
            if let Ok(shards) = std::fs::read_dir(folder.path().join("blobs")) {
                for shard in shards.filter_map(Result::ok) {
                    left += std::fs::read_dir(shard.path()).map_or(0, |items| items.count());
                }
            }
        }
    }
    if left == 0 {
        if let Err(e) = std::fs::remove_dir_all(&old) {
            eprintln!("convert: cannot remove {}: {e}", old.display());
        }
    } else {
        eprintln!(
            "convert: {left} unused old files are kept in {}. \
             They weren't part of any file; you can delete the folder",
            old.display()
        );
    }
    if let Err(e) = storage::init_dirs(root) {
        eprintln!("convert: cannot prepare {}: {e}", root.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    /// 在舊的位置放一個 blob
    fn blob(root: &Path, folder: &str, n: u32, content: &[u8]) {
        let k = key(n);
        let dir = root.join(folder).join("blobs").join(&k[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&k), content).unwrap();
    }

    async fn row(
        pool: &SqlitePool,
        id: i64,
        owner: i64,
        parent: Option<i64>,
        name: &str,
        file: Option<u32>,
        trashed: Option<(i64, bool)>,
    ) {
        sqlx::query(
            "INSERT INTO legacy_files (id, owner_id, parent_id, name, kind, size, storage_key, \
                                       updated_at, trashed_at, trash_root) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 1700000000, ?, ?)",
        )
        .bind(id)
        .bind(owner)
        .bind(parent)
        .bind(name)
        .bind(if file.is_some() { "file" } else { "folder" })
        .bind(file.map(|_| 1i64))
        .bind(file.map(key))
        .bind(trashed.map(|(at, _)| at))
        .bind(trashed.is_some_and(|(_, root)| root))
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    async fn files_get_their_real_names(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        let admin = user::create_user(&pool, "admin", "x").await.unwrap();
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(admin)
            .execute(&pool)
            .await
            .unwrap();
        let jerry = user::create_user(&pool, "jerry", "x").await.unwrap();
        // 一個叫 users 的使用者：他舊的資料夾就叫 users
        let users = user::create_user(&pool, "users", "x").await.unwrap();

        // 管理員：相片/a.jpg，以及一個叫 users 的檔案（和保留的名稱同名）
        row(&pool, 1, admin, None, "相片", None, None).await;
        row(&pool, 2, admin, Some(1), "a.jpg", Some(1), None).await;
        row(&pool, 3, admin, None, "users", Some(2), None).await;
        blob(root, "admin", 1, b"a");
        blob(root, "admin", 2, b"u");
        // jerry：docs/b.txt 在垃圾桶裡（刪除的是 docs），c.txt 還在
        row(
            &pool,
            4,
            jerry,
            None,
            "docs",
            None,
            Some((1800000000, true)),
        )
        .await;
        row(
            &pool,
            5,
            jerry,
            Some(4),
            "b.txt",
            Some(3),
            Some((1800000000, false)),
        )
        .await;
        row(&pool, 6, jerry, None, "c.txt", Some(4), None).await;
        blob(root, "jerry", 3, b"b");
        blob(root, "jerry", 4, b"c");
        row(&pool, 7, users, None, "d.txt", Some(5), None).await;
        blob(root, "users", 5, b"d");
        // 舊版就已經遺失的 blob
        row(&pool, 8, jerry, None, "gone.txt", Some(6), None).await;

        run(&pool, root).await.unwrap();

        assert_eq!(std::fs::read(root.join("相片/a.jpg")).unwrap(), b"a");
        assert_eq!(std::fs::read(root.join("users (1)")).unwrap(), b"u");
        assert_eq!(std::fs::read(root.join("users/jerry/c.txt")).unwrap(), b"c");
        assert_eq!(std::fs::read(root.join("users/users/d.txt")).unwrap(), b"d");
        assert_eq!(
            std::fs::read(root.join(".manorcloud/trash/4/b.txt")).unwrap(),
            b"b"
        );
        let modified = std::fs::metadata(root.join("相片/a.jpg"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(modified, tree::unix_time(1700000000));

        let trash: Vec<(i64, Option<i64>, String, i64)> =
            sqlx::query_as("SELECT id, owner_id, path, size FROM trash")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(trash, [(4, Some(jerry), "docs".to_string(), 1)]);

        // 舊的資料夾與表都清掉了；再執行一次什麼都不做
        assert!(!root.join(".manorcloud/old").exists());
        assert!(!root.join(".manorcloud").join(PLAN_FILE).exists());
        run(&pool, root).await.unwrap();
    }

    #[sqlx::test]
    async fn an_interrupted_conversion_continues_from_the_plan(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        let jerry = user::create_user(&pool, "jerry", "x").await.unwrap();
        row(&pool, 1, jerry, None, "a.txt", Some(1), None).await;
        row(&pool, 2, jerry, None, "b.txt", Some(2), None).await;
        blob(root, "jerry", 1, b"a");
        blob(root, "jerry", 2, b"b");

        // 模擬中斷：計畫已經寫好，第一個檔案已經放好
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, owner_id, parent_id, name, kind, size, storage_key, updated_at, \
                    trashed_at, trash_root FROM legacy_files",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let owners = owners(&pool, root, &rows).await.unwrap();
        move_old_folder(&owners[&jerry]).unwrap();
        let plan = make_plan(&owners, &rows).unwrap();
        write_plan(&storage::system_dir(root).join(PLAN_FILE), &plan).unwrap();
        std::fs::create_dir_all(root.join("users/jerry")).unwrap();
        std::fs::rename(&plan.files[0].blob, &plan.files[0].target).unwrap();

        run(&pool, root).await.unwrap();

        assert_eq!(std::fs::read(root.join("users/jerry/a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(root.join("users/jerry/b.txt")).unwrap(), b"b");
    }

    #[sqlx::test]
    async fn something_in_the_way_stops_the_conversion(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let root = data.path();
        let jerry = user::create_user(&pool, "jerry", "x").await.unwrap();
        row(&pool, 1, jerry, None, "a.txt", Some(1), None).await;
        blob(root, "jerry", 1, b"a");
        std::fs::create_dir_all(root.join("users/jerry")).unwrap();
        std::fs::write(root.join("users/jerry/other"), b"x").unwrap();

        assert!(run(&pool, root).await.is_err());
        // 什麼都沒有刪：舊的表還在
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM legacy_files")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }
}
