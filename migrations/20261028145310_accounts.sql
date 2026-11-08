-- 帳號系統：角色、停用，以及身分與密碼的分離。
--
-- 設計：
-- - users 只描述「你是誰」；password_credentials 描述「用密碼證明身分」。
--   將來的 SSO 會是另一張表（oidc_identities），只用 SSO 的帳號在 password_credentials 中沒有資料。
--   SQLite 無法用 ALTER TABLE 拿掉既有欄位的 NOT NULL，若密碼留在 users，
--   日後就得重建整張 users（它被 sessions 與 files 參照），所以趁現在拆開。
-- - must_change_password 屬於密碼：它描述「這組密碼是臨時的」，與身分無關。
-- - 至少要有一個「有效的管理員」（role = 'admin' 且未停用），由 trigger 保證。
--   SQLite 一次只有一個寫入者，trigger 在寫入當下檢查，
--   所以兩個管理員同時互相降級，第二個操作一定會看到第一個的結果而被擋下。

-- 1. 角色與停用
ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'member'
    CHECK (role IN ('admin', 'member'));

ALTER TABLE users ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0
    CHECK (disabled IN (0, 1));

-- 既有資料：最早註冊的使用者成為管理員（全新安裝時沒有任何使用者，這行不影響任何資料）
UPDATE users SET role = 'admin' WHERE id = (SELECT MIN(id) FROM users);

-- 2. 密碼移到獨立的表
CREATE TABLE password_credentials (
    -- 一個使用者最多一組密碼；使用者刪除時一併刪除
    user_id              INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    password_hash        TEXT    NOT NULL,
    -- 臨時密碼（初始管理員、管理員建立或重設的密碼）：登入後必須先修改
    must_change_password INTEGER NOT NULL DEFAULT 0 CHECK (must_change_password IN (0, 1)),
    updated_at           INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT INTO password_credentials (user_id, password_hash)
SELECT id, password_hash FROM users;

-- SQLite 3.35 起支援 DROP COLUMN；password_hash 沒有索引或約束，可以直接移除
ALTER TABLE users DROP COLUMN password_hash;

-- 3. 至少一個有效的管理員
-- 「有效」= 角色是 admin 且未停用。降級與停用都會讓有效管理員減少一個。
CREATE TRIGGER users_keep_one_admin_on_update
BEFORE UPDATE OF role, disabled ON users
WHEN OLD.role = 'admin' AND OLD.disabled = 0
 AND (NEW.role != 'admin' OR NEW.disabled != 0)
 AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1
BEGIN
    SELECT RAISE(ABORT, 'at least one admin required');
END;

CREATE TRIGGER users_keep_one_admin_on_delete
BEFORE DELETE ON users
WHEN OLD.role = 'admin' AND OLD.disabled = 0
 AND (SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled = 0) <= 1
BEGIN
    SELECT RAISE(ABORT, 'at least one admin required');
END;-- Add migration script here
