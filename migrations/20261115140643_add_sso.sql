-- SSO：以 OpenID Connect 登入（Authentik 等）。
--
-- 設計：
-- - 帳號與 SSO 身分的對應只認「發行者 + 使用者編號」（issuer + subject），不認 email：
--   email 可以在身分提供者中被修改，用它對應會讓人冒用別人的帳號
-- - 只用 SSO 的帳號在 password_credentials 中沒有資料，不能用密碼登入（見 accounts 的 migration）
-- - 至少要有一個「能用密碼登入」的有效管理員：身分提供者掛掉時，還能登入修改設定

-- 1. 設定：只有一列（id = 1），由管理員在 Administration → Sign-in 修改
CREATE TABLE sso_settings (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    enabled       INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    -- 登入畫面的按鈕文字：Sign in with {display_name}
    display_name  TEXT    NOT NULL,
    issuer_url    TEXT    NOT NULL,
    client_id     TEXT    NOT NULL,
    -- 只有服務本身能讀取資料庫檔案（0600）；API 永遠不回傳它
    client_secret TEXT    NOT NULL,
    -- ManorCloud 對外的網址，Redirect URI 由它組成
    public_url    TEXT    NOT NULL,
    -- 第一次用 SSO 登入、但還沒有帳號時：1 自動建立為成員，0 拒絕
    auto_create   INTEGER NOT NULL DEFAULT 0 CHECK (auto_create IN (0, 1)),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

-- 2. 帳號與 SSO 身分的對應：一個帳號最多一個，一個身分只能對應一個帳號
CREATE TABLE sso_identities (
    user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    issuer     TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (issuer, subject)
);

-- 3. 進行中的登入流程。state 只存雜湊；每個流程只能使用一次，最多 10 分鐘
CREATE TABLE sso_flows (
    state_hash    BLOB    PRIMARY KEY,
    nonce         TEXT    NOT NULL,
    pkce_verifier TEXT    NOT NULL,
    -- 連結既有帳號時是那個帳號；一般登入是 NULL
    link_user_id  INTEGER REFERENCES users(id) ON DELETE CASCADE,
    expires_at    INTEGER NOT NULL
);

-- 4. 至少一個能用密碼登入的有效管理員。
--    原本的 trigger 保證「至少一個有效的管理員」；把只用 SSO 的帳號升為管理員之後，
--    那條規則仍然成立，卻可能沒有任何管理員能在身分提供者掛掉時登入。
--    錯誤訊息與原本的相同：管理員 API 已經把它對應成清楚的說明
CREATE TRIGGER users_keep_one_password_admin_on_update
BEFORE UPDATE OF role, disabled ON users
WHEN OLD.role = 'admin' AND OLD.disabled = 0
 AND (NEW.role != 'admin' OR NEW.disabled != 0)
 AND EXISTS (SELECT 1 FROM password_credentials WHERE user_id = OLD.id)
 AND (SELECT COUNT(*) FROM users
      JOIN password_credentials ON password_credentials.user_id = users.id
      WHERE users.role = 'admin' AND users.disabled = 0) <= 1
BEGIN
    SELECT RAISE(ABORT, 'at least one admin required');
END;

CREATE TRIGGER users_keep_one_password_admin_on_delete
BEFORE DELETE ON users
WHEN OLD.role = 'admin' AND OLD.disabled = 0
 AND EXISTS (SELECT 1 FROM password_credentials WHERE user_id = OLD.id)
 AND (SELECT COUNT(*) FROM users
      JOIN password_credentials ON password_credentials.user_id = users.id
      WHERE users.role = 'admin' AND users.disabled = 0) <= 1
BEGIN
    SELECT RAISE(ABORT, 'at least one admin required');
END;
