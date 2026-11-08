//! `manorcloud uninstall [--purge]`：移除 ManorCloud。
//!
//! 不加 --purge：移除程式與服務，保留設定檔與所有資料（重新安裝即可恢復）。
//! 加上 --purge：連同設定檔與 /var/lib/manorcloud 一起永久刪除。
//!
//! --purge 時「先確認、再動手」：取消就是什麼都沒發生，服務與程式都還在。
//!
//! 刪除的路徑全部是 layout.rs 中寫死的常數，絕不刪除設定檔裡寫的路徑：
//! 以 root 刪除一個可以被設定的路徑太危險（例如有人把資料目錄寫成 /）。
//! 設定指到別處的資料，只會明確告訴使用者「沒有被刪除」。
//!
//! 所有終端機輸出使用英文。

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use unicode_normalization::UnicodeNormalization;

use crate::{config, layout};

pub const USAGE: &str = "usage: manorcloud uninstall [--purge]";

/// 確認時最多詢問幾次：輸入法打出了看不懂的內容時，讓使用者有機會重新輸入
const MAX_ATTEMPTS: usize = 3;

pub fn run(args: &[OsString]) -> Result<(), String> {
    let purge = parse_args(args)?;

    // SAFETY: geteuid 沒有任何前置條件，只是讀取目前行程的有效使用者編號
    if unsafe { libc::geteuid() } != 0 {
        return Err("run with sudo".to_string());
    }

    // 在刪除設定檔之前先讀出它：之後就不知道資料原本設定在哪裡了
    let left_behind = if purge {
        data_outside_state_dir()
    } else {
        Vec::new()
    };

    // 在動任何東西之前先確認
    if purge {
        confirm_purge(&left_behind)?;
    }

    // 1. 停止並停用服務。服務不存在時 systemctl 會失敗，這裡不在乎
    run_quietly("systemctl", &["disable", "--now", layout::SERVICE_NAME]);
    remove_file(layout::UNIT_PATH)?;
    remove_dir(layout::DROPIN_DIR)?;
    run_quietly("systemctl", &["daemon-reload"]);

    // 2. 移除執行檔。Linux 允許刪除正在執行的程式自己：行程會繼續執行到結束
    remove_file(layout::BINARY_PATH)?;
    println!("Removed the ManorCloud program and service.");

    if !purge {
        println!(
            "Kept the configuration ({}) and data ({}).",
            layout::CONFIG_DIR,
            layout::STATE_DIR
        );
        println!("To keep a copy of the files first: sudo manorcloud export <output-dir>");
        return Ok(());
    }

    // 3. 永久刪除。路徑都是寫死的常數，不由任何輸入組成
    remove_dir(layout::STATE_DIR)?;
    remove_dir(layout::CONFIG_DIR)?;
    run_quietly("userdel", &[layout::SERVICE_USER]);
    println!("Deleted {} and {}.", layout::STATE_DIR, layout::CONFIG_DIR);

    if !left_behind.is_empty() {
        println!();
        println!(
            "NOT deleted (outside {}); remove them yourself if no longer needed:",
            layout::STATE_DIR
        );
        for line in &left_behind {
            println!("  {line}");
        }
    }
    Ok(())
}

/// 只接受沒有參數或只有 --purge：多餘或拼錯的參數一律拒絕，
/// 例如 --purg 若被默默當成「不清除」，使用者會以為資料已經刪除
fn parse_args(args: &[OsString]) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [flag] if flag == "--purge" => Ok(true),
        _ => Err(USAGE.to_string()),
    }
}

// ==================== 設定在別處的資料 ====================

/// 設定中指到 STATE_DIR 以外的資料位置，每一項是一行要顯示給使用者的文字。
///
/// 用與伺服器、export 相同的規則讀取設定。設定檔讀不到或格式錯誤時不阻擋移除，
/// 但提醒使用者自行確認：寧可多提醒，不可讓人以為資料都已刪除。
fn data_outside_state_dir() -> Vec<String> {
    let settings = match config::load_from_file(Path::new(layout::ENV_FILE)) {
        Ok(settings) => settings,
        Err(e) => {
            return vec![format!(
                "(cannot read the configuration: {e}; \
                 check MANORCLOUD_DATA_DIR and MANORCLOUD_DATABASE_URL yourself)"
            )];
        }
    };

    let mut locations = vec![layout::resolve(settings.data_dir)];
    if let Ok(db) = config::sqlite_path(&settings.database_url) {
        locations.push(layout::resolve(db));
    }

    let state_dir = Path::new(layout::STATE_DIR);
    locations
        .into_iter()
        .filter(|path| !is_inside(path, state_dir))
        .map(|path| path.display().to_string())
        .collect()
}

/// path 是否實際位於 base 底下。
///
/// - 路徑存在時先解析符號連結，看實際位置（例如 data 是一個指向外接硬碟的連結）
/// - 含 `..` 的路徑一律視為「不在底下」：/var/lib/manorcloud/../x 字面上以 base 開頭，
///   實際卻在外面。寧可多提醒
fn is_inside(path: &Path, base: &Path) -> bool {
    let real: PathBuf = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let real_base: PathBuf = fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());

    !real.components().any(|c| c == Component::ParentDir) && real.starts_with(&real_base)
}

// ==================== 確認 ====================

/// 對確認問題的回答
#[derive(Debug, PartialEq)]
enum Answer {
    Yes,
    No,
    /// 看不懂（例如輸入法打出了注音符號）：再問一次
    Unclear,
}

/// 解讀使用者的回答。
///
/// - 必須打完整的 `yes` 才刪除：這是無法復原的操作，只按一個 `y` 太容易誤觸
/// - `no`、`n`、直接按 Enter 都是取消：提示中的 `[no]` 表示預設是不刪除
/// - NFKC 正規化：中文輸入法打出的全形字母（ｙｅｓ）視為一般字母
/// - 其他任何內容都是「看不懂」，由呼叫端再問一次
fn interpret(raw: &str) -> Answer {
    let normalized: String = raw.nfkc().collect::<String>().trim().to_lowercase();
    match normalized.as_str() {
        "yes" => Answer::Yes,
        "" | "n" | "no" => Answer::No,
        _ => Answer::Unclear,
    }
}

/// 要求確認。
///
/// /dev/tty 以讀寫模式開啟，提示與回答都在終端機上：
/// 即使標準輸入或輸出被重新導向（例如 `> log`），使用者仍看得到提示，回答也一定來自終端機前的人。
/// 沒有終端機（例如在腳本或排程中執行）就拒絕，避免無人值守時誤刪資料。
fn confirm_purge(left_behind: &[String]) -> Result<(), String> {
    const CANCELLED: &str = "cancelled; nothing was removed";

    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| {
            "--purge must be run from a terminal so it can ask for confirmation".to_string()
        })?;

    let mut summary = format!(
        "This will permanently delete:\n  \
         {}   all user files and the database\n  \
         {}       the configuration\n",
        layout::STATE_DIR,
        layout::CONFIG_DIR
    );
    if !left_behind.is_empty() {
        summary.push_str(&format!(
            "\nThese locations are outside {} and will NOT be deleted:\n",
            layout::STATE_DIR
        ));
        for line in left_behind {
            summary.push_str(&format!("  {line}\n"));
        }
    }
    summary
        .push_str("\nTo keep a copy of the files first: sudo manorcloud export <output-dir>\n\n");
    write_tty(&tty, &summary)?;

    // 讀取用的 BufReader 只建立一次：每次重建會丟掉已經讀進緩衝區、還沒處理的內容
    let mut reader = BufReader::new(&tty);

    for _ in 0..MAX_ATTEMPTS {
        write_tty(
            &tty,
            "Delete everything listed above? (yes/no, default: no):  ",
        )?;

        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        // 讀到結尾（例如按了 Ctrl+D）：視為取消
        if read == 0 {
            return Err(CANCELLED.to_string());
        }

        match interpret(&line) {
            Answer::Yes => return Ok(()),
            Answer::No => return Err(CANCELLED.to_string()),
            Answer::Unclear => write_tty(&tty, "Please type yes or no.\n")?,
        }
    }

    // 問了幾次都看不懂：保守地取消
    Err(CANCELLED.to_string())
}

fn write_tty(mut tty: &std::fs::File, text: &str) -> Result<(), String> {
    tty.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    tty.flush().map_err(|e| e.to_string())
}

// ==================== 檔案操作 ====================

fn run_quietly(program: &str, args: &[&str]) {
    let _ = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// 刪除檔案；本來就不存在也算成功（目標是「不存在」）
fn remove_file(path: &str) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("cannot delete {path}: {e}")),
        _ => Ok(()),
    }
}

fn remove_dir(path: &str) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!("cannot delete {path}: {e}")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    // ==================== 參數 ====================

    #[test]
    fn no_arguments_keeps_data() {
        assert_eq!(parse_args(&args(&[])), Ok(false));
    }

    #[test]
    fn purge_flag() {
        assert_eq!(parse_args(&args(&["--purge"])), Ok(true));
    }

    #[test]
    fn misspelled_or_extra_arguments_are_rejected() {
        // 拼錯的 --purge 不能被默默當成「保留資料」
        for list in [
            &["--purg"][..],
            &["purge"],
            &["--purge", "--force"],
            &["--purge", "--purge"],
        ] {
            assert!(parse_args(&args(list)).is_err(), "{list:?}");
        }
    }

    #[test]
    fn usage_is_ascii() {
        assert!(USAGE.is_ascii());
    }

    // ==================== 確認的回答 ====================

    #[test]
    fn yes_must_be_the_whole_word() {
        for answer in ["yes\n", "YES\n", "  Yes  \n", "ｙｅｓ\n"] {
            assert_eq!(interpret(answer), Answer::Yes, "{answer:?}");
        }
        // 只按一個 y 不夠：這是無法復原的操作
        assert_eq!(interpret("y\n"), Answer::Unclear);
    }

    #[test]
    fn no_and_enter_cancel() {
        for answer in ["\n", "no\n", "N\n", "ｎｏ\n", "   \n"] {
            assert_eq!(interpret(answer), Answer::No, "{answer:?}");
        }
    }

    #[test]
    fn anything_else_is_asked_again() {
        // 輸入法打出的注音符號、拼錯的字、舊版的 DELETE
        for answer in ["ㄙ\n", "yess\n", "delete\n", "DELETE\n", "是\n"] {
            assert_eq!(interpret(answer), Answer::Unclear, "{answer:?}");
        }
    }

    // ==================== 設定在別處的資料 ====================

    #[test]
    fn paths_inside_and_outside() {
        let base = Path::new("/var/lib/manorcloud");

        assert!(is_inside(Path::new("/var/lib/manorcloud/data"), base));
        assert!(is_inside(
            Path::new("/var/lib/manorcloud/manorcloud.db"),
            base
        ));
        assert!(!is_inside(Path::new("/mnt/usb/manorcloud"), base));
        // 字首相同但不是同一個目錄
        assert!(!is_inside(Path::new("/var/lib/manorcloud-old/data"), base));
    }

    #[test]
    fn parent_components_count_as_outside() {
        // 字面上以 base 開頭，實際卻在外面：寧可多提醒
        let base = Path::new("/var/lib/manorcloud");
        assert!(!is_inside(Path::new("/var/lib/manorcloud/../other"), base));
    }

    #[test]
    fn symlink_to_elsewhere_counts_as_outside() {
        let base = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = base.path().join("data");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();

        // 路徑在 base 底下，但實際的資料在別處
        assert!(!is_inside(&link, base.path()));
    }
}
