-- 改用真實的檔名：檔案直接放在硬碟上的資料夾中，硬碟上看到的就是全部的檔案。
-- 資料庫只記錄硬碟上記不了的東西：垃圾桶裡的項目原本在哪裡、上傳的進度。
--
-- 舊的 files 與 storage_spaces 改名為 legacy_*。啟動時由 convert.rs 把裡面的檔案
-- 放到硬碟上的真實位置，完成後刪除這兩個表。
--
-- 未完成的上傳與搬移無法沿用（它們記錄的是資料夾的編號），一律取消。

DROP TABLE uploads;
DELETE FROM storage_moves;

ALTER TABLE files RENAME TO legacy_files;
ALTER TABLE storage_spaces RENAME TO legacy_storage_spaces;

-- 分段上傳的進度。暫存檔在 {空間的根目錄}/.manorcloud/uploads/{id}.part，
-- 檔案長度永遠「不少於」received：先寫檔案並 fsync，再更新 received
CREATE TABLE uploads (
    -- 32 個小寫 hex 字元，同時是暫存檔的檔名
    id            TEXT PRIMARY KEY
                  CHECK (length(id) = 32 AND id NOT GLOB '*[^0-9a-f]*'),
    -- 上傳的人
    owner_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 上傳到哪個空間：me、default、儲存位置的編號（與網址的 space= 相同）
    space         TEXT NOT NULL,
    -- 上傳到空間中的哪個資料夾，例如 相片/2026；空字串是空間的最上層
    dir           TEXT NOT NULL,
    name          TEXT NOT NULL,
    -- 宣告的總大小
    size          INTEGER NOT NULL CHECK (size >= 0),
    -- 已確認收到的位元組數
    received      INTEGER NOT NULL DEFAULT 0 CHECK (received >= 0 AND received <= size),
    -- 使用者電腦上檔案的修改時間（毫秒），重新整理後用來比對是不是同一個檔案
    file_modified INTEGER,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    -- 最後一次有進度的時間：24 小時沒有更新就視為放棄
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX uploads_owner ON uploads (owner_id);
CREATE INDEX uploads_updated_at ON uploads (updated_at);

-- 垃圾桶。被刪除的項目搬到 {硬碟}/.manorcloud/trash/{id}（檔案或整個資料夾），
-- 這裡記錄它原本在哪裡，還原時放回去
CREATE TABLE trash (
    -- AUTOINCREMENT：編號不重複使用，避免舊的暫存檔名被新的紀錄誤認
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 在哪一顆硬碟。NULL：資料目錄（Default）
    volume_id  INTEGER REFERENCES storage_volumes (id) ON DELETE CASCADE,
    -- 屬於誰的 My files。NULL：硬碟的最上層（管理員共用）
    owner_id   INTEGER REFERENCES users (id) ON DELETE CASCADE,
    -- 原本的位置，相對於那個空間，例如 相片/2026/a.jpg
    path       TEXT    NOT NULL CHECK (length(path) > 0),
    kind       TEXT    NOT NULL CHECK (kind IN ('file', 'folder')),
    -- 檔案的大小，或資料夾裡所有檔案的大小總和
    size       INTEGER NOT NULL DEFAULT 0 CHECK (size >= 0),
    trashed_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX trash_place ON trash (volume_id, owner_id);
CREATE INDEX trash_owner ON trash (owner_id);

-- 是否顯示以點開頭的隱藏檔（例如 .DS_Store）。預設不顯示
ALTER TABLE users ADD COLUMN show_hidden INTEGER NOT NULL DEFAULT 0 CHECK (show_hidden IN (0, 1));
