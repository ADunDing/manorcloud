-- 儲存空間配額與分享連結

-- 一般使用者最多可以使用多少空間（位元組）。NULL：不限制。管理員不受限制
ALTER TABLE users ADD COLUMN quota_bytes INTEGER CHECK (quota_bytes IS NULL OR quota_bytes >= 0);

-- 分享連結：任何拿到連結的人都可以瀏覽與下載（唯讀）
CREATE TABLE shares (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 網址中的隨機代碼
    token TEXT NOT NULL UNIQUE,
    owner_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 與網址參數 space= 相同：me 是分享者自己的 My files，default 或編號是管理員的硬碟
    space TEXT NOT NULL,
    -- 空間中的路徑；檔案改名或搬走之後，連結就失效
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'folder')),
    -- 有設定密碼時是 Argon2 雜湊
    password_hash TEXT,
    -- 到期時間（Unix 秒）；NULL：不會到期
    expires_at INTEGER,
    downloads INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE INDEX shares_owner ON shares (owner_id);
