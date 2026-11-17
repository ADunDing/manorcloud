-- 管理員在每一顆硬碟上的檔案空間（側邊欄的 Disk2 等）。
--
-- 每個空間是一個隱藏的帳號：不能登入（停用、沒有密碼），不出現在使用者清單中，
-- 檔案的上傳、搬移、垃圾桶、垃圾回收等全部沿用一般帳號的做法。
-- 只有管理員可以透過它操作檔案（API 的 space 參數，見 web::extract::FilesUser）。
--
-- 帳號名稱 ~disk-{編號} 含有一般帳號不允許的 ~，不會和真正的帳號衝突；
-- 資料夾名稱 manorcloud.admin 含有一般帳號不允許的 .，也不會。
CREATE TABLE storage_spaces (
    -- storage_volumes.id；0 代表資料目錄（Default）
    volume_key INTEGER PRIMARY KEY CHECK (volume_key >= 0),
    user_id    INTEGER NOT NULL UNIQUE REFERENCES users(id) ON DELETE CASCADE
);
