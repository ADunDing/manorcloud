//! 所有設定的讀取都集中在這裡，確保規則一致：
//! - 沒有設定或為空字串：使用預設值
//! - 有設定但不合法（包含不是 UTF-8）：回傳錯誤，程式不啟動。
//!   設定寫錯卻默默改用預設值，管理者會以為設定已生效。
//!
//! 設定的「來源」可以替換：伺服器讀取環境變數（由 systemd 從設定檔載入），
//! 匯出工具則自己讀取設定檔。兩者共用 Config::load，驗證規則只有一份。
//!
//! 錯誤訊息會在啟動時印到終端機與 journal，所以使用英文。

use std::collections::HashMap;
use std::env::VarError;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::storage::DEFAULT_MAX_UPLOAD_BYTES;

const DATA_DIR_VAR: &str = "MANORCLOUD_DATA_DIR";
const DEFAULT_DATA_DIR: &str = "./data";

const DATABASE_URL_VAR: &str = "MANORCLOUD_DATABASE_URL";
const DEFAULT_DATABASE_URL: &str = "sqlite://app.db";

const MAX_UPLOAD_VAR: &str = "MANORCLOUD_MAX_UPLOAD_BYTES";

const BIND_VAR: &str = "MANORCLOUD_BIND";
/// 預設只監聽本機：要讓區網其他裝置連入，必須在設定檔中明確寫出 0.0.0.0
/// install.sh 的 DEFAULT_BIND 必須與它一致（安裝完成時顯示的位址）
const DEFAULT_BIND: &str = "127.0.0.1:3000";

const SECURE_COOKIES_VAR: &str = "MANORCLOUD_SECURE_COOKIES";

const MAX_PENDING_UPLOADS_VAR: &str = "MANORCLOUD_MAX_PENDING_UPLOADS";
/// 每個使用者最多幾個未完成的上傳：放棄的上傳不能無限累積佔用磁碟
const DEFAULT_MAX_PENDING_UPLOADS: u32 = 5;

/// 依名稱取得設定值。與 `std::env::var` 相同的形狀，方便直接替換
pub type Lookup<'a> = &'a dyn Fn(&str) -> Result<String, VarError>;

#[derive(Debug, Clone)]
pub struct Config {
    /// 使用者檔案的存放根目錄
    pub data_dir: PathBuf,
    /// 資料庫連線字串。刻意不叫 DATABASE_URL：那個名稱留給 sqlx 的巨集在編譯時使用
    pub database_url: String,
    /// 單一檔案的上傳大小上限（位元組）
    pub max_upload_bytes: u64,
    /// 監聽的位址與埠號
    pub bind_addr: SocketAddr,
    /// Session Cookie 是否加上 Secure（只能在 HTTPS 下開啟）
    pub secure_cookies: bool,
    /// 每個使用者最多幾個未完成（可續傳）的上傳
    pub max_pending_uploads: u32,
}

impl Config {
    /// 從程式的環境變數讀取（伺服器使用）
    pub fn from_env() -> Result<Config, String> {
        Config::load(&|name: &str| std::env::var(name))
    }

    /// 從任意來源讀取並驗證所有設定
    pub fn load(lookup: Lookup<'_>) -> Result<Config, String> {
        let data_dir = read_env_string(DATA_DIR_VAR, lookup(DATA_DIR_VAR))?
            .map(|v| PathBuf::from(v.trim()))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR));
        let database_url = read_env_string(DATABASE_URL_VAR, lookup(DATABASE_URL_VAR))?
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| DEFAULT_DATABASE_URL.to_string());

        Ok(Config {
            data_dir,
            database_url,
            max_upload_bytes: parse_max_upload_bytes(lookup(MAX_UPLOAD_VAR))?,
            bind_addr: parse_bind_addr(lookup(BIND_VAR))?,
            secure_cookies: parse_bool(SECURE_COOKIES_VAR, lookup(SECURE_COOKIES_VAR))?,
            max_pending_uploads: parse_max_pending_uploads(lookup(MAX_PENDING_UPLOADS_VAR))?,
        })
    }
}

/// 解析 systemd EnvironmentFile 的格式：一行一個「名稱=值」。
///
/// - `#` 或 `;` 開頭的行是註解，空行忽略
/// - 值頭尾成對的雙引號或單引號會被去掉
/// - 同一個名稱出現多次時，後面的覆蓋前面的（與 systemd 相同）
///
/// 只解析，不執行任何內容：與 shell 的 `source` 不同。
pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        // 與 systemd 相同：值頭尾成對的雙引號或單引號會被去掉
        let value = ['"', '\'']
            .iter()
            .find_map(|&q| value.strip_prefix(q).and_then(|v| v.strip_suffix(q)))
            .unwrap_or(value);
        values.insert(name.trim().to_string(), value.to_string());
    }
    values
}

/// 讀取安裝後的設定檔並驗證。
///
/// 給 export、uninstall 這類以 sudo 手動執行的管理指令使用：它們不是由 systemd 啟動的，
/// 沒有 systemd 從設定檔載入的環境變數，必須自己讀。
/// 設定檔中有寫的項目優先；沒寫的才看環境變數；最後是預設值。
/// 驗證規則與伺服器啟動時完全相同（同一個 Config::load）。
pub fn load_from_file(env_file: &Path) -> Result<Config, String> {
    let values = match fs::read_to_string(env_file) {
        Ok(text) => parse_env_file(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!(
                "note: {} not found; using environment variables and defaults",
                env_file.display()
            );
            HashMap::new()
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(format!("cannot read {}: run with sudo", env_file.display()));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", env_file.display())),
    };

    Config::load(&|name: &str| match values.get(name) {
        Some(value) => Ok(value.clone()),
        None => std::env::var(name),
    })
}

/// 從 SQLite 連線字串取出檔案路徑：
/// sqlite:///var/lib/x.db、sqlite://app.db、sqlite:app.db，並去掉 ?mode=... 之類的參數
pub fn sqlite_path(url: &str) -> Result<PathBuf, String> {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .ok_or_else(|| format!("unrecognized database location: {url}"))?;
    let path = rest.split('?').next().unwrap_or("");
    if path.is_empty() || path == ":memory:" {
        return Err(format!("the database location is not a file: {url}"));
    }
    Ok(PathBuf::from(path))
}

/// 把取值的結果分成三種情況：沒有設定、有值、不合法
fn read_env_string(name: &str, raw: Result<String, VarError>) -> Result<Option<String>, String> {
    match raw {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!("{name} is not valid UTF-8")),
    }
}

fn parse_max_upload_bytes(raw: Result<String, VarError>) -> Result<u64, String> {
    let Some(value) = read_env_string(MAX_UPLOAD_VAR, raw)? else {
        return Ok(DEFAULT_MAX_UPLOAD_BYTES);
    };
    let value = value.trim();

    match value.parse::<u64>() {
        Ok(0) | Err(_) => Err(format!(
            "{MAX_UPLOAD_VAR} must be a positive integer (in bytes), got: {value}"
        )),
        Ok(n) => Ok(n),
    }
}

/// 必須是「IP:埠號」（例如 0.0.0.0:3000、[::]:3000），不接受主機名稱
fn parse_bind_addr(raw: Result<String, VarError>) -> Result<SocketAddr, String> {
    let Some(value) = read_env_string(BIND_VAR, raw)? else {
        return Ok(DEFAULT_BIND
            .parse()
            .expect("the default bind address must be valid"));
    };
    let value = value.trim();

    value.parse().map_err(|_| {
        format!("{BIND_VAR} must be an IP address and port, e.g. 0.0.0.0:3000, got: {value}")
    })
}

/// 必須是正整數。0 不允許：那等於關閉大檔案的上傳，而設定寫成 0 多半是誤會
fn parse_max_pending_uploads(raw: Result<String, VarError>) -> Result<u32, String> {
    let Some(value) = read_env_string(MAX_PENDING_UPLOADS_VAR, raw)? else {
        return Ok(DEFAULT_MAX_PENDING_UPLOADS);
    };
    let value = value.trim();

    match value.parse::<u32>() {
        Ok(0) | Err(_) => Err(format!(
            "{MAX_PENDING_UPLOADS_VAR} must be a positive integer, got: {value}"
        )),
        Ok(n) => Ok(n),
    }
}

/// 只接受明確的寫法（不分大小寫）；寫錯一律報錯，
/// 避免 `ture` 之類的拼字錯誤被默默當成 false，讓管理者以為設定已生效。
fn parse_bool(name: &str, raw: Result<String, VarError>) -> Result<bool, String> {
    let Some(value) = read_env_string(name, raw)? else {
        return Ok(false);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => Err(format!(
            "{name} must be true or false (1, 0, yes and no are also accepted), got: {other}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(value: &str) -> Result<String, VarError> {
        Ok(value.to_string())
    }

    fn unset() -> Result<String, VarError> {
        Err(VarError::NotPresent)
    }

    /// 用一組固定的值當作設定來源，不需要修改程式的環境變數
    fn load_from(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::load(&|name: &str| values.get(name).cloned().ok_or(VarError::NotPresent))
    }

    #[cfg(unix)]
    fn not_unicode() -> Result<String, VarError> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        Err(VarError::NotUnicode(OsString::from_vec(vec![b'1', 0xFF])))
    }

    // ==================== Config::load ====================

    #[test]
    fn defaults_are_safe_for_development() {
        let config = load_from(&[]).unwrap();

        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.database_url, "sqlite://app.db");
        assert_eq!(config.max_upload_bytes, DEFAULT_MAX_UPLOAD_BYTES);
        assert!(config.bind_addr.ip().is_loopback());
        assert!(!config.secure_cookies);
        assert_eq!(config.max_pending_uploads, 5);
    }

    #[test]
    fn values_are_read_and_trimmed() {
        let config = load_from(&[
            ("MANORCLOUD_DATA_DIR", " /srv/data "),
            ("MANORCLOUD_DATABASE_URL", "sqlite:///srv/x.db"),
            ("MANORCLOUD_BIND", "0.0.0.0:3000"),
            ("MANORCLOUD_SECURE_COOKIES", "true"),
            ("MANORCLOUD_MAX_PENDING_UPLOADS", "8"),
        ])
        .unwrap();

        assert_eq!(config.data_dir, PathBuf::from("/srv/data"));
        assert_eq!(config.database_url, "sqlite:///srv/x.db");
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:3000");
        assert!(config.secure_cookies);
        assert_eq!(config.max_pending_uploads, 8);
    }

    #[test]
    fn any_invalid_value_stops_loading() {
        for pair in [
            ("MANORCLOUD_SECURE_COOKIES", "ture"),
            ("MANORCLOUD_BIND", "localhost:3000"),
            ("MANORCLOUD_MAX_UPLOAD_BYTES", "10GB"),
            ("MANORCLOUD_MAX_PENDING_UPLOADS", "0"),
        ] {
            let error = load_from(&[pair]).unwrap_err();
            // 錯誤訊息會印到終端機：必須是 ASCII，而且指出是哪個設定
            assert!(error.is_ascii(), "{error}");
            assert!(error.contains(pair.0), "{error}");
        }
    }

    // ==================== parse_env_file ====================

    #[test]
    fn env_file_format() {
        let values = parse_env_file(
            "# comment\n\
            ; also a comment\n\
            \n\
            MANORCLOUD_BIND=0.0.0.0:3000\n\
            MANORCLOUD_DATA_DIR=\"/var/lib/manorcloud/data\"\n\
            MANORCLOUD_BIND=127.0.0.1:4000\n\
            a line without an equals sign\n",
        );

        // 同名時後面覆蓋前面，與 systemd 相同
        assert_eq!(values["MANORCLOUD_BIND"], "127.0.0.1:4000");
        // 成對的雙引號被去掉
        assert_eq!(values["MANORCLOUD_DATA_DIR"], "/var/lib/manorcloud/data");
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn env_file_single_quotes() {
        let values =
            parse_env_file("MANORCLOUD_DATA_DIR='/srv/data'\nMANORCLOUD_BIND=\"0.0.0.0:3000'\n");

        // 成對的單引號被去掉，與 systemd 相同
        assert_eq!(values["MANORCLOUD_DATA_DIR"], "/srv/data");
        // 不成對的引號原樣保留
        assert_eq!(values["MANORCLOUD_BIND"], "\"0.0.0.0:3000'");
    }

    #[test]
    fn env_file_is_not_executed() {
        // shell 的 source 會執行 $(...)；這裡只把它當成文字
        let values = parse_env_file("MANORCLOUD_DATA_DIR=$(rm -rf /)\n");
        assert_eq!(values["MANORCLOUD_DATA_DIR"], "$(rm -rf /)");
    }

    #[test]
    fn env_file_values_pass_the_same_validation() {
        // 設定檔中的值與環境變數用同一套規則驗證
        let values = parse_env_file("MANORCLOUD_SECURE_COOKIES=ture\n");
        let result =
            Config::load(&|name: &str| values.get(name).cloned().ok_or(VarError::NotPresent));

        assert!(result.is_err());
    }

    // ==================== 個別欄位 ====================

    #[test]
    #[cfg(unix)]
    fn not_unicode_is_rejected_not_defaulted() {
        assert!(parse_max_upload_bytes(not_unicode()).is_err());
        assert!(read_env_string("X", not_unicode()).is_err());
    }

    #[test]
    fn max_upload_values() {
        assert_eq!(
            parse_max_upload_bytes(unset()),
            Ok(DEFAULT_MAX_UPLOAD_BYTES)
        );
        assert_eq!(parse_max_upload_bytes(set(" 42 ")), Ok(42));
        for value in ["0", "-1", "10GB", "1.5", "abc"] {
            assert!(parse_max_upload_bytes(set(value)).is_err(), "{value}");
        }
    }

    #[test]
    fn bind_values() {
        assert_eq!(
            parse_bind_addr(set("[::]:8080")).unwrap().to_string(),
            "[::]:8080"
        );
        for value in ["3000", "0.0.0.0", "localhost:3000", "0.0.0.0:99999", "abc"] {
            assert!(parse_bind_addr(set(value)).is_err(), "{value}");
        }
    }

    #[test]
    fn bool_values() {
        for value in ["true", "TRUE", "1", "yes", " Yes "] {
            assert_eq!(parse_bool("X", set(value)), Ok(true), "{value}");
        }
        for value in ["false", "False", "0", "no"] {
            assert_eq!(parse_bool("X", set(value)), Ok(false), "{value}");
        }
        for value in ["ture", "flase", "on", "2", "enabled"] {
            assert!(parse_bool("X", set(value)).is_err(), "{value}");
        }
    }

    #[test]
    fn max_pending_uploads_values() {
        assert_eq!(parse_max_pending_uploads(unset()), Ok(5));
        assert_eq!(parse_max_pending_uploads(set(" 12 ")), Ok(12));
        for value in ["0", "-1", "five", "1.5"] {
            let error = parse_max_pending_uploads(set(value)).unwrap_err();
            assert!(error.is_ascii(), "{error}");
            assert!(error.contains(MAX_PENDING_UPLOADS_VAR), "{error}");
        }
    }

    // ==================== sqlite_path ====================

    #[test]
    fn sqlite_path_formats() {
        assert_eq!(
            sqlite_path("sqlite:///var/lib/manorcloud/manorcloud.db").unwrap(),
            PathBuf::from("/var/lib/manorcloud/manorcloud.db")
        );
        assert_eq!(
            sqlite_path("sqlite://app.db").unwrap(),
            PathBuf::from("app.db")
        );
        assert_eq!(
            sqlite_path("sqlite:app.db?mode=rwc").unwrap(),
            PathBuf::from("app.db")
        );
        assert!(sqlite_path("sqlite::memory:").is_err());
        assert!(sqlite_path("postgres://x").is_err());
    }
}
