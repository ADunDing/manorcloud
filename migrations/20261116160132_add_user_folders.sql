-- 每個使用者在儲存位置中有自己的資料夾，例如 /mnt/disk2/manorcloud/alice/，
-- 底下是他自己的 blobs/、tmp/、uploads/。同一顆硬碟上的使用者，資料各自分開。
--
-- 資料夾名稱預設是帳號名稱。帳號改名時，資料夾跟著改名；
-- 改名失敗（例如同名的資料夾已經存在）時沿用原本的名稱，所以兩者分開記錄。
-- 帳號名稱只有小寫英文、數字、底線與連字號，不會有 / 或 ..，可以直接當成資料夾名稱。
ALTER TABLE users ADD COLUMN storage_folder TEXT;

UPDATE users SET storage_folder = username;

-- 同一個位置中，資料夾名稱不能重複。資料目錄的 storage_volume_id 是 NULL，
-- UNIQUE 會把每個 NULL 視為不同的值，所以用 IFNULL 換成 0（位置的編號從 1 開始）
CREATE UNIQUE INDEX users_storage_folder_unique
    ON users (IFNULL(storage_volume_id, 0), storage_folder);

-- 新的使用者：資料夾名稱等於帳號名稱。用 trigger 而不是在每個建立帳號的地方設定，
-- 管理員建立、SSO 自動建立、初始管理員都不會漏掉
CREATE TRIGGER users_default_storage_folder
AFTER INSERT ON users
WHEN NEW.storage_folder IS NULL
BEGIN
    UPDATE users SET storage_folder = NEW.username WHERE id = NEW.id;
END;
