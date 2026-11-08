//! systemd 的通知協定（sd_notify）。
//!
//! 服務檔設定為 Type=notify 時，systemd 會等到程式傳送 READY=1 才認為服務已啟動：
//! `systemctl start` 會等待；程式在準備過程中失敗，指令會回報失敗。
//! 協定很簡單：把一段文字傳到 NOTIFY_SOCKET 指定的 Unix datagram socket。
//!
//! 不是由 systemd 啟動時（開發時的 cargo run）沒有 NOTIFY_SOCKET，什麼都不做。

use std::ffi::OsStr;
use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::Duration;

use sqlx::SqlitePool;

/// 健康檢查的超時上限
const MAX_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// 通知 systemd：已開始監聽，可以接收請求
pub fn notify_ready() {
    match notify("READY=1") {
        Ok(true) => println!("notified systemd: service is ready"),
        Ok(false) => {}
        Err(e) => eprintln!("failed to notify systemd: {e}"),
    }
}

/// 通知 systemd：開始停止，正在等待進行中的請求完成
pub fn notify_stopping() {
    if let Err(e) = notify("STOPPING=1") {
        eprintln!("failed to notify systemd: {e}");
    }
}

/// 若服務檔設定了 WatchdogSec，定期回報「還活著」。
///
/// 每 WatchdogSec 的三分之一做一次健康檢查，通過才回報。
/// 一次失敗之後還有下一次機會，**連續兩次**失敗才會超過 WatchdogSec、由 systemd 重啟服務：
/// 儲存裝置偶爾卡頓幾秒，不應該讓正在上傳的檔案跟著被中斷。
/// （若間隔是 WatchdogSec 的一半，一次失敗後下一次檢查開始時期限就已經到了。）
/// `Restart=on-failure` 只能處理當掉，看門狗能處理卡死。
///
/// 偵測得到：
/// - 整個程式卡住：tokio 的排程停了，這個回報任務本身就無法執行
/// - 連線池的連線全被佔住、或資料庫卡住：健康檢查拿不到連線或超時，不回報
///
/// 偵測不到：單一請求卡在某個 `.await` 上。這時其他請求仍能正常處理，
/// 伺服器整體還在服務，本來就不需要重啟。
pub fn spawn_watchdog(pool: SqlitePool) {
    let Some(usec) = std::env::var("WATCHDOG_USEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    // WATCHDOG_PID 存在時，只有指定的行程才應該回報
    if let Ok(pid) = std::env::var("WATCHDOG_PID")
        && pid.parse::<u32>().ok() != Some(std::process::id())
    {
        return;
    }

    let interval = ping_interval(Duration::from_micros(usec));
    let health_timeout = health_timeout(interval);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;

            if let Err(reason) = check_health(&pool, health_timeout).await {
                // 不回報：持續失敗時讓 systemd 在超時後重啟服務
                eprintln!("watchdog: health check failed, not sending the ping: {reason}");
                continue;
            }
            if let Err(e) = notify("WATCHDOG=1") {
                eprintln!("failed to send the watchdog ping: {e}");
            }
        }
    });
}

/// 回報間隔：WatchdogSec 的三分之一。
/// 一次檢查失敗之後，期限之前還有一次機會（見 spawn_watchdog 的說明）
fn ping_interval(watchdog: Duration) -> Duration {
    watchdog / 3
}

/// 健康檢查的超時：回報間隔的三分之一，最多 10 秒。
/// 必須小於回報間隔，否則一次檢查還沒結束，下一次回報的時間就到了
fn health_timeout(interval: Duration) -> Duration {
    (interval / 3).min(MAX_HEALTH_TIMEOUT)
}

/// 從連線池拿一個連線，執行 SELECT 1，限時完成。
///
/// SELECT 1 不讀取任何資料表，不需要取得資料庫檔案的鎖：正常的長時間寫入交易不會讓它變慢。
/// 它真正測的是「能不能從連線池拿到連線」，而連線全被佔住是最常見的卡死原因。
async fn check_health(pool: &SqlitePool, timeout: Duration) -> Result<(), String> {
    match tokio::time::timeout(timeout, sqlx::query("SELECT 1").execute(pool)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("database error: {e}")),
        Err(_) => Err(format!(
            "no database connection within {} seconds",
            timeout.as_secs_f32()
        )),
    }
}

/// 回傳是否有送出（沒有 NOTIFY_SOCKET 代表不是由 systemd 啟動）
fn notify(state: &str) -> io::Result<bool> {
    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(false);
    };
    send_to(&socket, state)?;
    Ok(true)
}

/// NOTIFY_SOCKET 以 @ 開頭時是 Linux 的抽象命名空間 socket（沒有對應的檔案），否則是檔案路徑
fn send_to(socket: &OsStr, state: &str) -> io::Result<()> {
    let sender = UnixDatagram::unbound()?;
    match socket.as_bytes().strip_prefix(b"@") {
        Some(name) => {
            let addr = SocketAddr::from_abstract_name(name)?;
            sender.send_to_addr(state.as_bytes(), &addr)?;
        }
        None => {
            sender.send_to(state.as_bytes(), socket)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    fn receive(receiver: &UnixDatagram) -> String {
        let mut buf = [0u8; 64];
        let n = receiver.recv(&mut buf).unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    }

    // ==================== 通知 ====================

    #[test]
    fn sends_to_path_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();

        send_to(path.as_os_str(), "READY=1").unwrap();

        assert_eq!(receive(&receiver), "READY=1");
    }

    #[test]
    fn sends_to_abstract_socket() {
        // 抽象命名空間是全系統共用的：名稱帶上行程編號，避免與其他測試衝突
        let name = format!("manorcloud-test-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let receiver = UnixDatagram::bind_addr(&addr).unwrap();

        send_to(OsStr::new(&format!("@{name}")), "WATCHDOG=1").unwrap();

        assert_eq!(receive(&receiver), "WATCHDOG=1");
    }

    // ==================== 健康檢查 ====================

    #[test]
    fn health_timeout_is_shorter_than_the_interval() {
        // 預設的 WatchdogSec=60：間隔 20 秒，檢查限時約 6.7 秒
        let interval = Duration::from_secs(20);
        assert_eq!(health_timeout(interval), interval / 3);
        // 很短的 WatchdogSec：依比例縮短，仍小於間隔
        assert_eq!(
            health_timeout(Duration::from_secs(3)),
            Duration::from_secs(1)
        );
        // 很長的 WatchdogSec：不超過上限
        assert_eq!(health_timeout(Duration::from_secs(600)), MAX_HEALTH_TIMEOUT);
    }

    #[sqlx::test]
    async fn healthy_pool_passes(pool: SqlitePool) {
        assert!(check_health(&pool, Duration::from_secs(1)).await.is_ok());
    }

    #[tokio::test]
    async fn exhausted_pool_fails() {
        // 只有一個連線的池子，把它佔住不放：模擬所有連線都卡在某處
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let _held = pool.acquire().await.unwrap();

        let error = check_health(&pool, Duration::from_millis(200))
            .await
            .unwrap_err();

        assert!(error.contains("no database connection"), "{error}");
    }

    #[test]
    fn one_failed_check_does_not_cause_a_restart() {
        let watchdog = Duration::from_secs(60);
        // 呼叫 spawn_watchdog 實際使用的函式：有人改回一半時，這個測試會失敗
        let interval = ping_interval(watchdog);

        // t=0 回報成功；下一次檢查失敗；再下一次檢查（含它的時限）必須在期限之前完成
        let second_chance_done = interval * 2 + health_timeout(interval);
        assert!(second_chance_done < watchdog, "{second_chance_done:?}");
    }
}
