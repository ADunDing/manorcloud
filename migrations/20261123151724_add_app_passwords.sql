-- 應用程式密碼與 WebDAV。
--
-- WebDAV 用戶端（Finder、檔案總管、rclone、Obsidian 等）每個請求都帶著帳號與密碼，
-- 而且不能輸入兩步驟驗證碼，所以不用登入密碼，改用每台裝置各自的「應用程式密碼」：
-- 用 SSO 登入的帳號也能用，裝置遺失時只要撤銷那一組。
--
-- 密碼是伺服器產生的隨機字串（100 位元），只存 SHA-256：
-- 隨機字串猜不到，不需要 Argon2 這種刻意變慢的雜湊，每個請求都能很快驗證

CREATE TABLE app_passwords (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 使用者取的名稱，例如 MacBook Finder
    name         TEXT    NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    token_hash   BLOB    NOT NULL UNIQUE,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    -- 最後一次使用的時間（Unix 秒）；NULL：還沒用過
    last_used_at INTEGER
);
CREATE INDEX app_passwords_user ON app_passwords (user_id);

-- 是否開放 WebDAV（/dav/）。預設關閉，由管理員在管理頁面開啟
ALTER TABLE server_settings ADD COLUMN webdav_enabled INTEGER NOT NULL DEFAULT 0
    CHECK (webdav_enabled IN (0, 1));
