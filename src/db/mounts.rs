//! 掛載（storage_mounts）：分享給成員的儲存位置交給哪些人使用，各自可檢視或可編輯。
//!
//! 只有用途是「分享給成員」的位置可以掛載（storage_volumes.shared），
//! 這種位置上不放任何人的 My files（見 db::storage 的 set_shared 與 start_move）。
//! 掛載的人也碰不到最上層的 `users` 資料夾（見 `Space::abs`），多一道防線。
//!
//! 與 storage.rs 相同，使用執行期查詢而不是 query! 巨集。

use sqlx::{FromRow, SqlitePool};

/// 一個人可以使用的掛載
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Mount {
    pub volume_id: i64,
    pub name: String,
    pub can_write: bool,
}

/// 一個位置的一個掛載對象
#[derive(Debug, Clone, Copy, PartialEq, FromRow)]
pub struct Grant {
    pub user_id: i64,
    pub can_write: bool,
}

/// 這個人可以使用的掛載，依名稱排序
pub async fn of_user(pool: &SqlitePool, user_id: i64) -> Result<Vec<Mount>, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.volume_id, v.name, m.can_write FROM storage_mounts AS m \
         JOIN storage_volumes AS v ON v.id = m.volume_id \
         WHERE m.user_id = ? ORDER BY v.name, v.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// 這個人能不能使用這個位置。None：沒有掛載給他；Some(can_write)
pub async fn access(
    pool: &SqlitePool,
    user_id: i64,
    volume_id: i64,
) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT can_write FROM storage_mounts WHERE user_id = ? AND volume_id = ?")
        .bind(user_id)
        .bind(volume_id)
        .fetch_optional(pool)
        .await
}

/// 這個位置掛載給了誰，依使用者編號排序
pub async fn of_volume(pool: &SqlitePool, volume_id: i64) -> Result<Vec<Grant>, sqlx::Error> {
    sqlx::query_as(
        "SELECT user_id, can_write FROM storage_mounts WHERE volume_id = ? ORDER BY user_id",
    )
    .bind(volume_id)
    .fetch_all(pool)
    .await
}

#[derive(Debug)]
pub enum SetMountsError {
    VolumeNotFound,
    /// 位置的用途是存放成員的個人檔案：不能分享
    NotShared,
    /// 不存在的使用者，或不是一般使用者（管理員本來就能看到每一顆硬碟）
    NotAMember,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for SetMountsError {
    fn from(e: sqlx::Error) -> Self {
        SetMountsError::Db(e)
    }
}

/// 設定一個位置掛載給誰：整份取代。空的清單代表不再掛載給任何人
pub async fn set_for_volume(
    pool: &SqlitePool,
    volume_id: i64,
    grants: &[Grant],
) -> Result<(), SetMountsError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    let shared: Option<bool> =
        sqlx::query_scalar("SELECT shared FROM storage_volumes WHERE id = ?")
            .bind(volume_id)
            .fetch_optional(&mut *tx)
            .await?;
    match shared {
        None => return Err(SetMountsError::VolumeNotFound),
        Some(false) if !grants.is_empty() => return Err(SetMountsError::NotShared),
        Some(_) => {}
    }

    sqlx::query("DELETE FROM storage_mounts WHERE volume_id = ?")
        .bind(volume_id)
        .execute(&mut *tx)
        .await?;
    for grant in grants {
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id = ? AND role = 'member')",
        )
        .bind(grant.user_id)
        .fetch_one(&mut *tx)
        .await?;
        if !member {
            return Err(SetMountsError::NotAMember);
        }
        sqlx::query(
            "INSERT INTO storage_mounts (volume_id, user_id, can_write) VALUES (?, ?, ?) \
             ON CONFLICT (volume_id, user_id) DO UPDATE SET can_write = excluded.can_write",
        )
        .bind(volume_id)
        .bind(grant.user_id)
        .bind(grant.can_write)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// 每個位置掛載給了幾個人：(位置的編號, 人數)
pub async fn counts(pool: &SqlitePool) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as("SELECT volume_id, count(*) FROM storage_mounts GROUP BY volume_id")
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{storage as storage_db, user};

    /// 用途是分享給成員的位置
    async fn volume(pool: &SqlitePool, name: &str) -> i64 {
        let id = storage_db::add_volume(pool, name, &format!("/mnt/{name}"))
            .await
            .unwrap()
            .id;
        storage_db::set_shared(pool, id, true).await.unwrap();
        id
    }

    async fn member(pool: &SqlitePool, name: &str) -> i64 {
        user::create_user(pool, name, "x").await.unwrap()
    }

    fn grant(user_id: i64, can_write: bool) -> Grant {
        Grant { user_id, can_write }
    }

    #[sqlx::test]
    async fn grants_replace_the_previous_ones(pool: SqlitePool) {
        let disk = volume(&pool, "iso").await;
        let carol = member(&pool, "carol").await;
        let jerry = member(&pool, "jerry").await;

        set_for_volume(&pool, disk, &[grant(carol, true), grant(jerry, false)])
            .await
            .unwrap();
        assert_eq!(access(&pool, carol, disk).await.unwrap(), Some(true));
        assert_eq!(access(&pool, jerry, disk).await.unwrap(), Some(false));
        assert_eq!(
            of_user(&pool, jerry).await.unwrap(),
            [Mount {
                volume_id: disk,
                name: "iso".to_string(),
                can_write: false
            }]
        );

        set_for_volume(&pool, disk, &[grant(jerry, true)])
            .await
            .unwrap();
        assert_eq!(access(&pool, carol, disk).await.unwrap(), None);
        assert_eq!(of_volume(&pool, disk).await.unwrap(), [grant(jerry, true)]);

        set_for_volume(&pool, disk, &[]).await.unwrap();
        assert!(of_volume(&pool, disk).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn only_members_can_be_given_a_mount(pool: SqlitePool) {
        let disk = volume(&pool, "iso").await;
        let admin = member(&pool, "alice").await;
        sqlx::query("UPDATE users SET role = 'admin' WHERE id = ?")
            .bind(admin)
            .execute(&pool)
            .await
            .unwrap();

        assert!(matches!(
            set_for_volume(&pool, disk, &[grant(admin, true)]).await,
            Err(SetMountsError::NotAMember)
        ));
        assert!(matches!(
            set_for_volume(&pool, disk, &[grant(9999, true)]).await,
            Err(SetMountsError::NotAMember)
        ));
        // 失敗時什麼都沒有改
        assert!(of_volume(&pool, disk).await.unwrap().is_empty());
        assert!(matches!(
            set_for_volume(&pool, 9999, &[]).await,
            Err(SetMountsError::VolumeNotFound)
        ));
    }

    #[sqlx::test]
    async fn the_two_purposes_never_mix(pool: SqlitePool) {
        let disk = volume(&pool, "iso").await;
        let personal = storage_db::add_volume(&pool, "home", "/mnt/home")
            .await
            .unwrap()
            .id;
        let carol = member(&pool, "carol").await;
        let jerry = member(&pool, "jerry").await;

        // 存放個人檔案的位置不能分享
        assert!(matches!(
            set_for_volume(&pool, personal, &[grant(jerry, true)]).await,
            Err(SetMountsError::NotShared)
        ));
        // 分享的位置不能放任何人的 My files
        assert!(matches!(
            storage_db::set_volume(&pool, carol, Some(disk)).await,
            Err(storage_db::StartMoveError::Shared)
        ));
        assert!(matches!(
            storage_db::start_move(&pool, carol, Some(disk), Some(1)).await,
            Err(storage_db::StartMoveError::Shared)
        ));
        // 有人的 My files 在這裡（或正在搬過來）時，不能改成分享
        storage_db::set_volume(&pool, carol, Some(personal))
            .await
            .unwrap();
        assert!(matches!(
            storage_db::set_shared(&pool, personal, true).await,
            Err(storage_db::SetSharedError::HasPeople)
        ));
        storage_db::start_move(&pool, carol, None, Some(1))
            .await
            .unwrap();
        assert!(matches!(
            storage_db::set_shared(&pool, personal, true).await,
            Err(storage_db::SetSharedError::HasPeople)
        ));

        // 分享改回存放個人檔案：所有人的分享都取消
        set_for_volume(&pool, disk, &[grant(jerry, true)])
            .await
            .unwrap();
        storage_db::set_shared(&pool, disk, false).await.unwrap();
        assert!(of_volume(&pool, disk).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn removing_a_location_or_a_user_removes_their_mounts(pool: SqlitePool) {
        let disk = volume(&pool, "iso").await;
        let other = volume(&pool, "photos").await;
        let carol = member(&pool, "carol").await;
        set_for_volume(&pool, disk, &[grant(carol, true)])
            .await
            .unwrap();
        set_for_volume(&pool, other, &[grant(carol, true)])
            .await
            .unwrap();

        storage_db::delete_volume(&pool, disk).await.unwrap();
        assert_eq!(of_user(&pool, carol).await.unwrap().len(), 1);

        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(carol)
            .execute(&pool)
            .await
            .unwrap();
        assert!(of_volume(&pool, other).await.unwrap().is_empty());
    }
}
