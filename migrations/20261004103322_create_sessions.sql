-- Session 資料表：每一列代表一個登入中的裝置
CREATE TABLE sessions (
    -- token 的 SHA-256 雜湊值（32 bytes），直接當主鍵
    token_hash  BLOB    PRIMARY KEY NOT NULL,

    -- 屬於哪個使用者；使用者被刪除時，其所有 session 一併刪除
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- 建立時間與過期時間，皆為 Unix 秒數
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at  INTEGER NOT NULL
);

-- 加速「刪除某使用者的所有 session」與 CASCADE 刪除
CREATE INDEX idx_sessions_user_id ON sessions(user_id);

-- 加速「清除所有過期 session」
CREATE INDEX idx_sessions_expires_at ON sessions(expires_at);