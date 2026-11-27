-- 儲存位置的用途：
--   shared = 0：存放成員的個人檔案（他們的 My files 放在這裡，每個人只看得到自己的）
--   shared = 1：分享給成員（整個位置掛載給選定的成員，見 storage_mounts）
--
-- 兩種用途互斥：分享的位置上不放任何人的 My files，存放個人檔案的位置不掛載給任何人。
-- 分享的位置沒有垃圾桶：刪除就是永久刪除。
-- 資料目錄（Default）不在這張表裡，永遠是存放個人檔案。
ALTER TABLE storage_volumes ADD COLUMN shared INTEGER NOT NULL DEFAULT 0 CHECK (shared IN (0, 1));

-- 0.4.4 的掛載：掛載的位置上不會有人的 My files（當時已經互斥），直接標成分享
UPDATE storage_volumes SET shared = 1 WHERE id IN (SELECT volume_id FROM storage_mounts);
