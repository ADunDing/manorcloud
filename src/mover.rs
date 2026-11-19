//! 更換一般使用者的儲存位置時，在背景把他的檔案搬過去。
//!
//! 1. 把 `{原本的硬碟}/users/{名稱}` 與他在垃圾桶中的項目，複製到目的地的 `.manorcloud/tmp/move-{編號}/`
//! 2. 全部複製完，改名到正式的位置（同一顆硬碟內，一瞬間完成）
//! 3. 在一個交易中把使用者換到新位置（垃圾桶的紀錄也一起），並刪除搬移紀錄
//! 4. 刪除原本位置的資料夾與垃圾桶中的項目
//!
//! 任何時候中斷都安全：
//! - 第 3 步之前：使用者仍在原本的位置，檔案照常讀取。重新啟動後從頭複製（暫存的部分丟掉重來）
//! - 第 3 步之後：原本位置留下的資料夾在下次搬移、或管理員手動清除前都還在，不影響任何人
//!
//! 搬移期間他的 My files 是唯讀的（見 `crate::space`），複製到一半時檔案不會改變。

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::db::storage::{self as storage_db, Finish};
use crate::db::trash as trash_db;
use crate::{folders, storage, tree};

/// 進度多久寫進資料庫一次。畫面每兩秒讀一次，寫得更頻繁沒有意義
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// 在背景執行一筆搬移
pub fn spawn(pool: SqlitePool, data_dir: Arc<Path>, move_id: i64) {
    tokio::spawn(async move {
        if let Err(e) = run(&pool, &data_dir, move_id).await {
            eprintln!("storage move {move_id}: failed: {e}");
            if let Err(db) = storage_db::fail_move(&pool, move_id, &e).await {
                eprintln!("storage move {move_id}: cannot record the failure: {db}");
            }
        }
    });
}

/// 伺服器啟動時，接著做完上次沒有做完的搬移
pub async fn resume_all(pool: &SqlitePool, data_dir: &Arc<Path>) {
    match storage_db::running_moves(pool).await {
        Ok(ids) => {
            for id in ids {
                println!("storage move {id}: resuming");
                spawn(pool.clone(), Arc::clone(data_dir), id);
            }
        }
        Err(e) => eprintln!("cannot read the unfinished storage moves: {e}"),
    }
}

/// 搬移一位使用者的檔案。回傳的錯誤是給管理員看的訊息，會記錄在搬移紀錄中
async fn run(pool: &SqlitePool, data_dir: &Path, move_id: i64) -> Result<(), String> {
    let db = |e: sqlx::Error| format!("database error: {e}");

    let Some(record) = storage_db::find_move(pool, move_id).await.map_err(db)? else {
        return Ok(());
    };
    if record.error.is_some() {
        return Ok(());
    }
    let user_id = record.user_id;
    let here = storage_db::user_location(pool, user_id)
        .await
        .map_err(db)?
        .ok_or("the user no longer exists")?;
    let from_root = folders::volume_root(pool, data_dir, here.volume_id).await?;
    let to_root = folders::volume_root(pool, data_dir, record.target_volume_id).await?;
    for root in [&from_root, &to_root] {
        if root.as_path() != data_dir && !storage::volume_ready(root) {
            return Err(format!(
                "{} isn't available. Connect the disk and retry",
                root.display()
            ));
        }
    }
    let source = storage::user_dir(&from_root, &here.folder).ok_or("invalid folder name")?;
    let target = storage::user_dir(&to_root, &here.folder).ok_or("invalid folder name")?;
    let trash = trash_db::of_owner(pool, user_id).await.map_err(db)?;
    let staging = storage::tmp_dir(&to_root).join(format!("move-{move_id}"));

    // 1. 在背景執行緒複製；這裡每隔一段時間更新進度，並確認搬移還沒被取消
    let progress = Arc::new(tree::Progress::default());
    let copy = tokio::task::spawn_blocking({
        let progress = Arc::clone(&progress);
        let (source, staging, from_root) = (source.clone(), staging.clone(), from_root.clone());
        let ids: Vec<i64> = trash.iter().map(|item| item.id).collect();
        move || -> io::Result<()> {
            if staging.exists() {
                tree::remove_path(&staging)?;
            }
            std::fs::create_dir_all(staging.join("trash"))?;
            if source.is_dir() {
                tree::copy_tracked(&source, &staging.join("home"), &progress)?;
            } else {
                std::fs::create_dir(staging.join("home"))?;
            }
            for id in ids {
                let item = storage::trash_path(&from_root, id);
                if std::fs::symlink_metadata(&item).is_ok() {
                    tree::copy_tracked(
                        &item,
                        &staging.join("trash").join(id.to_string()),
                        &progress,
                    )?;
                }
            }
            Ok(())
        }
    });
    tokio::pin!(copy);
    let result = loop {
        tokio::select! {
            result = &mut copy => break result.map_err(|e| e.to_string())?,
            () = tokio::time::sleep(PROGRESS_INTERVAL) => {
                let moved = i64::try_from(progress.copied.load(Ordering::Relaxed)).unwrap_or(i64::MAX);
                if !storage_db::set_progress(pool, move_id, moved).await.map_err(db)? {
                    progress.stop.store(true, Ordering::Relaxed);
                }
            }
        }
    };
    if progress.stop.load(Ordering::Relaxed) || !still_running(pool, move_id).await? {
        remove_quietly(staging).await;
        return Ok(());
    }
    if let Err(e) = result {
        remove_quietly(staging).await;
        return Err(match e.kind() {
            io::ErrorKind::StorageFull => "the storage location is full".to_string(),
            _ => format!("cannot copy the files: {e}"),
        });
    }

    // 2. 放到正式的位置
    let placed = tokio::task::spawn_blocking({
        let (staging, target, to_root) = (staging.clone(), target.clone(), to_root.clone());
        let ids: Vec<i64> = trash.iter().map(|item| item.id).collect();
        move || place(&staging, &target, &to_root, &ids)
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Err(e) = placed {
        remove_quietly(staging).await;
        return Err(format!("cannot put the files in place: {e}"));
    }

    // 3. 切換。這一刻之前被取消的話，把剛放好的複本收回
    match storage_db::finish_move(pool, move_id).await.map_err(db)? {
        Finish::Done => {}
        Finish::Cancelled => {
            remove_quietly(target).await;
            for item in &trash {
                remove_quietly(storage::trash_path(&to_root, item.id)).await;
            }
            remove_quietly(staging).await;
            return Ok(());
        }
    }
    remove_quietly(staging).await;

    // 4. 刪除原本位置的檔案
    remove_quietly(source).await;
    for item in &trash {
        remove_quietly(storage::trash_path(&from_root, item.id)).await;
    }
    println!(
        "storage move {move_id}: user {user_id} is now on {}",
        to_root.display()
    );
    // 搬移期間帳號被改名：資料夾在這裡才跟著改名
    folders::sync_name(pool, data_dir, user_id).await;
    Ok(())
}

async fn still_running(pool: &SqlitePool, move_id: i64) -> Result<bool, String> {
    Ok(storage_db::find_move(pool, move_id)
        .await
        .map_err(|e| format!("database error: {e}"))?
        .is_some_and(|m| m.error.is_none()))
}

/// 把暫存的複本放到正式的位置：My files 與垃圾桶中的每個項目
fn place(staging: &Path, target: &Path, to_root: &Path, trash: &[i64]) -> io::Result<()> {
    // 目的地已經有一個空的資料夾（例如先前換過來又換走）：可以取代；有東西就不行
    if target.exists() {
        std::fs::remove_dir(target).map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists and isn't empty", target.display()),
            )
        })?;
    }
    std::fs::create_dir_all(target.parent().expect("user_dir has a parent"))?;
    std::fs::rename(staging.join("home"), target)?;
    std::fs::create_dir_all(storage::trash_dir(to_root))?;
    for id in trash {
        let from = staging.join("trash").join(id.to_string());
        if std::fs::symlink_metadata(&from).is_ok() {
            std::fs::rename(from, storage::trash_path(to_root, *id))?;
        }
    }
    Ok(())
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
        eprintln!("storage move: {e}");
    }
}
