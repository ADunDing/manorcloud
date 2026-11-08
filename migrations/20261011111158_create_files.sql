-- 檔案與資料夾。
--
-- 設計：資料庫保存名稱與樹狀結構，磁碟上只用隨機的 storage_key 存放內容。
-- 使用者輸入的名稱永遠不會進入檔案系統路徑，從結構上消除路徑穿越。
--
-- 資料庫層保證的規則：
--   1. 父項目必須屬於同一個使用者（複合外鍵）
--   2. 父項目必須是資料夾（trigger）
--   3. 檔案種類建立後不可修改（trigger）
--   4. 同一資料夾內名稱不重複，且不分大小寫（部分唯一索引 + NOCASE）
--   5. 檔案與資料夾的欄位一致、storage_key 格式正確（CHECK）
--
-- 由應用程式負責的規則：
--   - 搬移資料夾時不可形成迴圈（搬進自己的子孫）
--   - 檔名的內容驗證（validate_file_name）
--
-- 孤兒檔案：使用者或資料夾被刪除時，CASCADE 會刪掉資料庫的列，
-- 但磁碟上的 blob 不會自動消失。之後以「垃圾回收」處理：
-- 定期掃描資料目錄，刪除在 files 表中找不到 storage_key 的 blob，
-- 並保留一段寬限期，避免誤刪正在上傳、尚未寫入資料庫的檔案。
CREATE TABLE files (
    id          INTEGER PRIMARY KEY,

    -- 擁有者；使用者刪除時，其所有檔案的資料庫紀錄一併刪除
    owner_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- 父資料夾；NULL 代表位於根目錄。外鍵定義在下方（與 owner_id 組成複合外鍵）
    parent_id   INTEGER,

    -- 顯示名稱，保留使用者輸入的大小寫，但比較時不分大小寫。
    -- 決定理由：檔案下載到 macOS／Windows（預設不分大小寫）時，
    -- README 與 readme 會互相覆蓋；對使用者而言它們也是同一個名稱。
    -- 限制：NOCASE 只處理 ASCII 的 A-Z，非 ASCII 字母（如 É/é）仍視為不同。
    name        TEXT    NOT NULL COLLATE NOCASE,

    kind        TEXT    NOT NULL CHECK (kind IN ('file', 'folder')),

    -- 以下三欄只有檔案才有，資料夾一律為 NULL
    size        INTEGER,
    mime        TEXT,
    storage_key TEXT    UNIQUE,

    -- Unix 秒數；updated_at 由應用程式在修改時更新
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch()),

    -- 複合外鍵的被參照端必須有唯一約束。id 本身已唯一，這組約束只是讓 SQLite 允許參照；
    -- 它的索引以 owner_id 開頭，也順便加速了「刪除使用者時依 owner_id 查找」的 CASCADE
    UNIQUE (owner_id, id),

    -- 父資料夾必須屬於同一個使用者：(我的擁有者, 我的父) 必須等於 (某列的擁有者, 某列的 id)。
    -- parent_id 為 NULL（根目錄）時，SQLite 不檢查複合外鍵，符合需求
    FOREIGN KEY (owner_id, parent_id) REFERENCES files(owner_id, id) ON DELETE CASCADE,

    -- 檔案與資料夾的欄位一致性
    CHECK (
        (kind = 'file'   AND storage_key IS NOT NULL AND size IS NOT NULL AND size >= 0)
     OR (kind = 'folder' AND storage_key IS NULL AND size IS NULL AND mime IS NULL)
    ),

    -- 深度防禦：storage_key 必須恰好是 32 個小寫 hex 字元
    CHECK (
        storage_key IS NULL
     OR (length(storage_key) = 32 AND storage_key NOT GLOB '*[^0-9a-f]*')
    )
);

-- 同一資料夾內名稱不重複（索引沿用 name 欄位的 NOCASE，所以不分大小寫）。
-- 不能寫成 UNIQUE(owner_id, parent_id, name)：SQL 中 NULL 不等於 NULL，
-- 根目錄（parent_id 為 NULL）的同名檔案會全部通過。所以拆成兩個部分索引。
CREATE UNIQUE INDEX idx_files_unique_name_in_folder
    ON files(parent_id, name)
    WHERE parent_id IS NOT NULL;

CREATE UNIQUE INDEX idx_files_unique_name_in_root
    ON files(owner_id, name)
    WHERE parent_id IS NULL;

-- 父項目必須是資料夾（新增時）。
-- 父項目不存在時子查詢為 NULL，`IS NOT` 會把它當成「不是資料夾」而拒絕。
CREATE TRIGGER files_parent_must_be_folder_on_insert
BEFORE INSERT ON files
WHEN NEW.parent_id IS NOT NULL
 AND (SELECT kind FROM files WHERE id = NEW.parent_id) IS NOT 'folder'
BEGIN
    SELECT RAISE(ABORT, 'parent must be a folder');
END;

-- 父項目必須是資料夾（搬移時）
CREATE TRIGGER files_parent_must_be_folder_on_update
BEFORE UPDATE OF parent_id ON files
WHEN NEW.parent_id IS NOT NULL
 AND (SELECT kind FROM files WHERE id = NEW.parent_id) IS NOT 'folder'
BEGIN
    SELECT RAISE(ABORT, 'parent must be a folder');
END;

-- 種類建立後不可修改，避免「有子項目的資料夾被改成檔案」
CREATE TRIGGER files_kind_is_immutable
BEFORE UPDATE OF kind ON files
WHEN NEW.kind IS NOT OLD.kind
BEGIN
    SELECT RAISE(ABORT, 'kind cannot be changed');
END;