//! Chrome 扩展到桌面应用的本机桥接，只接收已验证的支付宝链接。
//! 监听回环地址，配对码保存在应用配置目录；不接收姓名、地址或付款凭据。

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apw_core::notify::{Discord, Notification, Notifier};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{AppState, NOTICE_CHANNEL};

pub const PORT: u16 = 43849;
pub const EVENT_CHANNEL: &str = "watcher://payment-link";
const TOKEN_FILE: &str = "extension-pairing-code";
const MAX_HEADER: usize = 16 << 10;
const MAX_BODY: usize = 8 << 10;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentLink {
    pub url: String,
    /// `qr` 表示页面上可复制的二维码目标；`cashier` 表示支付宝收银台地址。
    pub kind: String,
    pub received_at: u64,
}

#[derive(Debug, Deserialize)]
struct IncomingLink {
    url: String,
    kind: String,
}

pub struct PaymentBridge {
    token: Option<String>,
    latest: RwLock<Option<PaymentLink>>,
}

impl PaymentBridge {
    pub fn disabled() -> Self {
        Self {
            token: None,
            latest: RwLock::new(None),
        }
    }

    pub fn pairing_code(&self) -> Result<&str, String> {
        self.token
            .as_deref()
            .ok_or_else(|| "本机付款链接桥接未启动，请查看运行日志".to_string())
    }

    pub fn latest(&self) -> Option<PaymentLink> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn record(&self, link: PaymentLink) -> bool {
        let mut current = self.latest.write().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|old| old.url == link.url) {
            return false;
        }
        *current = Some(link);
        true
    }
}

fn load_or_create_token(dir: &Path) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|e| format!("建立桥接配置目录失败：{e}"))?;
    let path = dir.join(TOKEN_FILE);
    match fs::File::open(&path) {
        Ok(file) => {
            let mut token = String::new();
            file.take(65)
                .read_to_string(&mut token)
                .map_err(|e| format!("读取扩展连接码失败：{e}"))?;
            if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("扩展连接码文件已损坏，请保留原文件并手动修复".into());
            }
            Ok(token)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let token = uuid::Uuid::new_v4().simple().to_string();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(mut file) => {
                    file.write_all(token.as_bytes())
                        .and_then(|()| file.sync_all())
                        .map_err(|e| format!("保存扩展连接码失败：{e}"))?;
                    Ok(token)
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => load_or_create_token(dir),
                Err(err) => Err(format!("建立扩展连接码文件失败：{err}")),
            }
        }
        Err(err) => Err(format!("打开扩展连接码文件失败：{err}")),
    }
}

pub fn prepare(app: &AppHandle) -> Result<(PaymentBridge, std::net::TcpListener), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let token = load_or_create_token(&dir)?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", PORT))
        .map_err(|e| format!("监听本机端口 {PORT} 失败：{e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok((
        PaymentBridge {
            token: Some(token),
            latest: RwLock::new(None),
        },
        listener,
    ))
}

pub async fn serve(app: AppHandle, listener: std::net::TcpListener) {
    let listener = match TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            let _ = app.emit(NOTICE_CHANNEL, format!("启动付款链接桥接失败：{err}"));
            return;
        }
    };
    loop {
        match listener.accept().await {
            Ok((socket, peer)) if peer.ip().is_loopback() => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { handle_connection(app, socket).await });
            }
            Ok(_) => {}
            Err(err) => {
                let _ = app.emit(NOTICE_CHANNEL, format!("付款链接桥接连接失败：{err}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn valid_origin(value: &str) -> bool {
    let Some(id) = value.strip_prefix("chrome-extension://") else {
        return false;
    };
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

fn valid_alipay_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    url.scheme() == "https"
        && (host == "alipay.com" || host.ends_with(".alipay.com"))
        && url.username().is_empty()
        && url.password().is_none()
        && value.len() <= MAX_BODY
}

async fn read_request(socket: &mut TcpStream) -> Result<(String, String, Vec<u8>), u16> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let count = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut chunk))
            .await
            .map_err(|_| 408u16)?
            .map_err(|_| 400u16)?;
        if count == 0 {
            return Err(400);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|slice| slice == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() > MAX_HEADER {
            return Err(431);
        }
    };
    if header_end > MAX_HEADER {
        return Err(431);
    }
    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|_| 400u16)?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(400u16)?.to_string();
    let path = parts.next().ok_or(400u16)?.to_string();
    if parts.next() != Some("HTTP/1.1") {
        return Err(400);
    }
    let mut origin = String::new();
    let mut token = String::new();
    let mut content_length = 0;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "origin" => origin = value.trim().to_string(),
            "authorization" => token = value.trim().to_string(),
            "content-length" => {
                content_length = value.trim().parse::<usize>().map_err(|_| 400u16)?;
            }
            "transfer-encoding" => return Err(400),
            _ => {}
        }
    }
    if !valid_origin(&origin) {
        return Err(403);
    }
    if content_length > MAX_BODY {
        return Err(413);
    }
    while bytes.len() - header_end < content_length {
        let count = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut chunk))
            .await
            .map_err(|_| 408u16)?
            .map_err(|_| 400u16)?;
        if count == 0 {
            return Err(400);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() - header_end > MAX_BODY {
            return Err(413);
        }
    }
    let body = bytes[header_end..header_end + content_length].to_vec();
    Ok((format!("{method} {path}\n{origin}"), token, body))
}

async fn respond(socket: &mut TcpStream, status: u16, origin: &str, body: &str) {
    let status_text = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let cors = if valid_origin(origin) {
        format!(
            "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: authorization, content-type\r\nAccess-Control-Allow-Private-Network: true\r\nVary: Origin\r\n"
        )
    } else {
        String::new()
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{cors}\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn handle_connection(app: AppHandle, mut socket: TcpStream) {
    let (route, authorization, body) = match read_request(&mut socket).await {
        Ok(request) => request,
        Err(status) => {
            respond(&mut socket, status, "", "{}").await;
            return;
        }
    };
    let Some((request_line, origin)) = route.split_once('\n') else {
        respond(&mut socket, 400, "", "{}").await;
        return;
    };
    if request_line == "OPTIONS /payment-link" {
        respond(&mut socket, 204, origin, "").await;
        return;
    }
    if request_line != "POST /payment-link" {
        respond(&mut socket, 404, origin, "{}").await;
        return;
    }
    let bridge = app.state::<PaymentBridge>();
    let Ok(pairing_code) = bridge.pairing_code() else {
        respond(&mut socket, 500, origin, "{}").await;
        return;
    };
    if authorization != format!("Bearer {pairing_code}") {
        respond(&mut socket, 401, origin, "{}").await;
        return;
    }
    let Ok(incoming) = serde_json::from_slice::<IncomingLink>(&body) else {
        respond(&mut socket, 400, origin, "{}").await;
        return;
    };
    if !matches!(incoming.kind.as_str(), "qr" | "cashier") || !valid_alipay_url(&incoming.url) {
        respond(&mut socket, 400, origin, "{}").await;
        return;
    }
    let link = PaymentLink {
        url: incoming.url,
        kind: incoming.kind,
        received_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let fresh = bridge.record(link.clone());
    respond(&mut socket, 200, origin, "{\"ok\":true}").await;
    if fresh {
        let _ = app.emit(EVENT_CHANNEL, &link);
        tauri::async_runtime::spawn(send_discord(app, link));
    }
}

async fn send_discord(app: AppHandle, link: PaymentLink) {
    let state = app.state::<AppState>();
    let settings = state.settings_snapshot();
    if settings.discord_webhook.trim().is_empty() {
        let _ = app.emit(
            NOTICE_CHANNEL,
            "尚未设置 Discord webhook，支付宝链接仅显示在应用内",
        );
        return;
    }
    let notification = Notification::new(
        "Apple 支付宝付款链接",
        if link.kind == "qr" {
            "付款二维码链接可能很快失效；付款前请核对商家与金额。"
        } else {
            "支付宝收银台页面；付款前请核对商家与金额。"
        },
    )
    .with_url(link.url);
    for discord in Discord::from_list(&settings.discord_webhook, state.http.clone()) {
        if let Err(err) = discord.notify(&notification).await {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("支付宝链接发送到 Discord 失败：{err}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{read_request, respond, valid_alipay_url, valid_origin};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[test]
    fn accepts_only_extension_origins_and_alipay_https_links() {
        assert!(valid_origin(&format!(
            "chrome-extension://{}",
            "a".repeat(32)
        )));
        assert!(!valid_origin("https://example.com"));
        assert!(valid_alipay_url("https://qr.alipay.com/abc123"));
        assert!(valid_alipay_url(
            "https://excashier.alipay.com/payment.htm?x=1"
        ));
        assert!(!valid_alipay_url("https://alipay.com.evil.example/pay"));
        assert!(!valid_alipay_url("http://qr.alipay.com/abc123"));
        assert!(!valid_alipay_url(
            "https://user:password@qr.alipay.com/abc123"
        ));
    }

    #[tokio::test]
    async fn extension_preflight_receives_local_cors_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut socket = TcpStream::connect(address).await.unwrap();
            let request = format!(
                "OPTIONS /payment-link HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: chrome-extension://{}\r\nContent-Length: 0\r\n\r\n",
                "a".repeat(32)
            );
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut answer = String::new();
            socket.read_to_string(&mut answer).await.unwrap();
            answer
        });
        let (mut socket, _) = listener.accept().await.unwrap();
        let (route, token, body) = read_request(&mut socket).await.unwrap();
        assert!(route.starts_with("OPTIONS /payment-link\nchrome-extension://"));
        assert!(token.is_empty());
        assert!(body.is_empty());
        let origin = route.split_once('\n').unwrap().1;
        respond(&mut socket, 204, origin, "").await;
        let response = client.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 204 No Content"));
        assert!(response.contains("Access-Control-Allow-Origin: chrome-extension://"));
        assert!(response.contains("Access-Control-Allow-Private-Network: true"));
    }

    #[tokio::test]
    async fn extension_post_reads_payment_body_and_authorization() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = r#"{"url":"https://qr.alipay.com/abc123","kind":"qr"}"#;
        let request = format!(
            "POST /payment-link HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: chrome-extension://{}\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
            "a".repeat(32),
            "b".repeat(32),
            body.len()
        );
        let client = tokio::spawn(async move {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket.write_all(request.as_bytes()).await.unwrap();
        });
        let (mut socket, _) = listener.accept().await.unwrap();
        let (route, token, received) = read_request(&mut socket).await.unwrap();
        client.await.unwrap();
        assert!(route.starts_with("POST /payment-link\nchrome-extension://"));
        assert_eq!(token, format!("Bearer {}", "b".repeat(32)));
        assert_eq!(received, body.as_bytes());
    }
}
