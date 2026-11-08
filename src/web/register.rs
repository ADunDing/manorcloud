//! 依照對同名的選擇建立檔案紀錄。一般上傳與分段上傳共用。
//!
//! - rename（預設）：自動改名為「旅行 (1).jpg」、「旅行 (2).jpg」⋯⋯，與瀏覽器下載同名檔案的規則相同。
//!   永遠不覆蓋任何東西，傳好的內容也不會因為同名而白費
//! - fail：同名就拒絕

use std::path::Path;

use serde::Deserialize;
use sqlx::SqlitePool;

use super::{error::AppError, files::folder_not_found};
use crate::{
    db::files::{self as files_db, CreateFileError, FileEntry, ReplaceOutcome},
    files::name::{numbered_name, validate_file_name},
    storage,
};

/// 自動改名時最多試到幾號
const MAX_NUMBER: u32 = 999;
/// 找到的名稱被同時的請求搶先時，最多重找幾次
const MAX_ATTEMPTS: u32 = 3;

/// 同名時怎麼處理。網址參數與 JSON 都寫成 rename、fail、replace。
/// 預設自動改名：使用者上傳同名檔案時，最自然的期待是「兩個都留著」
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    #[default]
    Rename,
    Fail,
    Replace,
}

#[derive(Debug)]
pub enum RegisterError {
    /// 同名（fail，或改名、取代都被搶先）
    NameConflict,
    /// 選擇取代，但同名的是資料夾
    FolderInTheWay,
    /// 父資料夾不存在或不屬於自己
    ParentNotFound,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for RegisterError {
    fn from(e: sqlx::Error) -> Self {
        RegisterError::Db(e)
    }
}

impl From<CreateFileError> for RegisterError {
    fn from(e: CreateFileError) -> Self {
        match e {
            CreateFileError::NameConflict => RegisterError::NameConflict,
            // 兩者對外必須完全相同，見 CreateFileError 的說明
            CreateFileError::ParentNotFolder | CreateFileError::ParentNotOwned => {
                RegisterError::ParentNotFound
            }
            CreateFileError::Db(e) => RegisterError::Db(e),
        }
    }
}

impl RegisterError {
    /// 一般的錯誤回應。分段上傳的同名有自己的代碼，由呼叫端另外處理
    pub fn into_app_error(self) -> AppError {
        match self {
            RegisterError::NameConflict => AppError::Conflict("同名項目已存在".to_string()),
            RegisterError::FolderInTheWay => {
                AppError::Conflict("同名的是資料夾，無法用檔案取代".to_string())
            }
            RegisterError::ParentNotFound => folder_not_found(),
            RegisterError::Db(e) => AppError::internal(e),
        }
    }
}

/// 要建立的檔案。名稱必須已通過 validate_file_name，blob 必須已經就位
pub struct NewFile<'a> {
    pub owner_id: i64,
    pub parent_id: Option<i64>,
    pub name: &'a str,
    pub size: i64,
    pub storage_key: &'a str,
}

/// 依照選擇提早檢查同名：不必傳完才發現。
/// 只是為了使用體驗，結果以 register_file 為準
pub async fn precheck(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
    name: &str,
    on_conflict: OnConflict,
) -> Result<(), RegisterError> {
    let existing = files_db::existing_kind(pool, owner_id, parent_id, name).await?;
    match (on_conflict, existing.as_deref()) {
        (_, None) | (OnConflict::Rename, _) => Ok(()),
        (OnConflict::Fail, Some(_)) => Err(RegisterError::NameConflict),
        (OnConflict::Replace, Some("folder")) => Err(RegisterError::FolderInTheWay),
        (OnConflict::Replace, Some(_)) => Ok(()),
    }
}

/// 依照選擇建立檔案紀錄。
///
/// 失敗時呼叫端負責刪除新的 blob；取代成功時，舊的 blob 在這裡刪除。
pub async fn register_file(
    pool: &SqlitePool,
    data_dir: &Path,
    file: &NewFile<'_>,
    on_conflict: OnConflict,
) -> Result<FileEntry, RegisterError> {
    match on_conflict {
        OnConflict::Fail => create(pool, file, file.name).await,
        OnConflict::Rename => create_renamed(pool, file).await,
        OnConflict::Replace => replace(pool, data_dir, file).await,
    }
}

/// 以指定的名稱建立。MIME 由最終的名稱推斷，不相信客戶端宣告的類型
async fn create(
    pool: &SqlitePool,
    file: &NewFile<'_>,
    name: &str,
) -> Result<FileEntry, RegisterError> {
    let mime = mime_guess::from_path(name).first_raw();
    Ok(files_db::create_file(
        pool,
        file.owner_id,
        file.parent_id,
        name,
        file.size,
        mime,
        file.storage_key,
    )
    .await?)
}

/// 保留兩者：原名沒被使用就用原名，否則找第一個沒被使用的編號
async fn create_renamed(pool: &SqlitePool, file: &NewFile<'_>) -> Result<FileEntry, RegisterError> {
    for _ in 0..MAX_ATTEMPTS {
        let Some(name) = free_name(pool, file).await? else {
            return Err(RegisterError::NameConflict);
        };
        match create(pool, file, &name).await {
            // 查到沒被使用、建立之前被同時的請求搶先：重找
            Err(RegisterError::NameConflict) => continue,
            other => return other,
        }
    }
    Err(RegisterError::NameConflict)
}

/// 第一個沒被使用的名稱：原名、(1)、(2)⋯⋯。
/// 加上編號後超過長度上限、或試到 MAX_NUMBER 都被使用時，回傳 None
async fn free_name(pool: &SqlitePool, file: &NewFile<'_>) -> Result<Option<String>, sqlx::Error> {
    if files_db::existing_kind(pool, file.owner_id, file.parent_id, file.name)
        .await?
        .is_none()
    {
        return Ok(Some(file.name.to_string()));
    }
    for n in 1..=MAX_NUMBER {
        let candidate = numbered_name(file.name, n);
        if validate_file_name(&candidate).is_err() {
            return Ok(None);
        }
        if files_db::existing_kind(pool, file.owner_id, file.parent_id, &candidate)
            .await?
            .is_none()
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// 取代：同名的是檔案就換掉內容；不存在就直接建立
async fn replace(
    pool: &SqlitePool,
    data_dir: &Path,
    file: &NewFile<'_>,
) -> Result<FileEntry, RegisterError> {
    let mime = mime_guess::from_path(file.name).first_raw();
    for _ in 0..MAX_ATTEMPTS {
        let outcome = files_db::replace_file(
            pool,
            file.owner_id,
            file.parent_id,
            file.name,
            file.size,
            mime,
            file.storage_key,
        )
        .await?;

        match outcome {
            ReplaceOutcome::Replaced { entry, old_key } => {
                // 紀錄已經指向新的 blob（交易已提交），這時才刪除舊的。
                // 失敗只會留下多餘的 blob，由垃圾回收清除
                if let Some(old_key) = old_key
                    && let Err(e) = storage::remove_blob(data_dir, &old_key).await
                {
                    eprintln!("failed to remove the replaced blob {old_key}: {e}");
                }
                return Ok(entry);
            }
            ReplaceOutcome::IsFolder => return Err(RegisterError::FolderInTheWay),
            ReplaceOutcome::Missing => match create(pool, file, file.name).await {
                // 查到不存在、建立之前被同時的請求搶先：重新嘗試取代
                Err(RegisterError::NameConflict) => continue,
                other => return other,
            },
        }
    }
    Err(RegisterError::NameConflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    fn write_blob(dir: &Path, key: &str) {
        let path = storage::blob_path(dir, key).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    fn new_file<'a>(owner_id: i64, name: &'a str, key: &'a str) -> NewFile<'a> {
        NewFile {
            owner_id,
            parent_id: None,
            name,
            size: 1,
            storage_key: key,
        }
    }

    async fn make_user(pool: &SqlitePool) -> i64 {
        user::create_user(pool, "carol", "x").await.unwrap()
    }

    // ==================== precheck ====================

    #[sqlx::test]
    async fn precheck_follows_the_choice(pool: SqlitePool) {
        let carol = make_user(&pool).await;
        files_db::create_file(&pool, carol, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();
        files_db::create_folder(&pool, carol, None, "docs")
            .await
            .unwrap();

        let check = |name: &'static str, choice| {
            let pool = pool.clone();
            async move { precheck(&pool, carol, None, name, choice).await }
        };

        assert!(check("new.txt", OnConflict::Fail).await.is_ok());
        assert!(matches!(
            check("A.TXT", OnConflict::Fail).await,
            Err(RegisterError::NameConflict)
        ));
        assert!(check("a.txt", OnConflict::Rename).await.is_ok());
        assert!(check("a.txt", OnConflict::Replace).await.is_ok());
        assert!(matches!(
            check("docs", OnConflict::Replace).await,
            Err(RegisterError::FolderInTheWay)
        ));
    }

    // ==================== 保留兩者 ====================

    #[sqlx::test]
    async fn rename_uses_the_original_name_when_free(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let carol = make_user(&pool).await;

        let entry = register_file(
            &pool,
            dir.path(),
            &new_file(carol, "a.txt", &key(1)),
            OnConflict::Rename,
        )
        .await
        .unwrap();

        assert_eq!(entry.name, "a.txt");
    }

    #[sqlx::test]
    async fn rename_picks_the_first_free_number(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let carol = make_user(&pool).await;
        files_db::create_file(&pool, carol, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();
        files_db::create_file(&pool, carol, None, "a (1).txt", 1, None, &key(2))
            .await
            .unwrap();

        // 不分大小寫：A.TXT 與 A (1).TXT 都被佔用
        let entry = register_file(
            &pool,
            dir.path(),
            &new_file(carol, "A.TXT", &key(3)),
            OnConflict::Rename,
        )
        .await
        .unwrap();

        assert_eq!(entry.name, "A (2).TXT");
        assert_eq!(entry.mime.as_deref(), Some("text/plain"));
    }

    // ==================== 取代 ====================

    #[sqlx::test]
    async fn replace_keeps_the_id_and_removes_the_old_blob(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let carol = make_user(&pool).await;
        write_blob(dir.path(), &key(1));
        write_blob(dir.path(), &key(2));
        let original = files_db::create_file(&pool, carol, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();

        let entry = register_file(
            &pool,
            dir.path(),
            &new_file(carol, "a.txt", &key(2)),
            OnConflict::Replace,
        )
        .await
        .unwrap();

        assert_eq!(entry.id, original.id);
        assert_eq!(entry.storage_key.as_deref(), Some(key(2).as_str()));
        // 舊的 blob 已刪除，新的還在
        assert!(!storage::blob_path(dir.path(), &key(1)).unwrap().exists());
        assert!(storage::blob_path(dir.path(), &key(2)).unwrap().exists());
    }

    #[sqlx::test]
    async fn replace_without_an_existing_file_creates_one(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let carol = make_user(&pool).await;

        let entry = register_file(
            &pool,
            dir.path(),
            &new_file(carol, "a.txt", &key(1)),
            OnConflict::Replace,
        )
        .await
        .unwrap();

        assert_eq!(entry.name, "a.txt");
    }

    #[sqlx::test]
    async fn a_folder_cannot_be_replaced(pool: SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let carol = make_user(&pool).await;
        files_db::create_folder(&pool, carol, None, "docs")
            .await
            .unwrap();

        let result = register_file(
            &pool,
            dir.path(),
            &new_file(carol, "docs", &key(1)),
            OnConflict::Replace,
        )
        .await;

        assert!(matches!(result, Err(RegisterError::FolderInTheWay)));
    }
}
