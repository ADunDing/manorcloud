//! 更換使用者的儲存位置時，在背景把檔案搬過去。
//!
//! 順序與「先 blob、後紀錄」的不變式相同：
//! 1. 把每個 blob 複製到新位置（保留同一個 storage_key）
//! 2. 全部複製完，在一個交易中把使用者換到新位置、刪除搬移紀錄
//! 3. 刪除原本位置的 blob
//!
//! 任何時候中斷都安全：
//! - 第 2 步之前：使用者仍在原本的位置，檔案照常讀取。重新啟動後接著複製，
//!   已經複製好的會被略過。複製到一半的只在新位置的 tmp/ 中，由垃圾回收清除
//! - 第 2 步之後：原本位置留下的 blob 沒有紀錄參照，由垃圾回收清除
//!
//! 搬移期間不能新增檔案（上傳、複製），見 web::storage::writable_root。
//! 刪除照常可以做：被刪掉的檔案在第 1 步會因為找不到而略過。

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::SqlitePool;

use crate::db::storage::{self as storage_db, Finish};
use crate::folders;
use crate::storage::{self, StorageError};

/// 進度最多多久寫進資料庫一次。畫面每兩秒讀一次，寫得更頻繁沒有意義
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

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
        // 已經被取消
        return Ok(());
    };
    let Some(here) = storage_db::user_location(pool, record.user_id)
        .await
        .map_err(db)?
    else {
        return Ok(());
    };
    // 兩邊的硬碟都必須有掛載（資料目錄沒有標記檔）
    let target = folders::volume_root(pool, data_dir, record.target_volume_id).await?;
    let source = here
        .path
        .as_deref()
        .map_or_else(|| data_dir.to_path_buf(), PathBuf::from);
    for (root, volume) in [
        (&source, here.volume_id),
        (&target, record.target_volume_id),
    ] {
        if volume.is_some() && !storage::volume_ready(root) {
            return Err(format!(
                "{} isn't available. Is the disk connected?",
                root.display()
            ));
        }
    }
    // 兩邊都是這個人自己的資料夾：{位置}/{資料夾名稱}
    let from = folders::user_root(data_dir, &here)?;
    let to = storage::user_root(&target, &here.folder)
        .ok_or_else(|| format!("invalid storage folder name {:?}", here.folder))?;

    // 新位置的 tmp/、blobs/、uploads/
    {
        let dir = to.clone();
        tokio::task::spawn_blocking(move || storage::init_dirs(&dir))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("cannot prepare {}: {e}", to.display()))?;
    }

    // 1. 複製。完成時若發現還有沒複製到的檔案（搬移開始的那一刻剛好完成的上傳），
    //    把它們也複製過去再試一次。新的上傳已經被擋下，最多多跑一兩輪
    let mut pending = storage_db::move_keys(pool, record.user_id)
        .await
        .map_err(db)?;
    let mut copied: HashSet<String> = HashSet::new();
    let mut moved: i64 = 0;
    let mut reported = Instant::now();
    loop {
        for (key, size) in &pending {
            match storage::copy_blob_to(&from, key, &to).await {
                Ok(_) => {}
                // 搬移期間被永久刪除了：不需要搬
                Err(StorageError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot copy a file to {}: {e}", to.display())),
            }
            copied.insert(key.clone());
            moved = moved.saturating_add(*size);

            if reported.elapsed() >= PROGRESS_INTERVAL {
                reported = Instant::now();
                if !storage_db::set_progress(pool, move_id, moved)
                    .await
                    .map_err(db)?
                {
                    // 被取消，或使用者被刪除：已經複製的部分由垃圾回收清除
                    println!("storage move {move_id}: cancelled");
                    return Ok(());
                }
            }
        }

        // 2. 換位置
        match storage_db::finish_move(pool, move_id, &copied)
            .await
            .map_err(db)?
        {
            Finish::Done => break,
            Finish::Cancelled => {
                println!("storage move {move_id}: cancelled");
                return Ok(());
            }
            Finish::Missing(more) => pending = more,
        }
    }
    println!(
        "storage move {move_id}: moved {} files of user {} to {}",
        copied.len(),
        record.user_id,
        to.display()
    );

    // 3. 刪除原本位置的資料夾。失敗的留在原處（沒有人參照，只是佔空間）
    folders::remove_user_folder(&from).await;

    // 搬移期間帳號改了名：現在可以讓資料夾跟著改名了
    folders::sync_name(pool, data_dir, record.user_id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{files, user};
    use crate::storage::TempBlob;

    /// 存進 carol 在這個位置中的資料夾
    async fn store(root: &Path, key: &str, content: &[u8]) {
        let root = root.join("carol");
        storage::init_dirs(&root).unwrap();
        let root = root.as_path();
        let mut blob = TempBlob::create(root, u64::MAX).await.unwrap();
        blob.write_chunk(content).await.unwrap();
        blob.persist(key).await.unwrap();
    }

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    #[sqlx::test]
    async fn moves_every_blob_and_switches_the_user(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let volume = storage_db::add_volume(&pool, "disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        for n in 1..=3 {
            store(data.path(), &key(n), b"abc").await;
            files::create_file(&pool, carol, None, &format!("f{n}"), 3, None, &key(n))
                .await
                .unwrap();
        }
        let storage_db::StartMove::Started { move_id, .. } =
            storage_db::start_move(&pool, carol, Some(volume))
                .await
                .unwrap()
        else {
            panic!("expected a move");
        };

        run(&pool, data.path(), move_id).await.unwrap();

        for n in 1..=3 {
            assert!(
                storage::blob_path(&disk.path().join("carol"), &key(n))
                    .unwrap()
                    .exists()
            );
        }
        // 原本的資料夾整個被移除
        assert!(!data.path().join("carol").exists());
        let here = storage_db::user_location(&pool, carol)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(here.volume_id, Some(volume));
        assert!(!here.moving);
    }

    #[sqlx::test]
    async fn files_deleted_during_the_move_are_skipped(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let volume = storage_db::add_volume(&pool, "disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        // 紀錄還在，但 blob 已經被刪除（搬移開始之後被永久刪除）
        files::create_file(&pool, carol, None, "gone", 3, None, &key(1))
            .await
            .unwrap();
        let storage_db::StartMove::Started { move_id, .. } =
            storage_db::start_move(&pool, carol, Some(volume))
                .await
                .unwrap()
        else {
            panic!("expected a move");
        };

        run(&pool, data.path(), move_id).await.unwrap();

        let here = storage_db::user_location(&pool, carol)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(here.volume_id, Some(volume));
    }

    #[sqlx::test]
    async fn a_cancelled_move_changes_nothing(pool: SqlitePool) {
        let data = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        storage::mark_volume(disk.path()).unwrap();
        let carol = user::create_user(&pool, "carol", "x").await.unwrap();
        let volume = storage_db::add_volume(&pool, "disk2", disk.path().to_str().unwrap())
            .await
            .unwrap()
            .id;
        store(data.path(), &key(1), b"abc").await;
        files::create_file(&pool, carol, None, "a", 3, None, &key(1))
            .await
            .unwrap();
        let storage_db::StartMove::Started { move_id, .. } =
            storage_db::start_move(&pool, carol, Some(volume))
                .await
                .unwrap()
        else {
            panic!("expected a move");
        };
        storage_db::cancel_move(&pool, carol).await.unwrap();

        run(&pool, data.path(), move_id).await.unwrap();

        assert!(
            storage::blob_path(&data.path().join("carol"), &key(1))
                .unwrap()
                .exists()
        );
        let here = storage_db::user_location(&pool, carol)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(here.volume_id, None);
    }
}
