//! `manorcloud reset-2fa <帳號>`：在伺服器上直接關閉某個帳號的兩步驟驗證。
//!
//! 給「管理員自己遺失手機、備用碼也找不到」的情況：其他人的 2FA 可以由管理員在網頁上關閉，
//! 但管理員自己無法登入時，只剩能登入伺服器的人能處理。
//! 同時登出那個帳號的所有裝置（撿到手機的人不應該繼續登入著）。
//!
//! 以服務的帳號執行（sudo -u manorcloud），不是 root：
//! SQLite 寫入時會建立 -wal、-shm 檔案，由 root 建立的話，服務之後就無法存取它們。
//! 服務執行中也可以使用：SQLite 允許多個連線，寫入會依序進行。
//!
//! 終端機訊息使用英文。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::{config, db, layout};

pub const USAGE: &str =
    "usage: sudo -u manorcloud manorcloud reset-2fa [--env-file <file>] <username>";

pub async fn run(args: &[OsString]) -> Result<(), String> {
    // SAFETY: geteuid 沒有任何前置條件，只是讀取目前行程的有效使用者編號
    if unsafe { libc::geteuid() } == 0 {
        return Err(
            "run this as the service account, not as root, so the database files \
                    keep the right owner:\n  sudo -u manorcloud manorcloud reset-2fa <username>"
                .to_string(),
        );
    }

    let (env_file, username) = parse_args(args)?;
    let settings = config::load_from_file(&env_file)?;
    let pool = db::init_pool(&settings.database_url).await;

    let user_id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&username)
        .fetch_optional(&pool)
        .await
        .map_err(|e| format!("cannot read the database: {e}"))?;
    let Some(user_id) = user_id else {
        pool.close().await;
        return Err(format!("there is no account named {username:?}"));
    };

    let had_two_factor = db::two_factor::disable(&pool, user_id, true)
        .await
        .map_err(|e| format!("cannot update the database: {e}"))?;
    pool.close().await;

    if had_two_factor {
        println!(
            "two-factor authentication is now off for {username}; all of its devices were logged out.\n\
             Log in with the password, then turn it on again in Settings > Sign-in."
        );
    } else {
        println!("{username} did not have two-factor authentication; nothing was changed.");
    }
    Ok(())
}

fn parse_args(args: &[OsString]) -> Result<(PathBuf, String), String> {
    let mut env_file = PathBuf::from(layout::ENV_FILE);
    let mut username = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--env-file") => {
                env_file = iter
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--env-file needs a path".to_string())?;
            }
            Some(flag) if flag.starts_with('-') => {
                return Err(format!("unknown option: {flag}\n{USAGE}"));
            }
            Some(name) => {
                if username.is_some() {
                    return Err(format!("only one username is allowed\n{USAGE}"));
                }
                username = Some(name.to_string());
            }
            None => return Err(format!("the username is not valid UTF-8\n{USAGE}")),
        }
    }

    Ok((env_file, username.ok_or_else(|| USAGE.to_string())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<(PathBuf, String), String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse_args(&args)
    }

    #[test]
    fn username_and_env_file() {
        assert_eq!(
            parse(&["alston"]).unwrap(),
            (PathBuf::from(layout::ENV_FILE), "alston".to_string())
        );
        assert_eq!(
            parse(&["--env-file", "/tmp/x.env", "alston"]).unwrap(),
            (PathBuf::from("/tmp/x.env"), "alston".to_string())
        );
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(parse(&[]).unwrap_err().contains("usage"));
        assert!(parse(&["a", "b"]).unwrap_err().contains("only one"));
        assert!(
            parse(&["--force", "a"])
                .unwrap_err()
                .contains("unknown option")
        );
        assert!(USAGE.is_ascii());
    }
}
