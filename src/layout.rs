//! 安裝後的檔案位置。
//!
//! 必須與根目錄的 install.sh（包括其中的 systemd 服務檔）一致：
//! 這些路徑同時出現在 Rust 與 shell 腳本中，修改時兩邊都要改。

use std::path::{Path, PathBuf};

/// systemd 服務名稱
pub const SERVICE_NAME: &str = "manorcloud";
/// 服務執行時使用的系統帳號
pub const SERVICE_USER: &str = "manorcloud";

/// 執行檔
pub const BINARY_PATH: &str = "/usr/local/bin/manorcloud";
/// systemd 服務檔
pub const UNIT_PATH: &str = "/etc/systemd/system/manorcloud.service";
/// `systemctl edit manorcloud` 建立的 drop-in 目錄（例如加上 ReadWritePaths）。
/// 服務移除後它就沒有用處，與服務檔一起刪除
pub const DROPIN_DIR: &str = "/etc/systemd/system/manorcloud.service.d";

/// 設定目錄與設定檔
pub const CONFIG_DIR: &str = "/etc/manorcloud";
pub const ENV_FILE: &str = "/etc/manorcloud/manorcloud.env";

/// systemd 的 StateDirectory，也是服務的 WorkingDirectory：
/// 設定中的相對路徑（例如預設的 ./data）以它為基準
pub const STATE_DIR: &str = "/var/lib/manorcloud";

/// 把設定中的路徑轉成絕對路徑：相對路徑以 STATE_DIR 為基準，與服務實際執行時一致。
///
/// 給 export、uninstall 這類手動執行的管理指令使用：它們的工作目錄是執行者當下所在的位置，
/// 不是服務的 WorkingDirectory。開頭的 ./ 去掉，顯示時才不會出現 /var/lib/manorcloud/./data
pub fn resolve(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        let relative = path.strip_prefix(".").unwrap_or(&path);
        Path::new(STATE_DIR).join(relative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_are_resolved_against_state_dir() {
        assert_eq!(
            resolve(PathBuf::from("app.db")),
            PathBuf::from("/var/lib/manorcloud/app.db")
        );
        assert_eq!(
            resolve(PathBuf::from("./data")),
            PathBuf::from("/var/lib/manorcloud/data")
        );
        assert_eq!(
            resolve(PathBuf::from("/srv/x.db")),
            PathBuf::from("/srv/x.db")
        );
    }
}
