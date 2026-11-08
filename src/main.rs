mod auth;
mod bootstrap;
mod config;
mod db;
mod export;
mod files;
mod gc;
mod layout;
mod storage;
mod systemd;
mod uninstall;
mod web;

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// 說明文字。依「使用者最常需要什麼」排列：先是服務怎麼管理，再是其他指令。
/// 只列出目前真的能用的指令；之後新增子指令時再加入。
/// 啟動伺服器的 run 是隱藏指令，刻意不列在這裡（見 parse_command）。
/// 所有終端機輸出都用英文：目標機器的終端機可能無法顯示中文。
const HELP: &str = "\
ManorCloud - home cloud storage

ManorCloud runs in the background as a systemd service. Manage it with systemctl:
  systemctl status manorcloud       show status
  systemctl restart manorcloud      restart after changing the configuration
  journalctl -u manorcloud -f       follow the logs

Administration (requires sudo):
  manorcloud export <output-dir>    export all files with their original names and folders
  manorcloud uninstall [--purge]    remove the program and service;
                                    --purge also deletes /var/lib/manorcloud and the configuration

Other:
  manorcloud --version              show the version
  manorcloud --help                 show this help

Files:
  /etc/manorcloud/manorcloud.env    configuration
  /var/lib/manorcloud               database and user files
";

/// 命令列解析的結果
#[derive(Debug, PartialEq)]
enum Command {
    /// 沒有給任何指令：顯示說明，但視為使用方式錯誤
    Missing,
    Help,
    Version,
    Run,
    Export(Vec<OsString>),
    Uninstall(Vec<OsString>),
    /// 不認得的指令或參數，附上原因
    Invalid(String),
}

/// 只解析，不執行任何動作：方便測試
fn parse_command(args: &[OsString]) -> Command {
    let Some(first) = args.first() else {
        return Command::Missing;
    };
    let rest = &args[1..];

    match first.to_string_lossy().as_ref() {
        // 隱藏指令：刻意不列在說明中。只給 systemd（服務檔的 ExecStart）與開發時使用，
        // 一般使用者應該用 systemctl。手動執行另有 check_run_allowed 的檢查擋下
        "run" if rest.is_empty() => Command::Run,
        // 設定一律透過設定檔：多餘的參數不能被默默忽略，否則會讓人以為設定生效了
        "run" => Command::Invalid(
            "run does not accept arguments; put settings in /etc/manorcloud/manorcloud.env"
                .to_string(),
        ),
        // 子指令自己解析剩下的參數
        "export" => Command::Export(rest.to_vec()),
        "uninstall" => Command::Uninstall(rest.to_vec()),
        "-h" | "--help" | "help" => Command::Help,
        "-V" | "--version" => Command::Version,
        other => Command::Invalid(format!("unknown command: {other}")),
    }
}

/// 判斷是否允許啟動伺服器。只負責判斷，不讀取環境：方便測試每一種組合。
///
/// 目的是防止誤操作，而不是阻擋刻意的人：有 shell 權限的人總能繞過，也不需要擋。
fn check_run_allowed(
    is_root: bool,
    under_systemd: bool,
    manual_override: bool,
    debug_build: bool,
) -> Result<(), &'static str> {
    // 以 root 建立的資料庫或暫存檔，之後以 manorcloud 帳號執行的服務會讀寫不了，
    // 服務將以難以理解的方式失敗。ManorCloud 在任何情況下都不需要特殊權限，所以沒有例外。
    if is_root {
        return Err("Do not run the server as root.\n\
                    To start the service: sudo systemctl start manorcloud");
    }
    // 正式環境由 systemd 啟動；開發時的 debug 建置、明確設定了覆寫的情況（容器、除錯）例外
    if !under_systemd && !manual_override && !debug_build {
        return Err("The server is managed by systemd.\n\
                    To start the service: sudo systemctl start manorcloud\n\
                    (To run in a container or in the foreground for debugging, \
                    set MANORCLOUD_MANUAL_RUN=1)");
    }
    Ok(())
}

fn main() -> ExitCode {
    // args_os：路徑參數不一定是 UTF-8，不能用會在此時 panic 的 args()
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    match parse_command(&args) {
        Command::Missing => {
            // 結束代碼 2：沒有給指令是使用方式錯誤。
            // 若有舊的服務設定還在執行沒有 run 的 manorcloud，systemd 會清楚地回報啟動失敗
            print!("{HELP}");
            ExitCode::from(2)
        }
        Command::Help => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("manorcloud {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Run => {
            // SAFETY: geteuid 沒有任何前置條件，只是讀取目前行程的有效使用者編號
            let is_root = unsafe { libc::geteuid() } == 0;
            // systemd 啟動服務時一定會設定 INVOCATION_ID（systemd 232 起）
            let under_systemd = std::env::var_os("INVOCATION_ID").is_some();
            let manual_override = std::env::var("MANORCLOUD_MANUAL_RUN").as_deref() == Ok("1");

            if let Err(reason) = check_run_allowed(
                is_root,
                under_systemd,
                manual_override,
                // 編譯時決定：cargo run 的 debug 建置為 true，release 建置為 false
                cfg!(debug_assertions),
            ) {
                eprintln!("{reason}");
                return ExitCode::FAILURE;
            }
            let runtime = runtime();
            runtime.block_on(serve());
            // 超過時限被中斷的請求，它們的工作在這裡被取消：
            // 上傳中的 TempBlob 隨之 Drop，暫存檔立刻刪除，不必等 24 小時後的垃圾回收。
            // 最多等 5 秒，不讓背景的阻塞工作（例如 fsync）拖住關機
            runtime.shutdown_timeout(Duration::from_secs(5));
            ExitCode::SUCCESS
        }
        Command::Export(rest) => report(runtime().block_on(export::run(&rest))),
        // uninstall 不需要非同步，也就不建立執行環境
        Command::Uninstall(rest) => report(uninstall::run(&rest)),
        Command::Invalid(reason) => {
            eprintln!("{reason}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}

/// 只有需要非同步的指令才建立 tokio 執行環境：顯示說明或版本不需要啟動一整組執行緒
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create the async runtime")
}

/// 子指令的結果轉成結束代碼：成功 0，失敗印出原因並回傳 1
fn report(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 啟動伺服器
async fn serve() {
    // 啟動時就讀取並檢查所有設定，有問題立刻失敗，而不是等到第一次使用。
    // 設定錯誤是管理者要修正的問題，不是程式的 bug：印出一行清楚的訊息後離開，而不是 panic
    let config = config::Config::from_env().unwrap_or_else(|e| {
        eprintln!("configuration error: {e}");
        std::process::exit(1);
    });

    storage::init_dirs(&config.data_dir).expect("failed to create the data directories");
    println!("version: {}", env!("CARGO_PKG_VERSION"));
    println!("data directory: {}", config.data_dir.display());
    println!("upload limit: {} bytes", config.max_upload_bytes);
    println!("secure cookies: {}", config.secure_cookies);
    println!(
        "unfinished uploads per user: {}",
        config.max_pending_uploads
    );

    let pool = db::init_pool(&config.database_url).await;

    // 在開始監聽之前確保系統可以被登入：全新安裝時建立初始管理員，
    // 尚未認領時重新產生密碼。不會與任何請求並行。
    if let Some(admin) = bootstrap::ensure_initial_admin(&pool)
        .await
        .expect("failed to create the initial administrator")
    {
        println!("{}", bootstrap::banner(&admin));
    }

    // 背景垃圾回收：清除孤兒 blob、殘留暫存檔與過期 session
    gc::spawn_periodic(
        pool.clone(),
        Arc::from(config.data_dir.as_path()),
        gc::GcPolicy::default(),
    );

    // 看門狗：定期確認資料庫連線正常後回報 systemd，卡死時由 systemd 重啟
    systemd::spawn_watchdog(pool.clone());

    let state = web::AppState::new(pool, config.data_dir, config.max_upload_bytes)
        .with_secure_cookies(config.secure_cookies)
        .with_max_pending_uploads(config.max_pending_uploads);
    // 登入次數限制：背景定期清除過期的記錄
    state.login_limits.spawn_pruning();
    web::run(state, config.bind_addr).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Command {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse_command(&args)
    }

    // ==================== 指令解析 ====================

    #[test]
    fn no_command_shows_help_instead_of_starting() {
        assert_eq!(parse(&[]), Command::Missing);
    }

    #[test]
    fn run_is_hidden_but_works() {
        assert_eq!(parse(&["run"]), Command::Run);
        assert!(!HELP.contains("manorcloud run"));
    }

    #[test]
    fn run_rejects_extra_arguments() {
        assert!(matches!(
            parse(&["run", "--port", "8080"]),
            Command::Invalid(_)
        ));
    }

    #[test]
    fn export_receives_its_arguments() {
        assert_eq!(
            parse(&["export", "--env-file", "/tmp/x.env", "/tmp/out"]),
            Command::Export(vec![
                OsString::from("--env-file"),
                OsString::from("/tmp/x.env"),
                OsString::from("/tmp/out"),
            ])
        );
    }

    #[test]
    fn export_is_listed_in_help() {
        assert!(HELP.contains("manorcloud export"));
    }

    #[test]
    fn uninstall_receives_its_arguments() {
        assert_eq!(
            parse(&["uninstall", "--purge"]),
            Command::Uninstall(vec![OsString::from("--purge")])
        );
        assert!(HELP.contains("manorcloud uninstall"));
    }

    #[test]
    fn help_and_version() {
        for flag in ["-h", "--help", "help"] {
            assert_eq!(parse(&[flag]), Command::Help, "{flag}");
        }
        for flag in ["-V", "--version"] {
            assert_eq!(parse(&[flag]), Command::Version, "{flag}");
        }
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(matches!(parse(&["serve"]), Command::Invalid(_)));
        assert!(matches!(parse(&["--run"]), Command::Invalid(_)));
    }

    #[test]
    fn help_is_ascii_only() {
        // 終端機輸出一律使用英文：任何非 ASCII 字元都可能在沒有中文字型的主控台上顯示異常
        assert!(HELP.is_ascii());
    }

    // ==================== 啟動伺服器的檢查 ====================

    #[test]
    fn systemd_can_start_the_server() {
        assert!(check_run_allowed(false, true, false, false).is_ok());
    }

    #[test]
    fn manual_start_is_refused_in_release() {
        assert!(check_run_allowed(false, false, false, false).is_err());
    }

    #[test]
    fn exceptions_for_development_and_containers() {
        // 開發時的 debug 建置
        assert!(check_run_allowed(false, false, false, true).is_ok());
        // 明確設定了 MANORCLOUD_MANUAL_RUN=1
        assert!(check_run_allowed(false, false, true, false).is_ok());
    }

    #[test]
    fn root_is_always_refused() {
        // 不論是否由 systemd 啟動、是否設定了覆寫、是否為 debug 建置
        for under_systemd in [false, true] {
            for manual_override in [false, true] {
                for debug_build in [false, true] {
                    let result =
                        check_run_allowed(true, under_systemd, manual_override, debug_build);
                    assert!(result.is_err());
                    // 錯誤訊息也會印到終端機，同樣必須是英文
                    assert!(result.unwrap_err().is_ascii());
                }
            }
        }
    }
}
