//! 所有設定的讀取都集中在這裡，確保規則一致：
//! - 沒有設定或為空字串：使用預設值
//! - 有設定但不合法（包含不是 UTF-8）：回傳錯誤，程式不啟動。
//!   設定寫錯卻默默改用預設值，管理者會以為設定已生效。
//!
//! 設定檔只放「讀資料庫之前就要知道」的設定：監聽的位址、資料與資料庫的位置、TLS。
//! 其他設定都在管理頁面修改（存在資料庫的 server_settings）。
//!
//! 設定的「來源」可以替換：伺服器讀取環境變數（由 systemd 從設定檔載入），
//! 匯出工具則自己讀取設定檔。兩者共用 Config::load，驗證規則只有一份。
//!
//! 錯誤訊息會在啟動時印到終端機與 journal，所以使用英文。

use std::collections::HashMap;
use std::env::VarError;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

const DATA_DIR_VAR: &str = "MANORCLOUD_DATA_DIR";
const DEFAULT_DATA_DIR: &str = "./data";

const DATABASE_URL_VAR: &str = "MANORCLOUD_DATABASE_URL";
const DEFAULT_DATABASE_URL: &str = "sqlite://app.db";

const BIND_VAR: &str = "MANORCLOUD_BIND";
/// 預設只監聽本機：要讓區網其他裝置連入，必須在設定檔中明確寫出 0.0.0.0
/// install.sh 的 DEFAULT_BIND 必須與它一致（安裝完成時顯示的位址）
const DEFAULT_BIND: &str = "127.0.0.1:3000";

const TLS_CERT_VAR: &str = "MANORCLOUD_TLS_CERT";
const TLS_KEY_VAR: &str = "MANORCLOUD_TLS_KEY";

// 舊版的設定，現在改在管理頁面設定（見 Legacy）
pub const MAX_UPLOAD_VAR: &str = "MANORCLOUD_MAX_UPLOAD_BYTES";
pub const MAX_PENDING_UPLOADS_VAR: &str = "MANORCLOUD_MAX_PENDING_UPLOADS";
pub const TRUSTED_PROXIES_VAR: &str = "MANORCLOUD_TRUSTED_PROXIES";
pub const SECURE_COOKIES_VAR: &str = "MANORCLOUD_SECURE_COOKIES";

/// 依名稱取得設定值。與 `std::env::var` 相同的形狀，方便直接替換
pub type Lookup<'a> = &'a dyn Fn(&str) -> Result<String, VarError>;

#[derive(Debug, Clone)]
pub struct Config {
    /// 使用者檔案的存放根目錄
    pub data_dir: PathBuf,
    /// 資料庫連線字串。刻意不叫 DATABASE_URL：那個名稱留給 sqlx 的巨集在編譯時使用
    pub database_url: String,
    /// 監聽的位址與埠號
    pub bind_addr: SocketAddr,
    /// 直接提供 HTTPS 時的憑證與私鑰；沒有設定時只提供 HTTP
    pub tls: Option<TlsFiles>,
    /// 設定檔中還留著的舊設定
    pub legacy: Legacy,
}

/// 舊版放在設定檔、現在改在管理頁面設定的值。
///
/// 升級後第一次啟動時，管理頁面還沒有設定的項目會搬進資料庫
/// （web::server_settings::load_settings），之後設定檔的這幾行就沒有作用了。
/// 值仍然要合法：寫錯的值不能默默被忽略。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Legacy {
    pub max_upload_bytes: Option<u64>,
    pub max_pending_uploads: Option<u32>,
    pub trusted_proxies: Option<Vec<IpRange>>,
    /// MANORCLOUD_SECURE_COOKIES 還在設定檔裡。現在由程式自己判斷連線是不是 HTTPS，
    /// 這一行只會被提醒可以刪掉
    pub secure_cookies: bool,
}

/// 一個 IP 位址或網段，例如 127.0.0.1、172.18.0.0/16、fd00::/8
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IpRange {
    addr: IpAddr,
    prefix: u8,
}

impl std::fmt::Display for IpRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let max = if self.addr.is_ipv4() { 32 } else { 128 };
        if self.prefix == max {
            write!(f, "{}", self.addr)
        } else {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }
}

impl IpRange {
    /// 解析「位址」或「位址/前綴長度」
    pub fn parse(text: &str) -> Option<IpRange> {
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text, None),
        };
        let addr: IpAddr = addr.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().ok().filter(|&p| p <= max)?,
            None => max,
        };
        Some(IpRange {
            addr: addr.to_canonical(),
            prefix: if addr.to_canonical().is_ipv4() && !addr.is_ipv4() {
                prefix.saturating_sub(96)
            } else {
                prefix
            },
        })
    }

    /// `ip` 是否在這個範圍內。IPv4 對應的 IPv6 位址（::ffff:1.2.3.4）當作 IPv4 比對
    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = |ip: IpAddr| -> (u128, u32) {
            match ip.to_canonical() {
                IpAddr::V4(v4) => (u128::from(u32::from(v4)), 32),
                IpAddr::V6(v6) => (u128::from(v6), 128),
            }
        };
        let (range, width) = bits(self.addr);
        let (ip, ip_width) = bits(ip);
        if width != ip_width {
            return false;
        }
        let prefix = u32::from(self.prefix);
        if prefix == 0 {
            return true;
        }
        let shift = width - prefix;
        (range >> shift) == (ip >> shift)
    }
}

/// 以逗號分隔的位址或網段，例如 `127.0.0.1, 172.18.0.0/16`。空白的項目略過。
/// 錯誤訊息直接給使用者看（管理頁面），指出是哪一項
pub fn parse_ip_ranges(text: &str) -> Result<Vec<IpRange>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            IpRange::parse(item).ok_or_else(|| {
                format!(
                    "Enter IP addresses or ranges separated by commas, \
                     e.g. 127.0.0.1, 172.18.0.0/16 (not valid: {item})"
                )
            })
        })
        .collect()
}

/// 寫回文字（存進資料庫、顯示在管理頁面）
pub fn format_ip_ranges(ranges: &[IpRange]) -> String {
    ranges
        .iter()
        .map(IpRange::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// PEM 格式的憑證鏈與私鑰檔案路徑
#[derive(Debug, Clone, PartialEq)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
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
            bind_addr: parse_bind_addr(lookup(BIND_VAR))?,
            tls: parse_tls(lookup(TLS_CERT_VAR), lookup(TLS_KEY_VAR))?,
            legacy: Legacy {
                max_upload_bytes: parse_max_upload_bytes(lookup(MAX_UPLOAD_VAR))?,
                max_pending_uploads: parse_max_pending_uploads(lookup(MAX_PENDING_UPLOADS_VAR))?,
                trusted_proxies: parse_trusted_proxies(lookup(TRUSTED_PROXIES_VAR))?,
                secure_cookies: read_env_string(SECURE_COOKIES_VAR, lookup(SECURE_COOKIES_VAR))?
                    .is_some(),
            },
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

fn parse_max_upload_bytes(raw: Result<String, VarError>) -> Result<Option<u64>, String> {
    let Some(value) = read_env_string(MAX_UPLOAD_VAR, raw)? else {
        return Ok(None);
    };
    let value = value.trim();

    match value.parse::<u64>() {
        Ok(0) | Err(_) => Err(format!(
            "{MAX_UPLOAD_VAR} must be a positive integer (in bytes), got: {value}"
        )),
        Ok(n) => Ok(Some(n)),
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

/// 憑證與私鑰必須一起設定：只設一個多半是漏寫，不能默默改用 HTTP
fn parse_tls(
    cert: Result<String, VarError>,
    key: Result<String, VarError>,
) -> Result<Option<TlsFiles>, String> {
    let cert = read_env_string(TLS_CERT_VAR, cert)?;
    let key = read_env_string(TLS_KEY_VAR, key)?;
    match (cert, key) {
        (None, None) => Ok(None),
        (Some(cert), Some(key)) => Ok(Some(TlsFiles {
            cert: PathBuf::from(cert.trim()),
            key: PathBuf::from(key.trim()),
        })),
        _ => Err(format!(
            "{TLS_CERT_VAR} and {TLS_KEY_VAR} must be set together"
        )),
    }
}

/// 以逗號分隔的位址或網段，例如 127.0.0.1,172.18.0.0/16
fn parse_trusted_proxies(raw: Result<String, VarError>) -> Result<Option<Vec<IpRange>>, String> {
    let Some(value) = read_env_string(TRUSTED_PROXIES_VAR, raw)? else {
        return Ok(None);
    };
    parse_ip_ranges(&value)
        .map(Some)
        .map_err(|e| format!("{TRUSTED_PROXIES_VAR}: {e}"))
}

/// 必須是正整數。0 不允許：那等於關閉大檔案的上傳，而設定寫成 0 多半是誤會
fn parse_max_pending_uploads(raw: Result<String, VarError>) -> Result<Option<u32>, String> {
    let Some(value) = read_env_string(MAX_PENDING_UPLOADS_VAR, raw)? else {
        return Ok(None);
    };
    let value = value.trim();

    match value.parse::<u32>() {
        Ok(0) | Err(_) => Err(format!(
            "{MAX_PENDING_UPLOADS_VAR} must be a positive integer, got: {value}"
        )),
        Ok(n) => Ok(Some(n)),
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
        assert!(config.bind_addr.ip().is_loopback());
        assert_eq!(config.tls, None);
        assert_eq!(config.legacy, Legacy::default());
    }

    #[test]
    fn old_settings_are_kept_for_moving_to_administration() {
        let config = load_from(&[
            ("MANORCLOUD_MAX_UPLOAD_BYTES", " 42 "),
            ("MANORCLOUD_MAX_PENDING_UPLOADS", "8"),
            ("MANORCLOUD_TRUSTED_PROXIES", "192.168.10.200"),
            ("MANORCLOUD_SECURE_COOKIES", "true"),
        ])
        .unwrap();

        let legacy = config.legacy;
        assert_eq!(legacy.max_upload_bytes, Some(42));
        assert_eq!(legacy.max_pending_uploads, Some(8));
        assert_eq!(
            legacy.trusted_proxies,
            Some(vec![IpRange::parse("192.168.10.200").unwrap()])
        );
        assert!(legacy.secure_cookies);
    }

    #[test]
    fn ip_ranges_are_addresses_or_ranges() {
        let proxies = parse_ip_ranges(" 127.0.0.1, 172.18.0.0/16 ,::1, ").unwrap();
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();

        assert_eq!(proxies.len(), 3);
        assert!(proxies[0].contains(ip("127.0.0.1")));
        assert!(!proxies[0].contains(ip("127.0.0.2")));
        // 監聽 [::] 時，IPv4 的連線會以 ::ffff:x.x.x.x 的形式出現
        assert!(proxies[0].contains(ip("::ffff:127.0.0.1")));
        assert!(proxies[1].contains(ip("172.18.200.3")));
        assert!(!proxies[1].contains(ip("172.19.0.1")));
        assert!(proxies[2].contains(ip("::1")));
        assert!(!proxies[2].contains(ip("127.0.0.1")));

        assert_eq!(format_ip_ranges(&proxies), "127.0.0.1, 172.18.0.0/16, ::1");
        assert_eq!(parse_ip_ranges(""), Ok(Vec::new()));
        for bad in ["nginx", "10.0.0.0/33", "127.0.0.1, x"] {
            assert!(parse_ip_ranges(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tls_files_are_read() {
        let config = load_from(&[
            ("MANORCLOUD_TLS_CERT", "/etc/manorcloud/tls/cert.pem"),
            ("MANORCLOUD_TLS_KEY", " /etc/manorcloud/tls/key.pem "),
        ])
        .unwrap();

        let tls = config.tls.unwrap();
        assert_eq!(tls.cert, PathBuf::from("/etc/manorcloud/tls/cert.pem"));
        assert_eq!(tls.key, PathBuf::from("/etc/manorcloud/tls/key.pem"));
    }

    #[test]
    fn values_are_read_and_trimmed() {
        let config = load_from(&[
            ("MANORCLOUD_DATA_DIR", " /srv/data "),
            ("MANORCLOUD_DATABASE_URL", "sqlite:///srv/x.db"),
            ("MANORCLOUD_BIND", "0.0.0.0:3000"),
        ])
        .unwrap();

        assert_eq!(config.data_dir, PathBuf::from("/srv/data"));
        assert_eq!(config.database_url, "sqlite:///srv/x.db");
        assert_eq!(config.bind_addr.to_string(), "0.0.0.0:3000");
    }

    #[test]
    fn any_invalid_value_stops_loading() {
        for pair in [
            ("MANORCLOUD_BIND", "localhost:3000"),
            ("MANORCLOUD_MAX_UPLOAD_BYTES", "10GB"),
            ("MANORCLOUD_MAX_PENDING_UPLOADS", "0"),
            ("MANORCLOUD_TLS_CERT", "/etc/manorcloud/tls/cert.pem"),
            ("MANORCLOUD_TLS_KEY", "/etc/manorcloud/tls/key.pem"),
            ("MANORCLOUD_TRUSTED_PROXIES", "127.0.0.1,nginx"),
            ("MANORCLOUD_TRUSTED_PROXIES", "10.0.0.0/33"),
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
        let values = parse_env_file("MANORCLOUD_BIND=localhost\n");
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
        assert_eq!(parse_max_upload_bytes(unset()), Ok(None));
        assert_eq!(parse_max_upload_bytes(set(" 42 ")), Ok(Some(42)));
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
    fn max_pending_uploads_values() {
        assert_eq!(parse_max_pending_uploads(unset()), Ok(None));
        assert_eq!(parse_max_pending_uploads(set(" 12 ")), Ok(Some(12)));
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
