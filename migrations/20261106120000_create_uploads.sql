-- 分段上傳的進度紀錄。完成或取消時刪除；24 小時沒有進度的由垃圾回收刪除。
--
-- 暫存檔位於 {data_dir}/uploads/{id}.part，檔案長度永遠「不少於」received：
-- 先寫檔案並 fsync，再更新 received。斷電在兩步之間時，檔案多出一段沒被承認的資料，
-- 下次續寫前先截斷到 received，不會出現「資料庫說有、檔案裡卻缺了一段」。
CREATE TABLE uploads (
    -- 32 個小寫 hex 字元，同時是暫存檔的檔名：其他格式一律拒絕，路徑不可能被拼出 ../
    id            TEXT PRIMARY KEY
                  CHECK (length(id) = 32 AND id NOT GLOB '*[^0-9a-f]*'),
    owner_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- NULL 代表根目錄
    parent_id     INTEGER,
    name          TEXT NOT NULL,
    -- 宣告的總大小
    size          INTEGER NOT NULL CHECK (size >= 0),
    -- 已確認收到的位元組數
    received      INTEGER NOT NULL DEFAULT 0 CHECK (received >= 0 AND received <= size),
    -- 使用者電腦上檔案的修改時間（毫秒），重新整理後用來比對是不是同一個檔案
    file_modified INTEGER,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    -- 最後一次有進度的時間：24 小時沒有更新就視為放棄
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),

    -- 與 files 相同的複合外鍵：父資料夾必須屬於同一個使用者。
    -- 資料夾被刪除時，進行中的上傳紀錄一併刪除（暫存檔由垃圾回收清除）
    FOREIGN KEY (parent_id, owner_id) REFERENCES files (id, owner_id) ON DELETE CASCADE
);

-- 列出、計算某人未完成的上傳
CREATE INDEX uploads_owner ON uploads (owner_id);
-- 垃圾回收找出太久沒有進度的上傳
CREATE INDEX uploads_updated_at ON uploads (updated_at);
