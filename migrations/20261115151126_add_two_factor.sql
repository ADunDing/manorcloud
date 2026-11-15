-- Add migration script here
-- 兩步驟驗證（2FA）：TOTP 驗證碼與備用碼。
--
-- 設計：
-- - 只用於密碼登入。SSO 登入由身分提供者（Authentik 等）負責，ManorCloud 不重複要求
-- - 一個帳號一組 TOTP。設定時先存成「未啟用」，使用者輸入正確的驗證碼確認之後才啟用：
--   掃錯 QR code 不會把自己鎖在外面
-- - 之後加入 Passkey（指紋、Face ID）時會是另一張表，與這裡互不影響

-- 1. TOTP
CREATE TABLE totp_credentials (
    user_id        INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    -- 原始的密鑰（20 位元組）。與密碼雜湊不同，驗證時需要原值，所以無法只存雜湊；
    -- 資料庫檔案只有服務本身能讀取
    secret         BLOB    NOT NULL CHECK (length(secret) >= 16),
    enabled        INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    -- 最後一次使用的時間段：之前（含）的驗證碼一律不接受，同一個驗證碼不能用兩次
    last_used_step INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch())
);

-- 2. 備用碼：只存雜湊，用過的記下時間（不刪除，才能顯示「還剩幾組」）
CREATE TABLE recovery_codes (
    user_id   INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash BLOB    NOT NULL,
    used_at   INTEGER,
    PRIMARY KEY (user_id, code_hash)
);

-- 3. 密碼正確、等待輸入驗證碼的登入。token 只存雜湊；最多 5 分鐘、只能用一次。
--    它不是 session：只知道密碼的人拿到它，什麼都不能做
CREATE TABLE two_factor_challenges (
    token_hash BLOB    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at INTEGER NOT NULL,
    -- 這個登入已經猜錯幾次：到上限就作廢，要從帳號密碼重新開始
    failures   INTEGER NOT NULL DEFAULT 0
);

-- 4. 整個系統的安全設定：只有一列（id = 1）
CREATE TABLE security_settings (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    -- 1：每個用密碼登入的人都必須開啟 2FA，沒有開啟的人登入後要先完成設定
    require_two_factor INTEGER NOT NULL DEFAULT 0 CHECK (require_two_factor IN (0, 1))
);

INSERT INTO security_settings (id) VALUES (1);
