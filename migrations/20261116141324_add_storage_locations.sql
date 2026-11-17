-- 儲存位置：檔案內容（blob）可以放在資料目錄以外的硬碟，由管理員為每個使用者指定。
--
-- 內建的資料目錄不在這張表裡：users.storage_volume_id 為 NULL 就代表它。
-- 每個位置底下的結構與資料目錄相同（tmp/、blobs/、uploads/），
-- tmp/ 與 blobs/ 在同一顆硬碟上，rename 才是原子操作。
CREATE TABLE storage_volumes (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE COLLATE NOCASE
                       CHECK (length(name) BETWEEN 1 AND 40),
    -- 絕對路徑。加入時已經解析過符號連結，之後不會再改變
    path       TEXT    NOT NULL UNIQUE CHECK (substr(path, 1, 1) = '/'),
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

-- 使用者的檔案存在哪個位置。NULL：資料目錄。
-- 沒有 ON DELETE：還有人使用的位置不能刪除（應用程式會先檢查，外鍵是最後一道防線）
ALTER TABLE users ADD COLUMN storage_volume_id INTEGER REFERENCES storage_volumes(id);

-- 更換位置時，把檔案搬過去的進度。每個使用者最多一筆，搬完就刪除。
--
-- 搬移期間：檔案照常從原本的位置讀取；新增檔案（上傳、複製）會被拒絕，
-- 否則它會寫進原本的位置，搬完之後就找不到了。
-- id 讓背景工作認得「自己的那一筆」：取消之後立刻重新開始，舊的工作不會更新到新的紀錄。
-- AUTOINCREMENT：刪掉的編號不會被重新使用（沒有它的話，新紀錄可能拿到剛被取消的那一筆的編號）
CREATE TABLE storage_moves (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id          INTEGER NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE,
    -- 目的地。NULL：資料目錄
    target_volume_id INTEGER REFERENCES storage_volumes(id),
    total_bytes      INTEGER NOT NULL CHECK (total_bytes >= 0),
    moved_bytes      INTEGER NOT NULL DEFAULT 0 CHECK (moved_bytes >= 0),
    -- 失敗的原因。NULL：進行中
    error            TEXT,
    started_at       INTEGER NOT NULL DEFAULT (unixepoch())
);
