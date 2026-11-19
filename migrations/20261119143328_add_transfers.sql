-- 跨硬碟的複製與移動（例如把 Disk2 上的資料夾搬到 My files）。
-- 在背景一個項目接著一個項目進行，伺服器重新啟動後從還沒完成的項目接著做。
--
-- 移動：先完整複製到目的地，確認之後才刪除來源。

CREATE TABLE transfers (
    -- AUTOINCREMENT：編號不重複使用，取消之後殘留的背景工作不會誤認新的紀錄
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind        TEXT    NOT NULL CHECK (kind IN ('copy', 'move')),
    -- 來源：空間（與網址的 space= 相同）、資料夾、選取的名稱（JSON 陣列）
    from_space  TEXT    NOT NULL,
    from_dir    TEXT    NOT NULL,
    names       TEXT    NOT NULL,
    -- 目的地：空間與資料夾
    to_space    TEXT    NOT NULL,
    to_dir      TEXT    NOT NULL,
    -- 已經完成的項目數（names 的前幾個）
    done_items  INTEGER NOT NULL DEFAULT 0,
    -- 移動時，目前這個項目已經放到目的地、還沒刪除來源：重新啟動後只需要刪除來源
    placed      INTEGER NOT NULL DEFAULT 0 CHECK (placed IN (0, 1)),
    total_bytes INTEGER NOT NULL DEFAULT 0,
    done_bytes  INTEGER NOT NULL DEFAULT 0,
    state       TEXT    NOT NULL DEFAULT 'running'
                CHECK (state IN ('running', 'done', 'failed', 'cancelled')),
    error       TEXT,
    -- 略過的項目（符號連結、特殊檔案），JSON 陣列
    skipped     TEXT    NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX transfers_owner ON transfers (owner_id);
