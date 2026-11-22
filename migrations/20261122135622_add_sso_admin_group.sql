-- 單一登入的管理員群組：身分提供者中屬於這個群組的人，用 SSO 登入時成為管理員，
-- 不屬於的人成為成員。空字串：不依群組決定角色
ALTER TABLE sso_settings ADD COLUMN admin_group TEXT NOT NULL DEFAULT '';
