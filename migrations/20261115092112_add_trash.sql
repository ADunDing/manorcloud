-- 垃圾桶：刪除改成「放進垃圾桶」，可以還原；超過保留期限由垃圾回收永久刪除。
--
-- trashed_at：放進垃圾桶的時間（Unix 秒數）。NULL 代表不在垃圾桶。
--   刪除一個資料夾時，它與底下的所有內容都標上同一個時間：
--   所有一般的查詢只要加上 trashed_at IS NULL，就看不到、也操作不到它們。
-- trash_root：被刪除的「那一個」項目是 1，一起進入垃圾桶的內容是 0。
--   垃圾桶的清單只顯示 trash_root = 1 的項目；還原時從它往下，只還原同一次放進去的內容
--   （資料夾裡先前已經被個別刪除的項目，自己也是 trash_root，維持在垃圾桶中）。
ALTER TABLE files ADD COLUMN trashed_at INTEGER;
ALTER TABLE files ADD COLUMN trash_root INTEGER NOT NULL DEFAULT 0 CHECK (trash_root IN (0, 1));

-- 「同一資料夾內名稱不重複」只適用於不在垃圾桶的項目：
-- 刪掉 報告.docx 之後，可以立刻再建立一個同名的檔案。
-- 還原時若原處已有同名的項目，由應用程式自動改名為「名稱 (1)」
DROP INDEX idx_files_unique_name_in_folder;
DROP INDEX idx_files_unique_name_in_root;

CREATE UNIQUE INDEX idx_files_unique_name_in_folder
    ON files(parent_id, name)
    WHERE parent_id IS NOT NULL AND trashed_at IS NULL;

CREATE UNIQUE INDEX idx_files_unique_name_in_root
    ON files(owner_id, name)
    WHERE parent_id IS NULL AND trashed_at IS NULL;

-- 垃圾桶的清單與垃圾回收的過期查詢
CREATE INDEX idx_files_trash
    ON files(owner_id, trashed_at)
    WHERE trash_root = 1;
