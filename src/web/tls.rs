//! 直接提供 HTTPS：使用者自己準備的憑證與私鑰（PEM 檔）。
//!
//! TLS 握手在背景各自進行，不會因為一個很慢（或故意不回應）的連線卡住其他連線。
//! 握手成功的連線才交給 axum；失敗或逾時的直接關閉。

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::server::TlsStream;

/// 握手的時間上限：超過就放棄這個連線
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 已完成握手、等待 axum 接手的連線數上限
const QUEUE: usize = 64;

/// 讀取憑證鏈與私鑰，建立 TLS 設定。錯誤訊息直接給管理者看
pub fn load(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, String> {
    let cert_pem =
        std::fs::read(cert).map_err(|e| format!("cannot read {}: {e}", cert.display()))?;
    let key_pem = std::fs::read(key).map_err(|e| format!("cannot read {}: {e}", key.display()))?;

    let certs = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{} is not a valid PEM certificate: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} contains no certificate", cert.display()));
    }
    let key_der = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| format!("{} is not a valid PEM private key: {e}", key.display()))?;

    let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| format!("the certificate and private key do not match: {e}"))?;
    // 優先使用 HTTP/2：瀏覽器能在同一條連線上同時傳送多個上傳的分段。
    // 不支援的用戶端（curl --http1.1、舊的程式）照樣用 HTTP/1.1
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// 提供給 axum::serve 的 HTTPS 監聽器
pub struct TlsListener {
    local_addr: SocketAddr,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> io::Result<TlsListener> {
        let local_addr = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(QUEUE);
        tokio::spawn(accept_loop(tcp, TlsAcceptor::from(config), tx));
        Ok(TlsListener { local_addr, ready })
    }
}

async fn accept_loop(
    tcp: TcpListener,
    acceptor: TlsAcceptor,
    tx: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        // 伺服器停止（監聽器被丟棄）後也結束，關閉監聽的連接埠
        let accepted = tokio::select! {
            accepted = tcp.accept() => accepted,
            _ = tx.closed() => return,
        };
        let (stream, addr) = match accepted {
            Ok(pair) => pair,
            Err(_) => {
                // 例如檔案描述子用完：稍等再試，不要空轉
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Ok(Ok(tls)) =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
            {
                let _ = tx.send((tls, addr)).await;
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(pair) => pair,
            // 背景迴圈只在監聽器被丟棄後結束，不會走到這裡；保險起見永遠等待
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_reported_by_name() {
        let error = load(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .unwrap_err();
        assert!(error.contains("/nonexistent/cert.pem"), "{error}");
    }

    #[test]
    fn a_file_without_a_certificate_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, "not a certificate\n").unwrap();
        std::fs::write(&key, "not a key\n").unwrap();
        let error = load(&cert, &key).unwrap_err();
        assert!(error.contains("no certificate"), "{error}");
    }
}
