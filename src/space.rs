//! 空間：一個人在畫面上看到的一組檔案，以及它在硬碟上的位置與權限。
//!
//! 網址的 `space=` 參數決定是哪一個空間（下載是瀏覽器直接開啟的連結，無法加上 header）：
//!
//! | space=           | 誰可以用       | 硬碟上的位置                                      |
//! |------------------|----------------|---------------------------------------------------|
//! | 沒有、`me`       | 所有人         | 一般使用者：`{他的硬碟}/users/{名稱}`；管理員：他的硬碟的最上層 |
//! | `default`、`{n}` | 管理員         | 資料目錄、儲存位置 n 的最上層                      |
//! | `{n}`            | 掛載給他的人   | 儲存位置 n 的最上層（唯讀或可以修改，見 db::mounts） |
//!
//! 硬碟的最上層（管理員看到的樣子）有一個 `users` 資料夾，裡面是每位使用者的 My files：
//! 管理員可以開啟與下載，但不能修改（他自己在那裡的資料夾除外）。
//! 掛載的位置上也可能放著某些人的 My files：掛載的人看不到 `users` 資料夾（is_ignored），
//! 也碰不到裡面的任何路徑（abs）。

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::{
    db::{mounts, storage as storage_db, trash::Place},
    storage::{self, USERS_DIR},
    tree::RelPath,
};

/// 網址中的 `space=`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpaceKey {
    Me,
    /// None：資料目錄
    Volume(Option<i64>),
}

impl SpaceKey {
    pub fn parse(text: Option<&str>) -> Option<SpaceKey> {
        match text {
            None | Some("" | "me") => Some(SpaceKey::Me),
            Some("default") => Some(SpaceKey::Volume(None)),
            Some(text) => text
                .parse()
                .ok()
                .filter(|id| *id > 0)
                .map(|id| SpaceKey::Volume(Some(id))),
        }
    }

    /// 寫回網址參數的形式（記錄在上傳中，之後用同一個方式找回空間）
    pub fn to_param(&self) -> String {
        match self {
            SpaceKey::Me => "me".to_string(),
            SpaceKey::Volume(None) => "default".to_string(),
            SpaceKey::Volume(Some(id)) => id.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Space {
    pub key: SpaceKey,
    /// 硬碟的根目錄。`.manorcloud/` 在這裡
    pub root: PathBuf,
    /// 空間在根目錄中的位置：一般使用者是 `users/{名稱}`，其他是空的
    pub base: RelPath,
    /// 刪除的項目放進哪一個垃圾桶
    pub trash: Place,
    /// 整個空間都不能修改時的原因（正在搬到另一顆硬碟）
    pub read_only: Option<String>,
    /// 硬碟的最上層：`users` 資料夾受保護
    pub disk_root: bool,
    /// 管理員自己在 `users` 中的資料夾（他可以修改）
    pub own_folder: Option<String>,
    /// 掛載給一般使用者的位置：`users` 資料夾不顯示，檔案不算進配額
    pub mount: bool,
    /// 刪除的項目放進垃圾桶。分享給成員的位置沒有垃圾桶：刪除就是永久刪除
    pub keeps_trash: bool,
}

#[derive(Debug)]
pub enum SpaceError {
    NotFound,
    /// 只有管理員可以使用
    AdminOnly,
    /// 硬碟沒有掛載
    Unavailable(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for SpaceError {
    fn from(e: sqlx::Error) -> Self {
        SpaceError::Db(e)
    }
}

/// 不能修改的原因（給使用者看）
pub const USERS_READ_ONLY: &str =
    "People's own files can be opened and downloaded here, but not changed";

/// 唯讀的掛載不能修改的原因
pub const MOUNT_READ_ONLY: &str = "You can open and download files here, but not change them";

/// 掛載的人碰不到的路徑：硬碟最上層的 `users`，裡面是其他人的 My files。
/// 回應時當成「找不到」，不透露那裡有沒有東西
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfReach;

impl Space {
    /// 空間中的路徑 → 相對於根目錄的路徑。
    ///
    /// 每一個檔案操作都經過這裡，所以掛載的人不能碰 `users` 的規則只在這裡檢查一次：
    /// 讀取、下載、打包、縮圖、複製的來源都擋得住（寫入另外由 check_write_in 擋下）
    pub fn abs(&self, rel: &RelPath) -> Result<RelPath, OutOfReach> {
        if self.mount && rel.parts().first().is_some_and(|first| first == USERS_DIR) {
            return Err(OutOfReach);
        }
        Ok(self.base.join(rel))
    }

    /// 可以在這個資料夾中新增、移除項目嗎？不行時回傳原因
    pub fn check_write_in(&self, dir: &RelPath) -> Result<(), String> {
        if let Some(reason) = &self.read_only {
            return Err(reason.clone());
        }
        if self.disk_root
            && let Some(first) = dir.parts().first()
            && first == USERS_DIR
        {
            let own = dir
                .parts()
                .get(1)
                .is_some_and(|folder| self.own_folder.as_deref() == Some(folder.as_str()));
            if !own {
                return Err(USERS_READ_ONLY.to_string());
            }
        }
        Ok(())
    }

    /// 可以改名、搬移、刪除這個項目嗎？
    pub fn check_modify(&self, item: &RelPath) -> Result<(), String> {
        let parent = item
            .parent()
            .ok_or_else(|| "This folder can't be changed".to_string())?;
        if self.disk_root && item.parts() == [USERS_DIR] {
            return Err("The users folder can't be changed".to_string());
        }
        self.check_write_in(&parent)?;
        // 自己在 users 中的資料夾本身也不能改名或刪除
        if self.disk_root && parent.parts() == [USERS_DIR] {
            return Err(USERS_READ_ONLY.to_string());
        }
        Ok(())
    }

    /// 這個資料夾中，畫面上不能動的項目（最上層的 users）
    pub fn is_locked(&self, dir: &RelPath, name: &str) -> bool {
        self.disk_root && dir.is_root() && name == USERS_DIR
    }

    /// 這個資料夾中不顯示的項目：硬碟最上層的 lost+found，以及掛載中的 users
    pub fn is_ignored(&self, dir: &RelPath, name: &str) -> bool {
        dir.is_root()
            && self.base.is_root()
            && (name == "lost+found" || (self.mount && name == USERS_DIR))
    }

    pub fn tmp_dir(&self) -> PathBuf {
        storage::tmp_dir(&self.root)
    }

    pub fn uploads_dir(&self) -> PathBuf {
        storage::uploads_dir(&self.root)
    }
}

/// 一顆硬碟的根目錄（必須有掛載），以及它是不是分享給成員的位置。資料目錄永遠不是
pub async fn volume_root(
    pool: &SqlitePool,
    data_dir: &Path,
    volume_id: Option<i64>,
) -> Result<(PathBuf, bool), SpaceError> {
    let Some(id) = volume_id else {
        return Ok((data_dir.to_path_buf(), false));
    };
    let volume = storage_db::find_volume(pool, id)
        .await?
        .ok_or(SpaceError::NotFound)?;
    let root = PathBuf::from(&volume.path);
    if !storage::volume_ready(&root) {
        return Err(SpaceError::Unavailable(format!(
            "{} isn't available right now. Is the disk connected?",
            volume.name
        )));
    }
    Ok((root, volume.shared))
}

/// 找出這個人要使用的空間，並確認他有權限
pub async fn resolve(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    admin: bool,
    key: SpaceKey,
) -> Result<Space, SpaceError> {
    match key {
        SpaceKey::Me => {
            let location = storage_db::user_location(pool, user_id)
                .await?
                .ok_or(SpaceError::NotFound)?;
            let (root, _) = volume_root(pool, data_dir, location.volume_id).await?;
            let read_only = location.moving.then(|| {
                "Your files are being moved to another disk. You can change them again when it's done."
                    .to_string()
            });
            if admin {
                return Ok(Space {
                    key,
                    root,
                    base: RelPath::default(),
                    trash: Place {
                        volume_id: location.volume_id,
                        owner_id: None,
                    },
                    read_only,
                    disk_root: true,
                    own_folder: Some(location.folder),
                    mount: false,
                    keeps_trash: true,
                });
            }
            let dir = storage::user_dir(&root, &location.folder).ok_or(SpaceError::NotFound)?;
            if !dir.is_dir() {
                std::fs::create_dir_all(&dir).map_err(|e| {
                    SpaceError::Unavailable(format!("Your folder can't be created: {e}"))
                })?;
            }
            let base = RelPath::parse(&format!("{USERS_DIR}/{}", location.folder))
                .map_err(|_| SpaceError::NotFound)?;
            Ok(Space {
                key,
                root,
                base,
                trash: Place {
                    volume_id: location.volume_id,
                    owner_id: Some(user_id),
                },
                read_only,
                disk_root: false,
                own_folder: None,
                mount: false,
                keeps_trash: true,
            })
        }
        SpaceKey::Volume(volume_id) => {
            if !admin {
                return mounted(pool, data_dir, user_id, key, volume_id).await;
            }
            let (root, shared) = volume_root(pool, data_dir, volume_id).await?;
            let own_folder = storage_db::user_location(pool, user_id)
                .await?
                .filter(|location| location.volume_id == volume_id)
                .map(|location| location.folder);
            Ok(Space {
                key,
                root,
                base: RelPath::default(),
                trash: Place {
                    volume_id,
                    owner_id: None,
                },
                read_only: None,
                disk_root: true,
                own_folder,
                mount: false,
                keeps_trash: !shared,
            })
        }
    }
}

/// 一般使用者打開一顆硬碟：只有掛載給他的位置可以。
/// 資料目錄（`default`）永遠不能掛載：那裡是每個人的 My files
async fn mounted(
    pool: &SqlitePool,
    data_dir: &Path,
    user_id: i64,
    key: SpaceKey,
    volume_id: Option<i64>,
) -> Result<Space, SpaceError> {
    let Some(id) = volume_id else {
        return Err(SpaceError::AdminOnly);
    };
    let can_write = mounts::access(pool, user_id, id)
        .await?
        .ok_or(SpaceError::AdminOnly)?;
    let (root, shared) = volume_root(pool, data_dir, volume_id).await?;
    // 掛載只存在於分享的位置（db::mounts 設定時檢查）；這裡再確認一次
    if !shared {
        return Err(SpaceError::AdminOnly);
    }
    Ok(Space {
        key,
        root,
        base: RelPath::default(),
        // 分享的位置沒有垃圾桶（keeps_trash）；這只是形式上的值
        trash: Place {
            volume_id,
            owner_id: None,
        },
        read_only: (!can_write).then(|| MOUNT_READ_ONLY.to_string()),
        // users 資料夾不能修改（它也不會顯示，見 is_ignored）
        disk_root: true,
        own_folder: None,
        mount: true,
        keeps_trash: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).unwrap()
    }

    fn disk_root(own: Option<&str>) -> Space {
        Space {
            key: SpaceKey::Volume(None),
            root: PathBuf::from("/data"),
            base: RelPath::default(),
            trash: Place {
                volume_id: None,
                owner_id: None,
            },
            read_only: None,
            disk_root: true,
            own_folder: own.map(str::to_string),
            mount: false,
            keeps_trash: true,
        }
    }

    #[test]
    fn space_keys_round_trip() {
        for (text, key) in [
            ("me", SpaceKey::Me),
            ("default", SpaceKey::Volume(None)),
            ("3", SpaceKey::Volume(Some(3))),
        ] {
            assert_eq!(SpaceKey::parse(Some(text)), Some(key.clone()));
            assert_eq!(key.to_param(), text);
        }
        assert_eq!(SpaceKey::parse(None), Some(SpaceKey::Me));
        for bad in ["0", "-1", "x7", "abc"] {
            assert_eq!(SpaceKey::parse(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn the_users_folder_is_read_only_for_administrators() {
        let space = disk_root(Some("carol"));

        assert!(space.check_write_in(&rel("")).is_ok());
        assert!(space.check_write_in(&rel("相片")).is_ok());
        assert!(space.check_write_in(&rel("users")).is_err());
        assert!(space.check_write_in(&rel("users/jerry/a")).is_err());
        // 自己的資料夾裡可以
        assert!(space.check_write_in(&rel("users/carol")).is_ok());
        assert!(space.check_modify(&rel("users/carol/a.txt")).is_ok());
        // users 本身、每個人的資料夾本身都不能動
        assert!(space.check_modify(&rel("users")).is_err());
        assert!(space.check_modify(&rel("users/carol")).is_err());
        assert!(space.check_modify(&rel("users/jerry")).is_err());
        assert!(space.check_modify(&rel("")).is_err());
        assert!(space.is_locked(&rel(""), "users"));
        assert!(!space.is_locked(&rel("相片"), "users"));

        let mut read_only = disk_root(None);
        read_only.read_only = Some("moving".into());
        assert_eq!(read_only.check_write_in(&rel("")), Err("moving".into()));
    }
}
