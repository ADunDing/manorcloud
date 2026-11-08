use std::fmt;

use sqlx::SqlitePool;

/// 與 migration 中 trigger 的 RAISE 訊息完全一致。
/// 兩邊必須同步修改；`parent_is_a_file` 測試會在不一致時失敗。
const PARENT_NOT_FOLDER_MSG: &str = "parent must be a folder";

/// files 表的一列。
///
/// 含有 `storage_key` 這個內部資訊，不應直接序列化回傳給前端，
/// HTTP 層要另外定義回應型別（比照 User 與 CurrentUser 的做法）。
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)] // 查詢時一併取出，除錯時有用；目前沒有程式讀取
pub struct FileEntry {
    pub id: i64,
    pub owner_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub size: Option<i64>,
    pub mime: Option<String>,
    pub storage_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 建立檔案或資料夾時，資料庫約束可能拒絕的原因。
///
/// **HTTP 層的對應規則（安全性要求）：**
/// `ParentNotFolder` 與 `ParentNotOwned` 必須回傳**完全相同**的 404「找不到資料夾」。
/// 資料層保留區分只是為了測試與記錄 log。若對外回應不同，
/// 攻擊者可以用遞增的整數 id 逐一探測，判斷哪些 id 是別人的資料夾，
/// 進而推測他人的目錄結構。這與登入時不區分「帳號不存在」與「密碼錯誤」是同一個道理。
#[derive(Debug)]
pub enum CreateFileError {
    /// 同一資料夾內已有同名項目（不分大小寫，也不分檔案或資料夾）
    NameConflict,
    /// 父項目不存在或不是資料夾（由 trigger 擋下；trigger 先於外鍵執行）
    ParentNotFolder,
    /// 父資料夾存在，但屬於其他使用者（由複合外鍵擋下）
    ParentNotOwned,
    /// 其他資料庫錯誤
    Db(sqlx::Error),
}

impl fmt::Display for CreateFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateFileError::NameConflict => f.write_str("an item with the same name exists"),
            CreateFileError::ParentNotFolder => f.write_str("parent is missing or not a folder"),
            CreateFileError::ParentNotOwned => f.write_str("parent folder belongs to another user"),
            CreateFileError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for CreateFileError {}

/// 依資料庫回報的約束種類分類錯誤
fn classify(e: sqlx::Error) -> CreateFileError {
    if let Some(db) = e.as_database_error() {
        // storage_key 也有 UNIQUE，但 128 位元隨機值不會碰撞，視為名稱重複
        if db.is_unique_violation() {
            return CreateFileError::NameConflict;
        }
        if db.is_foreign_key_violation() {
            return CreateFileError::ParentNotOwned;
        }
        // trigger 的 RAISE 沒有專屬錯誤種類，只能比對訊息文字
        if db.message() == PARENT_NOT_FOLDER_MSG {
            return CreateFileError::ParentNotFolder;
        }
    }
    CreateFileError::Db(e)
}

/// 這個 id 是否為該使用者的資料夾。用於上傳、列表與搬移前的檢查。
///
/// 三個條件寫在同一個 WHERE 裡：「不存在」「屬於別人」「是檔案而非資料夾」
/// 都是「找不到符合的列」，天然得到同樣的 false，程式中沒有任何能洩漏差異的分支。
///
/// 上傳時這只是為了使用體驗（不讓使用者傳完大檔案才知道失敗）。
/// 檢查之後、寫入之前，資料夾仍可能被刪除，正確性由 `create_file` 的資料庫約束保證。
///
/// 接受連線池或交易（`&pool` 或 `&mut *tx`），讓交易內的檢查可以共用同一份 SQL。
pub async fn is_owned_folder<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
    owner_id: i64,
    folder_id: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM files
            WHERE id = ? AND owner_id = ? AND kind = 'folder'
        ) AS "exists!: bool"
        "#,
        folder_id,
        owner_id
    )
    .fetch_one(executor)
    .await
}

/// 新增一筆檔案紀錄。
///
/// 呼叫前必須：名稱已通過 `validate_file_name`、blob 已經寫入磁碟（`persist` 成功）。
/// （先寫 blob 再寫資料庫：中途失敗只會留下沒人參照的 blob，由垃圾回收清除。）
///
/// `size` 是 `i64`（SQLite 的 INTEGER），而 `TempBlob::persist` 回傳 `u64`。
/// 轉換時用 `i64::try_from`，不要用 `as`：`as` 遇到超出範圍的值會默默變成負數。
pub async fn create_file(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
    name: &str,
    size: i64,
    mime: Option<&str>,
    storage_key: &str,
) -> Result<FileEntry, CreateFileError> {
    sqlx::query_as!(
        FileEntry,
        r#"
        INSERT INTO files (owner_id, parent_id, name, kind, size, mime, storage_key)
        VALUES (?, ?, ?, 'file', ?, ?, ?)
        RETURNING
            id          AS "id!",
            owner_id    AS "owner_id!",
            parent_id   AS "parent_id?",
            name        AS "name!",
            kind        AS "kind!",
            size        AS "size?",
            mime        AS "mime?",
            storage_key AS "storage_key?",
            created_at  AS "created_at!",
            updated_at  AS "updated_at!"
        "#,
        owner_id,
        parent_id,
        name,
        size,
        mime,
        storage_key
    )
    .fetch_one(pool)
    .await
    .map_err(classify)
}

/// 資料夾中同名項目（不分大小寫，與唯一索引相同）的種類："file" 或 "folder"；沒有時為 None。
///
/// `parent_id IS ?` 而不是 `= ?`：根目錄的 parent_id 是 NULL，而 `NULL = NULL` 不成立
pub async fn existing_kind(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
    name: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT kind FROM files
         WHERE owner_id = ? AND parent_id IS ? AND name = ? COLLATE NOCASE",
    )
    .bind(owner_id)
    .bind(parent_id)
    .bind(name)
    .fetch_optional(pool)
    .await
}

/// 取代同名檔案的結果
#[derive(Debug)]
pub enum ReplaceOutcome {
    /// 已取代：紀錄指向新的 blob，保留原本的編號。舊的 blob 由呼叫端在這之後刪除
    Replaced {
        entry: FileEntry,
        old_key: Option<String>,
    },
    /// 沒有同名的項目：呼叫端改用 create_file 建立
    Missing,
    /// 同名的是資料夾，不能用檔案取代
    IsFolder,
}

/// 把同名檔案的內容換成新的 blob：保留編號，更新大小、類型與修改時間。
///
/// 查詢與更新在同一個交易中，舊的 storage_key 一定是被換掉的那一個。
/// 回傳之後呼叫端才刪除舊的 blob：與刪除項目相同的順序，任何失敗都只會留下多餘的 blob，
/// 不會出現指向不存在內容的紀錄。
pub async fn replace_file(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
    name: &str,
    size: i64,
    mime: Option<&str>,
    storage_key: &str,
) -> Result<ReplaceOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let existing: Option<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, kind, storage_key FROM files
         WHERE owner_id = ? AND parent_id IS ? AND name = ? COLLATE NOCASE",
    )
    .bind(owner_id)
    .bind(parent_id)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;

    // 交易在 return 時被丟棄，自動回滾（這兩種情況都沒有寫入任何東西）
    let Some((id, kind, old_key)) = existing else {
        return Ok(ReplaceOutcome::Missing);
    };
    if kind != "file" {
        return Ok(ReplaceOutcome::IsFolder);
    }

    let entry: FileEntry = sqlx::query_as(
        "UPDATE files SET storage_key = ?, size = ?, mime = ?, updated_at = unixepoch()
         WHERE id = ?
         RETURNING id, owner_id, parent_id, name, kind, size, mime, storage_key,
                   created_at, updated_at",
    )
    .bind(storage_key)
    .bind(size)
    .bind(mime)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(ReplaceOutcome::Replaced { entry, old_key })
}

/// 新增一個資料夾。名稱必須已通過 `validate_file_name`。
///
/// 錯誤分類與 `create_file` 相同：同名（不分檔案或資料夾）、父項目無效。
pub async fn create_folder(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
    name: &str,
) -> Result<FileEntry, CreateFileError> {
    sqlx::query_as!(
        FileEntry,
        r#"
        INSERT INTO files (owner_id, parent_id, name, kind)
        VALUES (?, ?, ?, 'folder')
        RETURNING
            id          AS "id!",
            owner_id    AS "owner_id!",
            parent_id   AS "parent_id?",
            name        AS "name!",
            kind        AS "kind!",
            size        AS "size?",
            mime        AS "mime?",
            storage_key AS "storage_key?",
            created_at  AS "created_at!",
            updated_at  AS "updated_at!"
        "#,
        owner_id,
        parent_id,
        name
    )
    .fetch_one(pool)
    .await
    .map_err(classify)
}

/// 列出某資料夾（`None` 代表根目錄）的直接子項目：資料夾在前，再依名稱排序。
///
/// - `parent_id IS ?`：`IS` 把兩個 NULL 視為相等，同一條 SQL 可處理根目錄與資料夾。
///   用 `=` 的話 `parent_id = NULL` 永遠不成立，根目錄會永遠是空的。
/// - `ORDER BY name` 沿用欄位的 NOCASE：Alpha 排在 zeta 之前（僅限 ASCII）。
/// - 只回傳該使用者的項目。呼叫端需自行確認 parent_id 是自己的資料夾，
///   否則「不存在」與「空資料夾」都會得到空陣列。
///
/// 效能取捨：SQLite 準備查詢時不知道 `?` 是否為 NULL，因此無法使用兩個部分索引，
/// 會改用 (owner_id, id) 的索引掃描該使用者的所有項目。家用規模沒有問題；
/// 單一使用者若有數十萬個項目，可拆成「根目錄」與「資料夾」兩條 SQL。
pub async fn list_children(
    pool: &SqlitePool,
    owner_id: i64,
    parent_id: Option<i64>,
) -> Result<Vec<FileEntry>, sqlx::Error> {
    sqlx::query_as!(
        FileEntry,
        r#"
        SELECT
            id          AS "id!",
            owner_id    AS "owner_id!",
            parent_id   AS "parent_id?",
            name        AS "name!",
            kind        AS "kind!",
            size        AS "size?",
            mime        AS "mime?",
            storage_key AS "storage_key?",
            created_at  AS "created_at!",
            updated_at  AS "updated_at!"
        FROM files
        WHERE owner_id = ? AND parent_id IS ?
        ORDER BY kind = 'folder' DESC, name, id
        "#,
        owner_id,
        parent_id
    )
    .fetch_all(pool)
    .await
}

/// 取得該使用者的一個項目。「不存在」與「屬於別人」都回傳 `None`。
///
/// 接受連線池或交易（`&pool` 或 `&mut *tx`）。
pub async fn find_file<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
    owner_id: i64,
    id: i64,
) -> Result<Option<FileEntry>, sqlx::Error> {
    sqlx::query_as!(
        FileEntry,
        r#"
        SELECT
            id          AS "id!",
            owner_id    AS "owner_id!",
            parent_id   AS "parent_id?",
            name        AS "name!",
            kind        AS "kind!",
            size        AS "size?",
            mime        AS "mime?",
            storage_key AS "storage_key?",
            created_at  AS "created_at!",
            updated_at  AS "updated_at!"
        FROM files
        WHERE id = ? AND owner_id = ?
        "#,
        id,
        owner_id
    )
    .fetch_optional(executor)
    .await
}

/// 刪除一個項目；若是資料夾，底下所有內容一併刪除（CASCADE）。
///
/// 回傳：`None` 代表不存在或不屬於該使用者；
/// `Some(keys)` 代表**已經 commit**，`keys` 是被刪除的檔案的 storage_key，
/// 呼叫端必須接著刪除這些 blob（失敗則交給垃圾回收）。
///
/// 不變式：資料庫裡的每一個 storage_key，磁碟上一定有對應的 blob。
/// 所以順序是「先刪資料庫、commit 之後才刪 blob」：
/// 中途失敗只會留下孤兒 blob，不會留下指向不存在檔案的紀錄。
///
/// 為什麼需要交易，而且是 IMMEDIATE：
/// - `DELETE ... RETURNING` 看不到 CASCADE 連帶刪除的列，所以要先用遞迴 CTE 找出整棵子樹的 key。
///   「找 key」與「刪除」若不在同一個交易，中間有人上傳檔案進這個資料夾，
///   它會被 CASCADE 刪掉，但 key 不在清單裡，blob 就成了孤兒。
/// - DEFERRED 交易先讀後寫，在 WAL 模式下若有其他連線在中間 commit，
///   升級成寫入時會立刻得到 SQLITE_BUSY_SNAPSHOT，busy_timeout 無法解決（快照已過時）。
///   IMMEDIATE 在 BEGIN 時就取得寫入鎖，其他寫入者排隊，我們讀到的資料在 commit 前不會被改動。
pub async fn delete_tree(
    pool: &SqlitePool,
    owner_id: i64,
    id: i64,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    // 1. 收集整棵子樹的 storage_key。
    //    起點帶 owner_id：別人的 id 查不到任何東西。
    //    用 UNION 而非 UNION ALL：萬一資料已損壞形成循環，遇到走過的節點會自然停止。
    let keys = sqlx::query_scalar!(
        r#"
        WITH RECURSIVE subtree(id) AS (
            SELECT id FROM files WHERE id = ? AND owner_id = ?
            UNION
            SELECT f.id FROM files f JOIN subtree s ON f.parent_id = s.id
        )
        SELECT storage_key AS "storage_key!"
        FROM files
        WHERE id IN (SELECT id FROM subtree) AND storage_key IS NOT NULL
        "#,
        id,
        owner_id
    )
    .fetch_all(&mut *tx)
    .await?;

    // 2. 刪除；CASCADE 會帶走所有子孫。
    //    不另外查「是否存在」：不存在與屬於別人都是影響 0 列，天然無法區分。
    let deleted = sqlx::query!(
        "DELETE FROM files WHERE id = ? AND owner_id = ?",
        id,
        owner_id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if deleted == 0 {
        // tx 在這裡被丟棄，自動 rollback
        return Ok(None);
    }

    tx.commit().await?;
    Ok(Some(keys))
}

/// 取得某個分層（storage_key 的前兩字元）中所有的 storage_key，供垃圾回收比對。
///
/// 用範圍查詢 `>= 'ab' AND < 'abg'` 而不是 `substr(storage_key, 1, 2) = 'ab'`：
/// 後者會讓 storage_key 的唯一索引失效，256 個分層就是 256 次全表掃描。
/// 上界成立的原因：key 只含 0-9a-f，第三個字元最大是 'f'，而 'g' 排在 'f' 之後。
pub async fn keys_in_shard(pool: &SqlitePool, shard: &str) -> Result<Vec<String>, sqlx::Error> {
    let upper = format!("{shard}g");
    sqlx::query_scalar!(
        r#"
        SELECT storage_key AS "storage_key!"
        FROM files
        WHERE storage_key >= ? AND storage_key < ?
        "#,
        shard,
        upper
    )
    .fetch_all(pool)
    .await
}

/// 重新命名或搬移時可能的失敗原因
#[derive(Debug)]
pub enum UpdateError {
    /// 項目不存在或不屬於該使用者
    NotFound,
    /// 目標父資料夾不存在、不屬於該使用者，或不是資料夾。
    /// HTTP 層必須對這三種情況回傳相同的 404（理由同 `CreateFileError`）。
    TargetNotFolder,
    /// 把資料夾搬進它自己或它的子孫，會讓整棵子樹與根目錄斷開
    Cycle,
    /// 目標位置已有同名項目（不分大小寫）
    NameConflict,
    /// 其他資料庫錯誤
    Db(sqlx::Error),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::NotFound => f.write_str("item not found"),
            UpdateError::TargetNotFolder => f.write_str("target is not one of the user's folders"),
            UpdateError::Cycle => f.write_str("cannot move a folder into itself or its subfolder"),
            UpdateError::NameConflict => f.write_str("an item with the same name exists"),
            UpdateError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<sqlx::Error> for UpdateError {
    fn from(e: sqlx::Error) -> Self {
        UpdateError::Db(e)
    }
}

/// 把 UPDATE 時的約束錯誤，沿用建立時的分類規則轉換過來
fn update_constraint_error(e: sqlx::Error) -> UpdateError {
    match classify(e) {
        CreateFileError::NameConflict => UpdateError::NameConflict,
        CreateFileError::ParentNotFolder | CreateFileError::ParentNotOwned => {
            UpdateError::TargetNotFolder
        }
        CreateFileError::Db(e) => UpdateError::Db(e),
    }
}

/// 重新命名及／或搬移一個項目。
///
/// - `new_name`：`None` 代表不改名。名稱必須已通過 `validate_file_name`。
/// - `new_parent`：`None` 代表不搬移；`Some(None)` 搬到根目錄；`Some(Some(id))` 搬到該資料夾。
///
/// 循環檢查無法放進資料庫：SQLite 的 trigger 不能使用 WITH 子句，
/// 所以只能在這裡用遞迴 CTE 檢查。也因此資料庫本身無法阻止循環
/// （例如用 sqlite3 手動修改），所有遞迴查詢都用 UNION 防止無限遞迴。
///
/// 檢查與更新必須在同一個 IMMEDIATE 交易裡：兩個分頁同時送出「A 搬進 B」與「B 搬進 A」，
/// 若用 DEFERRED，兩邊可能都在對方 commit 之前通過檢查，結果形成循環。
/// IMMEDIATE 讓第二個交易在 BEGIN 時排隊，等第一個 commit 後才讀取，它的檢查就會看到新狀態而拒絕。
pub async fn update_item(
    pool: &SqlitePool,
    owner_id: i64,
    id: i64,
    new_name: Option<&str>,
    new_parent: Option<Option<i64>>,
) -> Result<FileEntry, UpdateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    // 1. 讀取項目（只查得到自己的）
    let current = find_file(&mut *tx, owner_id, id)
        .await?
        .ok_or(UpdateError::NotFound)?;

    // 2. 搬到某個資料夾時：確認目標有效，資料夾還要檢查循環
    if let Some(Some(target)) = new_parent {
        // 明確檢查而不只依賴約束：錯誤訊息清楚，而且確保下面的祖先鏈查詢不會走進別人的資料
        if !is_owned_folder(&mut *tx, owner_id, target).await? {
            return Err(UpdateError::TargetNotFolder);
        }

        // 檔案沒有子項目，不可能形成循環
        if current.kind == "folder" {
            // 從目標往上走祖先鏈，遇到被搬移的資料夾就代表會形成循環。
            // 目標等於自己時，祖先鏈的第一筆就是自己，所以「搬進自己」也被涵蓋。
            let would_cycle = sqlx::query_scalar!(
                r#"
                WITH RECURSIVE ancestors(id) AS (
                    SELECT id FROM files WHERE id = ? AND owner_id = ?
                    UNION
                    SELECT f.parent_id FROM files f JOIN ancestors a ON f.id = a.id
                    WHERE f.parent_id IS NOT NULL
                )
                SELECT EXISTS (SELECT 1 FROM ancestors WHERE id = ?) AS "cycle!: bool"
                "#,
                target,
                owner_id,
                id
            )
            .fetch_one(&mut *tx)
            .await?;

            if would_cycle {
                return Err(UpdateError::Cycle);
            }
        }
    }

    // 3. 算出新值並更新。沒指定的欄位沿用原值。
    //    只改大小寫（a.txt → A.txt）不會與自己衝突：唯一約束比較的是其他列。
    let name = new_name.unwrap_or(&current.name);
    let parent_id = new_parent.unwrap_or(current.parent_id);

    let updated = sqlx::query_as!(
        FileEntry,
        r#"
        UPDATE files
        SET name = ?, parent_id = ?, updated_at = unixepoch()
        WHERE id = ? AND owner_id = ?
        RETURNING
            id          AS "id!",
            owner_id    AS "owner_id!",
            parent_id   AS "parent_id?",
            name        AS "name!",
            kind        AS "kind!",
            size        AS "size?",
            mime        AS "mime?",
            storage_key AS "storage_key?",
            created_at  AS "created_at!",
            updated_at  AS "updated_at!"
        "#,
        name,
        parent_id,
        id,
        owner_id
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(update_constraint_error)?;

    tx.commit().await?;
    Ok(updated)
}

/// 祖先鏈的一層
pub struct Ancestor {
    pub id: i64,
    pub name: String,
}

/// 祖先鏈的最大深度。正常資料不會有循環（搬移時會檢查），
/// 這是資料被手動改壞時，防止遞迴查詢失控的保險。
const MAX_ANCESTOR_DEPTH: i64 = 256;

/// 由根目錄往下排列的祖先鏈（不含自己），用於麵包屑導覽。
/// 項目不存在或不屬於該使用者時回傳空的清單；呼叫端應先用 `find_file` 確認項目存在。
///
/// 起點帶 owner_id；往上走的每一層由複合外鍵保證屬於同一個使用者。
///
/// 用 UNION ALL 加深度上限，而不是靠 UNION 去除重複來防止循環：
/// 每一列都帶有深度，同一個資料夾在不同深度是不同的列，UNION 無法讓循環停下來。
pub async fn ancestors(
    pool: &SqlitePool,
    owner_id: i64,
    id: i64,
) -> Result<Vec<Ancestor>, sqlx::Error> {
    sqlx::query_as!(
        Ancestor,
        r#"
        WITH RECURSIVE chain(id, parent_id, name, depth) AS (
            SELECT id, parent_id, name, 0 FROM files WHERE id = ? AND owner_id = ?
            UNION ALL
            SELECT f.id, f.parent_id, f.name, c.depth + 1
            FROM files f JOIN chain c ON f.id = c.parent_id
            WHERE c.depth < ?
        )
        SELECT id AS "id!", name AS "name!"
        FROM chain
        WHERE depth > 0
        ORDER BY depth DESC
        "#,
        id,
        owner_id,
        MAX_ANCESTOR_DEPTH
    )
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::user;

    async fn make_user(pool: &SqlitePool, username: &str) -> i64 {
        user::create_user(pool, username, "x").await.unwrap()
    }

    async fn make_folder(pool: &SqlitePool, owner: i64, parent: Option<i64>, name: &str) -> i64 {
        create_folder(pool, owner, parent, name).await.unwrap().id
    }

    /// 由數字產生合法的 storage_key（32 個小寫 hex 字元）
    fn key(n: u32) -> String {
        format!("{n:032x}")
    }

    fn names(entries: &[FileEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    // ==================== is_owned_folder ====================

    #[sqlx::test]
    async fn own_folder_is_owned(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;

        assert!(is_owned_folder(&pool, owner, docs).await.unwrap());
    }

    #[sqlx::test]
    async fn nested_own_folder_is_owned(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;
        let sub = make_folder(&pool, owner, Some(docs), "sub").await;

        assert!(is_owned_folder(&pool, owner, sub).await.unwrap());
    }

    #[sqlx::test]
    async fn other_users_folder_is_not_owned(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let alice_docs = make_folder(&pool, alice, None, "docs").await;

        assert!(!is_owned_folder(&pool, mallory, alice_docs).await.unwrap());
    }

    #[sqlx::test]
    async fn nonexistent_id_is_not_owned(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        assert!(!is_owned_folder(&pool, owner, 9999).await.unwrap());
    }

    #[sqlx::test]
    async fn own_file_is_not_a_folder(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        assert!(!is_owned_folder(&pool, owner, file.id).await.unwrap());
    }

    // ==================== create_file ====================

    #[sqlx::test]
    async fn creates_file_in_root(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        let entry = create_file(&pool, owner, None, "a.txt", 5, Some("text/plain"), &key(1))
            .await
            .unwrap();

        assert_eq!(entry.owner_id, owner);
        assert_eq!(entry.parent_id, None);
        assert_eq!(entry.name, "a.txt");
        assert_eq!(entry.kind, "file");
        assert_eq!(entry.size, Some(5));
        assert_eq!(entry.mime.as_deref(), Some("text/plain"));
        assert_eq!(entry.storage_key.as_deref(), Some(key(1).as_str()));
        assert!(entry.created_at > 0);
    }

    #[sqlx::test]
    async fn creates_file_in_own_folder(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;

        let entry = create_file(&pool, owner, Some(docs), "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        assert_eq!(entry.parent_id, Some(docs));
        assert_eq!(entry.mime, None);
    }

    #[sqlx::test]
    async fn duplicate_name_conflicts_case_insensitively(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let same = create_file(&pool, owner, None, "a.txt", 0, None, &key(2)).await;
        let upper = create_file(&pool, owner, None, "A.TXT", 0, None, &key(3)).await;

        assert!(matches!(same, Err(CreateFileError::NameConflict)));
        assert!(matches!(upper, Err(CreateFileError::NameConflict)));
    }

    #[sqlx::test]
    async fn same_name_allowed_in_different_folders(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;

        create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();
        let result = create_file(&pool, owner, Some(docs), "a.txt", 0, None, &key(2)).await;

        assert!(result.is_ok());
    }

    #[sqlx::test]
    async fn parent_is_a_file(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let result = create_file(&pool, owner, Some(file.id), "b.txt", 0, None, &key(2)).await;

        // 若 migration 的 RAISE 訊息被改掉，這裡會變成 Db(_) 而失敗
        assert!(matches!(result, Err(CreateFileError::ParentNotFolder)));
    }

    #[sqlx::test]
    async fn nonexistent_parent_is_caught_by_trigger(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        let result = create_file(&pool, owner, Some(9999), "a.txt", 0, None, &key(1)).await;

        // 記錄行為：BEFORE trigger 先於外鍵執行，所以不存在的父項目是 ParentNotFolder
        assert!(matches!(result, Err(CreateFileError::ParentNotFolder)));
    }

    #[sqlx::test]
    async fn other_users_folder_is_rejected(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let alice_docs = make_folder(&pool, alice, None, "docs").await;

        let result = create_file(&pool, mallory, Some(alice_docs), "x.txt", 0, None, &key(1)).await;

        assert!(matches!(result, Err(CreateFileError::ParentNotOwned)));
    }

    #[sqlx::test]
    async fn invalid_storage_key_is_rejected_by_check(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        let result = create_file(&pool, owner, None, "a.txt", 0, None, "../../etc/passwd").await;

        // CHECK 約束擋下，不屬於前三種，歸類為一般資料庫錯誤
        assert!(matches!(result, Err(CreateFileError::Db(_))));
    }

    // ==================== create_folder ====================

    #[sqlx::test]
    async fn creates_folder(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        let entry = create_folder(&pool, owner, None, "docs").await.unwrap();

        assert_eq!(entry.kind, "folder");
        assert_eq!(entry.size, None);
        assert_eq!(entry.storage_key, None);
    }

    #[sqlx::test]
    async fn folder_and_file_share_name_space(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        create_file(&pool, owner, None, "docs", 0, None, &key(1))
            .await
            .unwrap();

        // 同一資料夾內，資料夾不能與既有檔案同名（也不分大小寫）
        let result = create_folder(&pool, owner, None, "DOCS").await;

        assert!(matches!(result, Err(CreateFileError::NameConflict)));
    }

    #[sqlx::test]
    async fn folder_in_other_users_folder_is_rejected(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let alice_docs = make_folder(&pool, alice, None, "docs").await;

        let result = create_folder(&pool, mallory, Some(alice_docs), "x").await;

        assert!(matches!(result, Err(CreateFileError::ParentNotOwned)));
    }

    // ==================== list_children ====================

    #[sqlx::test]
    async fn list_root_puts_folders_first_case_insensitively(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        create_file(&pool, owner, None, "b.txt", 0, None, &key(1))
            .await
            .unwrap();
        create_file(&pool, owner, None, "A.txt", 0, None, &key(2))
            .await
            .unwrap();
        make_folder(&pool, owner, None, "zeta").await;
        make_folder(&pool, owner, None, "Alpha").await;

        let entries = list_children(&pool, owner, None).await.unwrap();

        // 資料夾在前；名稱不分大小寫排序，所以 Alpha 在 zeta 前、A.txt 在 b.txt 前
        assert_eq!(names(&entries), ["Alpha", "zeta", "A.txt", "b.txt"]);
    }

    #[sqlx::test]
    async fn list_root_excludes_nested_items(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;
        create_file(&pool, owner, Some(docs), "inner.txt", 0, None, &key(1))
            .await
            .unwrap();

        let root = list_children(&pool, owner, None).await.unwrap();
        let inside = list_children(&pool, owner, Some(docs)).await.unwrap();

        assert_eq!(names(&root), ["docs"]);
        assert_eq!(names(&inside), ["inner.txt"]);
    }

    #[sqlx::test]
    async fn list_only_returns_own_items(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        make_folder(&pool, alice, None, "alice-docs").await;
        make_folder(&pool, mallory, None, "mallory-docs").await;

        let entries = list_children(&pool, mallory, None).await.unwrap();

        assert_eq!(names(&entries), ["mallory-docs"]);
    }

    // ==================== find_file ====================

    #[sqlx::test]
    async fn find_own_file(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 3, None, &key(1))
            .await
            .unwrap();

        let found = find_file(&pool, owner, file.id).await.unwrap().unwrap();

        assert_eq!(found.name, "a.txt");
        assert_eq!(found.storage_key.as_deref(), Some(key(1).as_str()));
    }

    #[sqlx::test]
    async fn find_other_users_file_returns_none(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let file = create_file(&pool, alice, None, "secret.txt", 0, None, &key(1))
            .await
            .unwrap();

        assert!(find_file(&pool, mallory, file.id).await.unwrap().is_none());
        assert!(find_file(&pool, mallory, 9999).await.unwrap().is_none());
    }

    // ==================== delete_tree ====================

    async fn count_rows(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM files")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn delete_file_returns_its_key(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let keys = delete_tree(&pool, owner, file.id).await.unwrap();

        assert_eq!(keys, Some(vec![key(1)]));
        assert_eq!(count_rows(&pool).await, 0);
    }

    #[sqlx::test]
    async fn delete_folder_returns_all_descendant_keys(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        // docs/
        //   a.txt          (key 1)
        //   sub/
        //     b.txt        (key 2)
        // c.txt            (key 3，不在 docs 裡，不應受影響)
        let docs = make_folder(&pool, owner, None, "docs").await;
        let sub = make_folder(&pool, owner, Some(docs), "sub").await;
        create_file(&pool, owner, Some(docs), "a.txt", 0, None, &key(1))
            .await
            .unwrap();
        create_file(&pool, owner, Some(sub), "b.txt", 0, None, &key(2))
            .await
            .unwrap();
        create_file(&pool, owner, None, "c.txt", 0, None, &key(3))
            .await
            .unwrap();

        let mut keys = delete_tree(&pool, owner, docs).await.unwrap().unwrap();
        keys.sort();

        assert_eq!(keys, vec![key(1), key(2)]);
        // 只剩下 c.txt
        assert_eq!(count_rows(&pool).await, 1);
        assert_eq!(
            names(&list_children(&pool, owner, None).await.unwrap()),
            ["c.txt"]
        );
    }

    #[sqlx::test]
    async fn delete_empty_folder_returns_empty_list(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;

        // 刪除成功但沒有 blob 要清：Some(空清單)，與「找不到」的 None 不同
        assert_eq!(delete_tree(&pool, owner, docs).await.unwrap(), Some(vec![]));
        assert_eq!(count_rows(&pool).await, 0);
    }

    #[sqlx::test]
    async fn delete_other_users_item_returns_none_and_keeps_it(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let docs = make_folder(&pool, alice, None, "docs").await;
        create_file(&pool, alice, Some(docs), "secret.txt", 0, None, &key(1))
            .await
            .unwrap();

        assert_eq!(delete_tree(&pool, mallory, docs).await.unwrap(), None);
        // 交易已 rollback，alice 的資料完好
        assert_eq!(count_rows(&pool).await, 2);
    }

    #[sqlx::test]
    async fn delete_missing_returns_none(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;

        assert_eq!(delete_tree(&pool, owner, 9999).await.unwrap(), None);
    }

    // ==================== keys_in_shard ====================

    #[sqlx::test]
    async fn keys_in_shard_returns_only_that_shard(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let in_00 = key(1); // "000...01"，分層 00
        let in_01 = format!("01{}", "0".repeat(30));
        let in_0f = format!("0f{}", "0".repeat(30));
        for (i, k) in [&in_00, &in_01, &in_0f].into_iter().enumerate() {
            create_file(&pool, owner, None, &format!("{i}.txt"), 0, None, k)
                .await
                .unwrap();
        }
        // 資料夾沒有 storage_key，不應出現在任何分層
        make_folder(&pool, owner, None, "docs").await;

        assert_eq!(keys_in_shard(&pool, "00").await.unwrap(), vec![in_00]);
        assert_eq!(keys_in_shard(&pool, "01").await.unwrap(), vec![in_01]);
        assert_eq!(keys_in_shard(&pool, "0f").await.unwrap(), vec![in_0f]);
        assert!(keys_in_shard(&pool, "ff").await.unwrap().is_empty());
    }

    // ==================== update_item ====================

    async fn parent_of(pool: &SqlitePool, owner: i64, id: i64) -> Option<i64> {
        find_file(pool, owner, id).await.unwrap().unwrap().parent_id
    }

    #[sqlx::test]
    async fn rename_only(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let updated = update_item(&pool, owner, file.id, Some("b.txt"), None)
            .await
            .unwrap();

        assert_eq!(updated.name, "b.txt");
        assert_eq!(updated.parent_id, None);
    }

    #[sqlx::test]
    async fn move_into_folder_and_back_to_root(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let moved = update_item(&pool, owner, file.id, None, Some(Some(docs)))
            .await
            .unwrap();
        assert_eq!(moved.parent_id, Some(docs));
        assert_eq!(moved.name, "a.txt");

        let back = update_item(&pool, owner, file.id, None, Some(None))
            .await
            .unwrap();
        assert_eq!(back.parent_id, None);
    }

    #[sqlx::test]
    async fn rename_and_move_together(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let updated = update_item(&pool, owner, file.id, Some("b.txt"), Some(Some(docs)))
            .await
            .unwrap();

        assert_eq!(
            (updated.name.as_str(), updated.parent_id),
            ("b.txt", Some(docs))
        );
    }

    #[sqlx::test]
    async fn case_only_rename_does_not_conflict_with_itself(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();

        let updated = update_item(&pool, owner, file.id, Some("A.txt"), None)
            .await
            .unwrap();

        assert_eq!(updated.name, "A.txt");
    }

    #[sqlx::test]
    async fn moving_folder_into_itself_is_a_cycle(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let a = make_folder(&pool, owner, None, "a").await;

        let result = update_item(&pool, owner, a, None, Some(Some(a))).await;

        assert!(matches!(result, Err(UpdateError::Cycle)));
    }

    #[sqlx::test]
    async fn moving_folder_into_descendant_is_a_cycle(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        // a/b/c
        let a = make_folder(&pool, owner, None, "a").await;
        let b = make_folder(&pool, owner, Some(a), "b").await;
        let c = make_folder(&pool, owner, Some(b), "c").await;

        let result = update_item(&pool, owner, a, None, Some(Some(c))).await;

        assert!(matches!(result, Err(UpdateError::Cycle)));
        // 交易已 rollback，結構不變
        assert_eq!(parent_of(&pool, owner, a).await, None);
    }

    #[sqlx::test]
    async fn moving_folder_up_to_ancestor_is_allowed(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let a = make_folder(&pool, owner, None, "a").await;
        let b = make_folder(&pool, owner, Some(a), "b").await;
        let c = make_folder(&pool, owner, Some(b), "c").await;

        // 往上搬到祖先是安全的
        let moved = update_item(&pool, owner, c, None, Some(Some(a)))
            .await
            .unwrap();

        assert_eq!(moved.parent_id, Some(a));
    }

    #[sqlx::test]
    async fn invalid_targets_are_indistinguishable(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let alice = make_user(&pool, "alice").await;
        let item = make_folder(&pool, owner, None, "item").await;
        let own_file = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();
        let alice_folder = make_folder(&pool, alice, None, "secret").await;

        for target in [own_file.id, alice_folder, 9999] {
            let result = update_item(&pool, owner, item, None, Some(Some(target))).await;
            assert!(
                matches!(result, Err(UpdateError::TargetNotFolder)),
                "{target}"
            );
        }
    }

    #[sqlx::test]
    async fn name_conflicts_on_rename_and_on_move(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, owner, None, "docs").await;
        let a = create_file(&pool, owner, None, "a.txt", 0, None, &key(1))
            .await
            .unwrap();
        create_file(&pool, owner, None, "b.txt", 0, None, &key(2))
            .await
            .unwrap();
        create_file(&pool, owner, Some(docs), "A.TXT", 0, None, &key(3))
            .await
            .unwrap();

        // 改名成同資料夾已有的名稱
        let rename = update_item(&pool, owner, a.id, Some("B.TXT"), None).await;
        // 搬到已有同名（不分大小寫）檔案的資料夾
        let moving = update_item(&pool, owner, a.id, None, Some(Some(docs))).await;

        assert!(matches!(rename, Err(UpdateError::NameConflict)));
        assert!(matches!(moving, Err(UpdateError::NameConflict)));
    }

    #[sqlx::test]
    async fn other_users_item_is_not_found(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let file = create_file(&pool, alice, None, "secret.txt", 0, None, &key(1))
            .await
            .unwrap();

        let result = update_item(&pool, mallory, file.id, Some("pwned.txt"), None).await;

        assert!(matches!(result, Err(UpdateError::NotFound)));
        let unchanged = find_file(&pool, alice, file.id).await.unwrap().unwrap();
        assert_eq!(unchanged.name, "secret.txt");
    }

    /// 兩個分頁同時送出相反的搬移：結果必須恰好一個成功、一個因循環被拒絕，
    /// 而且最後資料庫中不存在循環。
    ///
    /// tokio::join! 讓兩個操作同時進行；連線池有多條連線時，兩個交易會真的在資料庫層競爭。
    /// IMMEDIATE 讓後到的交易在 BEGIN 時排隊，等前一個 commit 後才做循環檢查。
    #[sqlx::test]
    async fn concurrent_opposite_moves_cannot_create_cycle(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let a = make_folder(&pool, owner, None, "a").await;
        let b = make_folder(&pool, owner, None, "b").await;

        let (a_into_b, b_into_a) = tokio::join!(
            update_item(&pool, owner, a, None, Some(Some(b))),
            update_item(&pool, owner, b, None, Some(Some(a))),
        );

        let results = [&a_into_b, &b_into_a];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(results.iter().any(|r| matches!(r, Err(UpdateError::Cycle))));

        // 最終狀態：其中一個在另一個裡面，另一個仍在根目錄
        let state = (
            parent_of(&pool, owner, a).await,
            parent_of(&pool, owner, b).await,
        );
        assert!(
            state == (Some(b), None) || state == (None, Some(a)),
            "{state:?}"
        );
    }

    // ==================== ancestors ====================

    fn ancestor_names(chain: &[Ancestor]) -> Vec<&str> {
        chain.iter().map(|a| a.name.as_str()).collect()
    }

    #[sqlx::test]
    async fn ancestors_are_ordered_from_root(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let a = make_folder(&pool, owner, None, "a").await;
        let b = make_folder(&pool, owner, Some(a), "b").await;
        let c = make_folder(&pool, owner, Some(b), "c").await;
        let file = create_file(&pool, owner, Some(c), "x.txt", 0, None, &key(1))
            .await
            .unwrap();

        assert_eq!(
            ancestor_names(&ancestors(&pool, owner, c).await.unwrap()),
            ["a", "b"]
        );
        assert_eq!(
            ancestor_names(&ancestors(&pool, owner, file.id).await.unwrap()),
            ["a", "b", "c"]
        );
        // 根目錄的項目沒有祖先
        assert!(ancestors(&pool, owner, a).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn ancestors_of_other_users_item_are_empty(pool: SqlitePool) {
        let alice = make_user(&pool, "alice").await;
        let mallory = make_user(&pool, "mallory").await;
        let a = make_folder(&pool, alice, None, "a").await;
        let b = make_folder(&pool, alice, Some(a), "b").await;

        assert!(ancestors(&pool, mallory, b).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn ancestors_terminate_on_corrupted_cycle(pool: SqlitePool) {
        let owner = make_user(&pool, "carol").await;
        let a = make_folder(&pool, owner, None, "a").await;
        let b = make_folder(&pool, owner, Some(a), "b").await;
        // 用 SQL 直接製造循環（搬移 API 會擋下，這裡模擬資料被手動改壞）
        sqlx::query("UPDATE files SET parent_id = ? WHERE id = ?")
            .bind(b)
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();

        let chain = ancestors(&pool, owner, b).await.unwrap();

        // 查詢在深度上限處停止，而不是卡死
        assert_eq!(chain.len(), usize::try_from(MAX_ANCESTOR_DEPTH).unwrap());
    }

    // ==================== 同名的查詢與取代 ====================

    #[sqlx::test]
    async fn existing_kind_is_case_insensitive_and_per_folder(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let docs = make_folder(&pool, carol, None, "docs").await;
        create_file(&pool, carol, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();

        assert_eq!(
            existing_kind(&pool, carol, None, "A.TXT")
                .await
                .unwrap()
                .as_deref(),
            Some("file")
        );
        assert_eq!(
            existing_kind(&pool, carol, None, "DOCS")
                .await
                .unwrap()
                .as_deref(),
            Some("folder")
        );
        // 別的資料夾、別的使用者都沒有
        assert_eq!(
            existing_kind(&pool, carol, Some(docs), "a.txt")
                .await
                .unwrap(),
            None
        );
        let dave = make_user(&pool, "dave").await;
        assert_eq!(
            existing_kind(&pool, dave, None, "a.txt").await.unwrap(),
            None
        );
    }

    #[sqlx::test]
    async fn replace_keeps_the_id_and_swaps_the_content(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        let original = create_file(&pool, carol, None, "a.txt", 1, None, &key(1))
            .await
            .unwrap();

        let outcome = replace_file(&pool, carol, None, "A.TXT", 5, Some("text/plain"), &key(2))
            .await
            .unwrap();

        let ReplaceOutcome::Replaced { entry, old_key } = outcome else {
            panic!("expected Replaced, got {outcome:?}");
        };
        assert_eq!(entry.id, original.id);
        // 名稱維持原本的寫法
        assert_eq!(entry.name, "a.txt");
        assert_eq!(entry.size, Some(5));
        assert_eq!(entry.storage_key.as_deref(), Some(key(2).as_str()));
        assert_eq!(old_key.as_deref(), Some(key(1).as_str()));
    }

    #[sqlx::test]
    async fn replace_reports_missing_and_folders(pool: SqlitePool) {
        let carol = make_user(&pool, "carol").await;
        make_folder(&pool, carol, None, "docs").await;

        assert!(matches!(
            replace_file(&pool, carol, None, "new.txt", 1, None, &key(1))
                .await
                .unwrap(),
            ReplaceOutcome::Missing
        ));
        assert!(matches!(
            replace_file(&pool, carol, None, "docs", 1, None, &key(1))
                .await
                .unwrap(),
            ReplaceOutcome::IsFolder
        ));
    }
}
