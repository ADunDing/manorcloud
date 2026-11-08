//! 優雅關機：收到停止訊號後不再接受新連線，等進行中的請求完成，但有時間上限。
//!
//! 不能無限等待：一個正在上傳 10 GB 的連線會讓重新開機卡住很久。
//! 超過時限仍未完成的請求，在執行環境關閉時被取消；
//! 被取消的上傳，其 TempBlob 隨之 Drop，暫存檔立刻刪除。

use std::future::{Future, IntoFuture};
use std::io;
use std::time::Duration;

use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

/// 伺服器如何結束
#[derive(Debug, PartialEq)]
pub enum Shutdown {
    /// 所有進行中的請求都已完成
    Graceful,
    /// 等待超過時限，仍有請求未完成
    TimedOut,
}

/// 註冊 SIGTERM（systemctl stop）與 SIGINT（開發時的 Ctrl+C），回傳收到任一個時完成的 future。
///
/// 註冊是在呼叫這個函式的當下完成，而不是第一次 await 時：
/// 必須在通知 systemd 就緒之前呼叫，否則在那之後、開始監聽訊號之前收到的 SIGTERM
/// 會以預設行為直接結束程式。
pub fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    let mut terminate = signal(SignalKind::terminate()).expect("failed to listen for SIGTERM");
    let mut interrupt = signal(SignalKind::interrupt()).expect("failed to listen for SIGINT");

    async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
    }
}

/// 執行伺服器，直到 `stop` 完成；之後最多再等 `grace` 讓進行中的請求完成。
///
/// `stop` 可以是任何 future：正式執行時是 `shutdown_signal()`，測試時是一個通道，
/// 不需要真的對測試程式送訊號。
pub async fn serve_until(
    listener: TcpListener,
    app: Router,
    stop: impl Future<Output = ()> + Send + 'static,
    grace: Duration,
) -> io::Result<Shutdown> {
    // 「開始停止」的時刻：收到訊號才開始計時，而不是從伺服器啟動就計時
    let (stopping_tx, mut stopping_rx) = tokio::sync::watch::channel(false);

    let stop = async move {
        stop.await;
        println!(
            "shutting down: waiting up to {} seconds for requests in progress",
            grace.as_secs()
        );
        crate::systemd::notify_stopping();
        let _ = stopping_tx.send(true);
    };

    // with_graceful_shutdown：stop 完成後不再接受新連線，等現有連線上的請求完成
    // 附帶連線資訊：handler 才能用 ConnectInfo 取得對方的 IP（登入次數限制需要）
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop)
    .into_future();

    let deadline = async move {
        // 先轉成 bool：wait_for 回傳的 Ref 是一個讀取鎖，不能跨過下面的 await 一直握著
        // （也會讓整個 future 無法在執行緒之間移動）。這一行結束時它就被釋放
        let stopping = stopping_rx.wait_for(|stopping| *stopping).await.is_ok();

        if stopping {
            tokio::time::sleep(grace).await;
        } else {
            // 通道在送出之前就關閉：伺服器自己結束了，時限沒有意義，永遠等下去讓 server 那邊先完成
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        result = server => result.map(|()| Shutdown::Graceful),
        () = deadline => Ok(Shutdown::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;

    /// 綁定到隨機的可用埠號
    async fn bind() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// 一個處理時間為 `delay` 的路由
    fn slow_app(delay: Duration) -> Router {
        Router::new().route(
            "/slow",
            get(move || async move {
                tokio::time::sleep(delay).await;
                "done"
            }),
        )
    }

    /// 送出一個最簡單的 HTTP 請求，讀回完整的回應
    async fn fetch(addr: SocketAddr) -> io::Result<String> {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await?;
        Ok(response)
    }

    /// 用通道代替真正的停止訊號
    fn stop_channel() -> (
        oneshot::Sender<()>,
        impl Future<Output = ()> + Send + 'static,
    ) {
        let (tx, rx) = oneshot::channel::<()>();
        (tx, async move {
            let _ = rx.await;
        })
    }

    #[tokio::test]
    async fn idle_server_stops_immediately() {
        let (listener, _) = bind().await;
        let (tx, stop) = stop_channel();
        tx.send(()).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            serve_until(
                listener,
                slow_app(Duration::ZERO),
                stop,
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("an idle server must not wait for the grace period");

        assert_eq!(result.unwrap(), Shutdown::Graceful);
    }

    #[tokio::test]
    async fn request_in_progress_is_allowed_to_finish() {
        let (listener, addr) = bind().await;
        let (tx, stop) = stop_channel();
        let server = tokio::spawn(serve_until(
            listener,
            slow_app(Duration::from_millis(300)),
            stop,
            Duration::from_secs(5),
        ));

        // 請求送出後、完成之前，收到停止訊號
        let client = tokio::spawn(fetch(addr));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).unwrap();

        assert_eq!(server.await.unwrap().unwrap(), Shutdown::Graceful);
        let response = client.await.unwrap().unwrap();
        assert!(response.contains("done"), "{response}");
    }

    #[tokio::test]
    async fn slow_request_is_cut_off_after_the_grace_period() {
        let (listener, addr) = bind().await;
        let (tx, stop) = stop_channel();
        let server = tokio::spawn(serve_until(
            listener,
            slow_app(Duration::from_secs(60)),
            stop,
            Duration::from_millis(200),
        ));

        // 這個請求要 60 秒才會完成；結果不重要，所以不等它
        tokio::spawn(fetch(addr));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("must stop shortly after the grace period, not after 60 seconds");

        assert_eq!(result.unwrap().unwrap(), Shutdown::TimedOut);
    }
}
