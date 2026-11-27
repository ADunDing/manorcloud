-- 掛載：把整個儲存位置交給指定的一般使用者使用，出現在他們側邊欄的 My files 下方。
--
-- 一個位置可以掛給多人，每個人各自是唯讀或可以修改。管理員本來就能看到每一顆硬碟，不需要掛載。
-- 掛載的位置不能同時放任何人的 My files（應用程式在兩邊都會檢查）：
-- 否則硬碟最上層的 users 資料夾裡，是其他人的私人檔案。
-- 檔案不算進任何人的配額。
CREATE TABLE storage_mounts (
    volume_id INTEGER NOT NULL REFERENCES storage_volumes (id) ON DELETE CASCADE,
    user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- 1：可以上傳、改名、刪除；0：只能瀏覽與下載
    can_write INTEGER NOT NULL CHECK (can_write IN (0, 1)),
    PRIMARY KEY (volume_id, user_id)
);

CREATE INDEX storage_mounts_user ON storage_mounts (user_id);
