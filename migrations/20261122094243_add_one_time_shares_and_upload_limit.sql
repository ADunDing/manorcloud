-- 只能下載一次的分享連結，以及在管理頁面設定的上傳上限

-- 1：第一次下載開始時連結就失效（只用在檔案的分享）
ALTER TABLE shares ADD COLUMN one_time INTEGER NOT NULL DEFAULT 0 CHECK (one_time IN (0, 1));

-- 整個伺服器的設定，在管理頁面修改：只有一列（id = 1）
CREATE TABLE server_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    -- 單一檔案的上傳上限（位元組）。NULL：使用設定檔的 MANORCLOUD_MAX_UPLOAD_BYTES（預設 10 GiB）
    max_upload_bytes INTEGER CHECK (max_upload_bytes IS NULL OR max_upload_bytes > 0)
);

INSERT INTO server_settings (id) VALUES (1);
