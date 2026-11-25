-- 原本在設定檔（/etc/manorcloud/manorcloud.env）的設定，改在管理頁面設定。
-- 升級後第一次啟動時，設定檔裡還有的舊值會自動搬進來（見 web::server_settings::load_settings）

-- 每個使用者最多幾個未完成（可續傳）的上傳。NULL：預設值 5
ALTER TABLE server_settings ADD COLUMN max_pending_uploads INTEGER
    CHECK (max_pending_uploads IS NULL OR max_pending_uploads > 0);

-- 反向代理的位址或網段，以逗號分隔，例如 192.168.10.200, 172.18.0.0/16。空字串：沒有
ALTER TABLE server_settings ADD COLUMN trusted_proxies TEXT NOT NULL DEFAULT '';
