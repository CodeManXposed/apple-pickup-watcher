//! 浏览器扩展到桌面应用的本机桥接，接收任务阶段和已验证的支付宝链接。
//! 监听回环地址，配对码保存在应用配置目录；不接收姓名、地址或付款凭据。

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apw_core::notify::{Discord, Notification, NotifyError};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{AppState, NOTICE_CHANNEL};

pub const PORT: u16 = 43849;
pub const EVENT_CHANNEL: &str = "watcher://payment-link";
pub const STATUS_CHANNEL: &str = "watcher://bot-status";
const TOKEN_FILE: &str = "extension-pairing-code";
const SESSION_FILE: &str = "checkout-session.v1.json";
const CHECKOUT_SESSION_TTL_SECONDS: u64 = 4 * 60 * 60;
const PAYMENT_SESSION_TTL_SECONDS: u64 = 60 * 60;
const MAX_HEADER: usize = 16 << 10;
const MAX_BODY: usize = 8 << 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaymentLink {
    pub url: String,
    /// `qr` 表示页面上可复制的二维码目标；`cashier` 表示支付宝收银台地址。
    pub kind: String,
    pub received_at: u64,
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingLink {
    url: String,
    kind: String,
    part_number: Option<String>,
    store_number: Option<String>,
    task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotStatus {
    pub part_number: String,
    pub store_number: String,
    pub task_id: String,
    pub stage: String,
    pub updated_at: u64,
    #[serde(default)]
    pub automation_active: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingStatus {
    part_number: String,
    store_number: String,
    task_id: String,
    stage: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingTask {
    part_number: String,
    store_number: String,
    task_id: String,
}

pub struct PaymentBridge {
    token: Option<String>,
    session_path: Option<PathBuf>,
    session: RwLock<BridgeSession>,
    live_task_id: RwLock<Option<String>>,
    discord_delivery: tokio::sync::Mutex<DiscordDelivery>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSession {
    status: Option<BotStatus>,
    latest: Option<PaymentLink>,
}

fn link_is_fresh(current: Option<&PaymentLink>, link: &PaymentLink) -> bool {
    current.is_none_or(|old| {
        old.task_id != link.task_id
            || old.url != link.url && !(old.kind == "qr" && link.kind == "cashier")
    })
}

#[derive(Default)]
struct DiscordDelivery {
    task_id: Option<String>,
    webhook_config: String,
    message_ids: Vec<Option<String>>,
}

impl DiscordDelivery {
    fn use_context(&mut self, task_id: &str, webhook_config: &str) {
        if self.task_id.as_deref() != Some(task_id) || self.webhook_config != webhook_config {
            self.task_id = Some(task_id.to_owned());
            self.webhook_config = webhook_config.to_owned();
            self.message_ids.clear();
        }
    }

    fn message_id(&self, index: usize) -> Option<&str> {
        self.message_ids.get(index).and_then(Option::as_deref)
    }

    fn record(&mut self, index: usize, message_id: String) {
        if self.message_ids.len() <= index {
            self.message_ids.resize(index + 1, None);
        }
        self.message_ids[index] = Some(message_id);
    }
}

impl PaymentBridge {
    pub fn disabled() -> Self {
        Self {
            token: None,
            session_path: None,
            session: RwLock::new(BridgeSession::default()),
            live_task_id: RwLock::new(None),
            discord_delivery: tokio::sync::Mutex::new(DiscordDelivery::default()),
        }
    }

    pub fn pairing_code(&self) -> Result<&str, String> {
        self.token
            .as_deref()
            .ok_or_else(|| "本机付款链接桥接未启动，请查看运行日志".to_string())
    }

    pub fn latest(&self) -> Option<PaymentLink> {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .latest
            .clone()
    }

    fn is_current_link(&self, link: &PaymentLink, task_id: &str) -> bool {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .latest
            .as_ref()
            .is_some_and(|current| {
                current.url == link.url && current.kind == link.kind && current.task_id == task_id
            })
    }

    pub fn status(&self) -> Option<BotStatus> {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    pub fn has_progressed(&self, task_id: &str, stage: &str) -> bool {
        self.status().is_some_and(|status| {
            status.task_id == task_id
                && status.stage != "failed"
                && stage_rank(&status.stage)
                    .zip(stage_rank(stage))
                    .is_some_and(|(current, expected)| current >= expected)
        })
    }

    fn commit(&self, current: &mut BridgeSession, next: BridgeSession) -> Result<(), String> {
        if let Some(path) = &self.session_path
            && let Err(err) = write_session(path, &next)
        {
            *self.live_task_id.write().unwrap_or_else(|e| e.into_inner()) = None;
            if let Some(status) = &mut current.status {
                status.automation_active = false;
            }
            return Err(err);
        }
        *current = next;
        Ok(())
    }

    fn is_active_task(&self, task: &IncomingTask) -> bool {
        self.status().is_some_and(|status| {
            status.part_number == task.part_number
                && status.store_number == task.store_number
                && status.task_id == task.task_id
                && stage_rank(&status.stage).is_some_and(|rank| (1..12).contains(&rank))
                && status.automation_active
                && self
                    .live_task_id
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
                    == Some(task.task_id.as_str())
        })
    }

    pub fn is_live_cart_task_id(&self, task_id: &str) -> bool {
        self.status().is_some_and(|status| {
            status.task_id == task_id
                && status.automation_active
                && matches!(
                    status.stage.as_str(),
                    "starting" | "waitingRelease" | "waitingRoom" | "launched"
                )
                && self
                    .live_task_id
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
                    == Some(task_id)
        })
    }

    pub fn start_task(&self, part_number: &str, store_number: &str) -> Result<BotStatus, String> {
        let status = BotStatus {
            part_number: part_number.to_owned(),
            store_number: store_number.to_owned(),
            task_id: uuid::Uuid::new_v4().simple().to_string(),
            stage: "starting".into(),
            updated_at: now(),
            automation_active: true,
        };
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        self.commit(
            &mut current,
            BridgeSession {
                status: Some(status.clone()),
                latest: None,
            },
        )?;
        *self.live_task_id.write().unwrap_or_else(|e| e.into_inner()) =
            Some(status.task_id.clone());
        Ok(status)
    }

    /// 停止监控或关闭自动任务后，旧商品页不能再凭原任务身份加车或结账。
    /// 保留阶段与付款链接；已提交订单的支付宝页面仍可回传链接。
    pub fn pause_active_task(&self) -> Result<Option<BotStatus>, String> {
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        let Some(previous) = current.status.as_ref() else {
            return Ok(None);
        };
        if !previous.automation_active {
            return Ok(None);
        }
        let next = BotStatus {
            updated_at: now(),
            automation_active: false,
            ..previous.clone()
        };
        let mut updated = current.clone();
        updated.status = Some(next.clone());
        self.commit(&mut current, updated)?;
        *self.live_task_id.write().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(Some(next))
    }

    pub fn advance_task(
        &self,
        stage: &str,
        part_number: &str,
        store_number: &str,
        task_id: &str,
    ) -> Result<Option<BotStatus>, String> {
        let Some(next_rank) = stage_rank(stage) else {
            return Ok(None);
        };
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        let Some(previous) = current.status.as_ref() else {
            return Ok(None);
        };
        if previous.part_number != part_number
            || previous.store_number != store_number
            || previous.task_id != task_id
            || stage_rank(&previous.stage).is_none_or(|rank| rank >= next_rank)
        {
            return Ok(None);
        }
        let next = BotStatus {
            part_number: part_number.to_owned(),
            store_number: store_number.to_owned(),
            task_id: task_id.to_owned(),
            stage: stage.to_owned(),
            updated_at: now(),
            automation_active: previous.automation_active && stage != "failed",
        };
        let mut updated = current.clone();
        updated.status = Some(next.clone());
        self.commit(&mut current, updated)?;
        Ok(Some(next))
    }

    pub fn fail_if_stage(
        &self,
        task_id: &str,
        expected_stage: &str,
    ) -> Result<Option<BotStatus>, String> {
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        let Some(previous) = current.status.as_ref() else {
            return Ok(None);
        };
        if previous.task_id != task_id || previous.stage != expected_stage {
            return Ok(None);
        }
        let next = BotStatus {
            stage: "failed".into(),
            updated_at: now(),
            automation_active: false,
            ..previous.clone()
        };
        let mut updated = current.clone();
        updated.status = Some(next.clone());
        self.commit(&mut current, updated)?;
        Ok(Some(next))
    }

    fn record_current(
        &self,
        incoming: &IncomingLink,
        link: PaymentLink,
    ) -> Result<Option<(bool, Option<BotStatus>)>, String> {
        // 链接与阶段一起提交。新任务须等此写锁释放后才能启动；若它随后启动，
        // 会同时清除旧链接，不会被旧任务的迟到响应推进到付款阶段。
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        if incoming.task_id.as_deref() != Some(link.task_id.as_str())
            || !link_matches_task(incoming, current.status.as_ref())
        {
            return Ok(None);
        }
        let Some(previous) = current.status.as_ref().cloned() else {
            return Ok(None);
        };
        let fresh = link_is_fresh(current.latest.as_ref(), &link);
        let advanced = (previous.stage != "paymentLink").then(|| BotStatus {
            stage: "paymentLink".into(),
            updated_at: now(),
            ..previous
        });
        let mut updated = current.clone();
        if fresh {
            updated.latest = Some(link);
        }
        if let Some(next) = &advanced {
            updated.status = Some(next.clone());
        }
        if fresh || advanced.is_some() {
            self.commit(&mut current, updated)?;
        }
        Ok(Some((fresh, advanced)))
    }

    #[cfg(test)]
    fn record(&self, link: PaymentLink) -> bool {
        let mut current = self.session.write().unwrap_or_else(|e| e.into_inner());
        let fresh = link_is_fresh(current.latest.as_ref(), &link);
        if fresh {
            current.latest = Some(link);
        }
        fresh
    }
}

fn write_session(path: &Path, session: &BridgeSession) -> Result<(), String> {
    let dir = path.parent().ok_or("结账会话路径无效")?;
    let data = serde_json::to_vec(session).map_err(|err| format!("序列化结账会话失败：{err}"))?;
    let mut staged = tempfile::NamedTempFile::new_in(dir)
        .map_err(|err| format!("创建结账会话临时文件失败：{err}"))?;
    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("保护结账会话文件失败：{err}"))?;
    staged
        .write_all(&data)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|err| format!("写入结账会话失败：{err}"))?;
    staged
        .persist(path)
        .map_err(|err| format!("替换结账会话文件失败：{}", err.error))?;
    Ok(())
}

fn read_session(path: &Path, time: u64) -> Result<BridgeSession, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(BridgeSession::default()),
        Err(err) => return Err(format!("读取结账会话失败：{err}")),
    };
    let mut data = Vec::new();
    file.take(16 * 1024 + 1)
        .read_to_end(&mut data)
        .map_err(|err| format!("读取结账会话失败：{err}"))?;
    if data.len() > 16 * 1024 {
        return Err("结账会话文件过大".into());
    }
    let mut session: BridgeSession =
        serde_json::from_slice(&data).map_err(|err| format!("解析结账会话失败：{err}"))?;
    let Some(status) = &session.status else {
        return if session.latest.is_none() {
            Ok(session)
        } else {
            Err("结账会话缺少任务身份".into())
        };
    };
    let identity = IncomingTask {
        part_number: status.part_number.clone(),
        store_number: status.store_number.clone(),
        task_id: status.task_id.clone(),
    };
    if !valid_task_identity(&identity) || stage_rank(&status.stage).is_none() {
        return Err("结账会话任务身份或阶段无效".into());
    }
    if status.stage == "paymentLink" && session.latest.is_none() {
        return Err("结账会话缺少付款链接".into());
    }
    // 尚在 Apple 验证中的活动任务可等待较久，但重启后绝不恢复自动操作。
    // 已提交订单、已收到付款链接或明确暂停的任务继续使用较短期限。
    let ttl = if status.automation_active
        && !matches!(
            status.stage.as_str(),
            "orderSubmitted" | "paymentLink" | "failed"
        ) {
        CHECKOUT_SESSION_TTL_SECONDS
    } else {
        PAYMENT_SESSION_TTL_SECONDS
    };
    if status.updated_at > time || time - status.updated_at > ttl {
        return Ok(BridgeSession::default());
    }
    if let Some(link) = &session.latest
        && (link.task_id != status.task_id
            || !matches!(status.stage.as_str(), "paymentLink")
            || !matches!(link.kind.as_str(), "qr" | "cashier")
            || !valid_alipay_url(&link.url)
            || link.received_at > time
            || time - link.received_at > PAYMENT_SESSION_TTL_SECONDS)
    {
        return Err("结账会话付款链接无效".into());
    }
    if let Some(status) = &mut session.status {
        status.automation_active = false;
    }
    Ok(session)
}

fn load_session(dir: &Path) -> Result<(BridgeSession, Option<String>), String> {
    let path = dir.join(SESSION_FILE);
    match read_session(&path, now()) {
        Ok(session) => Ok((session, None)),
        Err(err) => {
            let archived = dir.join(format!(
                "{SESSION_FILE}.corrupt-{}-{}",
                now(),
                uuid::Uuid::new_v4().simple()
            ));
            fs::rename(&path, &archived)
                .map_err(|archive_err| format!("{err}；留档失败：{archive_err}"))?;
            Ok((
                BridgeSession::default(),
                Some(format!(
                    "{err}；原文件已留档为 {}，本次不会恢复旧结账任务",
                    archived.display()
                )),
            ))
        }
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

pub fn prepare(
    app: &AppHandle,
) -> Result<(PaymentBridge, std::net::TcpListener, Option<String>), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    let token = load_or_create_token(&dir)?;
    let (session, notice) = load_session(&dir)?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", PORT))
        .map_err(|e| format!("监听本机端口 {PORT} 失败：{e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok((
        PaymentBridge {
            token: Some(token),
            session_path: Some(dir.join(SESSION_FILE)),
            session: RwLock::new(session),
            live_task_id: RwLock::new(None),
            discord_delivery: tokio::sync::Mutex::new(DiscordDelivery::default()),
        },
        listener,
        notice,
    ))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn stage_rank(stage: &str) -> Option<u8> {
    Some(match stage {
        "starting" => 0,
        "waitingRelease" => 1,
        "waitingRoom" => 2,
        "launched" => 3,
        "bag" => 4,
        "checkout" => 5,
        "guest" => 6,
        "privacy" => 7,
        "profile" => 8,
        "profileAction" => 9,
        "payment" => 10,
        "review" => 11,
        "orderSubmitted" => 12,
        "paymentLink" => 13,
        "failed" => 14,
        _ => return None,
    })
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
        409 => "Conflict",
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

async fn handle_connection<R: Runtime>(app: AppHandle<R>, mut socket: TcpStream) {
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
    if matches!(
        request_line,
        "OPTIONS /payment-link"
            | "OPTIONS /task-status"
            | "OPTIONS /task-check"
            | "OPTIONS /pairing-check"
    ) {
        respond(&mut socket, 204, origin, "").await;
        return;
    }
    if !matches!(
        request_line,
        "POST /payment-link" | "POST /task-status" | "POST /task-check" | "POST /pairing-check"
    ) {
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
    if request_line == "POST /pairing-check" {
        respond(&mut socket, 200, origin, "{\"ok\":true}").await;
        return;
    }
    if request_line == "POST /task-check" {
        let Ok(incoming) = serde_json::from_slice::<IncomingTask>(&body) else {
            respond(&mut socket, 400, origin, "{}").await;
            return;
        };
        if !valid_task_identity(&incoming) {
            respond(&mut socket, 400, origin, "{}").await;
            return;
        }
        respond(
            &mut socket,
            if bridge.is_active_task(&incoming) {
                200
            } else {
                409
            },
            origin,
            "{}",
        )
        .await;
        return;
    }
    if request_line == "POST /task-status" {
        let Ok(incoming) = serde_json::from_slice::<IncomingStatus>(&body) else {
            respond(&mut socket, 400, origin, "{}").await;
            return;
        };
        if !valid_task_status(&incoming) {
            respond(&mut socket, 400, origin, "{}").await;
            return;
        }
        let advanced = if incoming.stage == "failed" {
            if bridge.status().is_some_and(|current| {
                current.part_number == incoming.part_number
                    && current.store_number == incoming.store_number
            }) {
                bridge.fail_if_stage(&incoming.task_id, "launched")
            } else {
                Ok(None)
            }
        } else {
            bridge.advance_task(
                &incoming.stage,
                &incoming.part_number,
                &incoming.store_number,
                &incoming.task_id,
            )
        };
        let status = match advanced {
            Ok(Some(status)) => status,
            Ok(None) => {
                respond(&mut socket, 409, origin, "{}").await;
                return;
            }
            Err(err) => {
                respond(&mut socket, 500, origin, "{}").await;
                let _ = app.emit(NOTICE_CHANNEL, format!("保存结账任务状态失败：{err}"));
                return;
            }
        };
        respond(&mut socket, 200, origin, "{\"ok\":true}").await;
        let _ = app.emit(STATUS_CHANNEL, status);
        if matches!(incoming.stage.as_str(), "privacy" | "profileAction") {
            use tauri_plugin_notification::NotificationExt;

            let message = if incoming.stage == "privacy" {
                "请在独立浏览器阅读并处理 Apple 隐私同意书"
            } else {
                "请在独立浏览器补全取货资料及所需身份验证"
            };
            let _ = app.emit(NOTICE_CHANNEL, message);
            let _ = app
                .notification()
                .builder()
                .title("Apple 结账需要操作")
                .body(message)
                .show();
        }
        if incoming.stage == "failed" {
            let _ = app.emit(
                NOTICE_CHANNEL,
                "Apple 未确认商品已加入购物袋。请在独立浏览器检查购物袋；本轮已停止，避免重复加车。",
            );
        }
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
    let Some(task_id) = incoming.task_id.clone() else {
        respond(&mut socket, 409, origin, "{}").await;
        return;
    };
    let link = PaymentLink {
        url: incoming.url.clone(),
        kind: incoming.kind.clone(),
        received_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        task_id: task_id.clone(),
    };
    let (fresh, advanced) = match bridge.record_current(&incoming, link.clone()) {
        Ok(Some(result)) => result,
        Ok(None) => {
            respond(&mut socket, 409, origin, "{}").await;
            return;
        }
        Err(err) => {
            respond(&mut socket, 500, origin, "{}").await;
            let _ = app.emit(NOTICE_CHANNEL, format!("保存支付宝链接失败：{err}"));
            return;
        }
    };
    respond(&mut socket, 200, origin, "{\"ok\":true}").await;
    if let Some(status) = advanced {
        let _ = app.emit(STATUS_CHANNEL, status);
    }
    if fresh && bridge.is_current_link(&link, &task_id) {
        let _ = app.emit(EVENT_CHANNEL, &link);
        tauri::async_runtime::spawn(send_discord(app, link, task_id));
    }
}

fn valid_task_status(status: &IncomingStatus) -> bool {
    matches!(
        status.stage.as_str(),
        "bag"
            | "checkout"
            | "guest"
            | "privacy"
            | "profile"
            | "profileAction"
            | "payment"
            | "review"
            | "orderSubmitted"
            | "failed"
    ) && valid_task_identity(&IncomingTask {
        part_number: status.part_number.clone(),
        store_number: status.store_number.clone(),
        task_id: status.task_id.clone(),
    })
}

fn valid_task_identity(task: &IncomingTask) -> bool {
    valid_task_id(&task.task_id)
        && (2..=10).contains(&task.store_number.len())
        && task
            .store_number
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && task
            .part_number
            .split_once('/')
            .is_some_and(|(part, suffix)| {
                (4..=20).contains(&part.len())
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                    && suffix.len() == 1
                    && suffix.bytes().all(|byte| byte.is_ascii_uppercase())
            })
}

fn valid_task_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn link_matches_task(link: &IncomingLink, status: Option<&BotStatus>) -> bool {
    status.is_some_and(|status| {
        link.part_number.as_deref() == Some(status.part_number.as_str())
            && link.store_number.as_deref() == Some(status.store_number.as_str())
            && link.task_id.as_deref() == Some(status.task_id.as_str())
            && matches!(status.stage.as_str(), "orderSubmitted" | "paymentLink")
    })
}

async fn send_discord<R: Runtime>(app: AppHandle<R>, link: PaymentLink, task_id: String) {
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
    .with_url(link.url.clone());
    let bridge = app.state::<PaymentBridge>();
    let mut delivery = bridge.discord_delivery.lock().await;
    if !bridge.is_current_link(&link, &task_id) {
        return;
    }
    delivery.use_context(&task_id, &settings.discord_webhook);
    let mut delivered = 0;
    for (index, discord) in Discord::from_list(&settings.discord_webhook, state.http.clone())
        .into_iter()
        .enumerate()
    {
        if !bridge.is_current_link(&link, &task_id) {
            break;
        }
        let previous = delivery.message_id(index).map(str::to_owned);
        let result = send_discord_link(&discord, &notification, previous.as_deref(), || {
            bridge.is_current_link(&link, &task_id)
        })
        .await;
        let result = if previous.is_some()
            && matches!(&result, Err(NotifyError::Rejected { status: 404, .. }))
        {
            // 用户删除了旧消息时，新建一条继续提供当前付款链接。
            send_discord_link(&discord, &notification, None, || {
                bridge.is_current_link(&link, &task_id)
            })
            .await
        } else {
            result
        };
        match result {
            Ok(Some(message_id)) => {
                delivery.record(index, message_id);
                delivered += 1;
            }
            Ok(None) => break,
            Err(err) => {
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    format!("支付宝链接发送到 Discord 失败：{err}"),
                );
            }
        }
    }
    if delivered > 0 {
        let _ = app.emit(
            NOTICE_CHANNEL,
            format!("支付宝链接已发送到 {delivered} 个 Discord webhook"),
        );
    }
}

async fn send_discord_link(
    discord: &Discord,
    notification: &Notification,
    previous: Option<&str>,
    is_current: impl Fn() -> bool,
) -> Result<Option<String>, NotifyError> {
    for attempt in 0..3 {
        if !is_current() {
            return Ok(None);
        }
        match discord.send_or_edit(notification, previous).await {
            Err(NotifyError::Rejected {
                status: 429,
                retry_after: Some(delay),
                ..
            }) if attempt < 2 && delay <= Duration::from_secs(10) => {
                tokio::time::sleep(delay).await;
            }
            result => return result.map(Some),
        }
    }
    unreachable!("the final attempt always returns")
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeSession, DiscordDelivery, IncomingLink, IncomingStatus, IncomingTask, PaymentBridge,
        PaymentLink, handle_connection, link_matches_task, load_session, now, read_request,
        read_session, respond, send_discord_link, valid_alipay_url, valid_origin,
        valid_task_status, write_session,
    };
    use crate::AppState;
    use apw_core::apple::{AppleClient, ClientConfig};
    use apw_core::catalog::Catalog;
    use apw_core::config::Settings;
    use apw_core::notify::{Discord, Notification};
    use apw_core::watcher::{Watcher, WatcherConfig};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tauri::Manager;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn real_http_bridge_accepts_only_submitted_task_payment_links() {
        let app = tauri::test::mock_app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout-session.v1.json");
        let mut bridge = PaymentBridge::disabled();
        bridge.token = Some("a".repeat(32));
        bridge.session_path = Some(path.clone());
        let task = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &task.task_id)
            .unwrap();
        app.manage(bridge);
        let apple = AppleClient::new(ClientConfig::default()).unwrap();
        let (watcher, _events, _engine) = Watcher::new(apple.clone(), WatcherConfig::default());
        app.manage(AppState {
            watcher,
            client: apple,
            catalog: Catalog::new(),
            http: reqwest::Client::new(),
            browser_install: tokio::sync::Mutex::new(()),
            settings: std::sync::RwLock::new(Settings::default()),
            store: None,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = app.handle().clone();
        let server = tokio::spawn(async move {
            for _ in 0..6 {
                let (socket, _) = listener.accept().await.unwrap();
                handle_connection(handle.clone(), socket).await;
            }
        });
        let client = reqwest::Client::new();
        let post = |route: &'static str, body: serde_json::Value| {
            client
                .post(format!("{base}{route}"))
                .header("Origin", format!("chrome-extension://{}", "a".repeat(32)))
                .bearer_auth("a".repeat(32))
                .json(&body)
        };
        let identity = serde_json::json!({
            "partNumber": "MJYE4CH/A", "storeNumber": "R683", "taskId": task.task_id,
        });
        let check = post("/task-check", identity.clone()).send().await.unwrap();
        assert_eq!(check.status(), 200);
        assert_eq!(
            check.headers()["access-control-allow-origin"],
            format!("chrome-extension://{}", "a".repeat(32))
        );
        let mut payment = identity.clone();
        payment["kind"] = "qr".into();
        payment["url"] = "https://qr.alipay.com/fixture".into();
        assert_eq!(
            post("/payment-link", payment.clone())
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        let mut submitted = identity.clone();
        submitted["stage"] = "orderSubmitted".into();
        assert_eq!(
            post("/task-status", submitted.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            post("/task-status", submitted)
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
        assert_eq!(
            post("/payment-link", payment.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        payment["kind"] = "cashier".into();
        payment["url"] = "https://excashier.alipay.com/fixture".into();
        assert_eq!(
            post("/payment-link", payment)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        server.await.unwrap();
        let saved = read_session(&path, now()).unwrap();
        assert_eq!(saved.status.unwrap().stage, "paymentLink");
        assert_eq!(saved.latest.unwrap().url, "https://qr.alipay.com/fixture");
    }

    #[tokio::test]
    #[ignore = "requires APW_SMOKE_PYTHON and APW_CHROME_PATH; uses a disposable Chrome profile and local fixtures"]
    async fn chrome_extension_completes_checkout_through_real_payment_bridge() {
        let python = std::env::var("APW_SMOKE_PYTHON").expect("set APW_SMOKE_PYTHON");
        let chrome = std::env::var("APW_CHROME_PATH").expect("set APW_CHROME_PATH");
        let app = tauri::test::mock_app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout-session.v1.json");
        let mut bridge = PaymentBridge::disabled();
        bridge.token = Some("a".repeat(32));
        bridge.session_path = Some(path.clone());
        let task = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &task.task_id)
            .unwrap();
        let task_id = task.task_id;
        app.manage(bridge);
        let apple = AppleClient::new(ClientConfig::default()).unwrap();
        let (watcher, _events, _engine) = Watcher::new(apple.clone(), WatcherConfig::default());
        app.manage(AppState {
            watcher,
            client: apple,
            catalog: Catalog::new(),
            http: reqwest::Client::new(),
            browser_install: tokio::sync::Mutex::new(()),
            settings: std::sync::RwLock::new(Settings::default()),
            store: None,
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = app.handle().clone();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let app = handle.clone();
                tokio::spawn(async move { handle_connection(app, socket).await });
            }
        });
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/browser-flow-smoke.py");
        let output = tokio::task::spawn_blocking(move || {
            std::process::Command::new(python)
                .arg(script)
                .env("APW_EXTERNAL_BRIDGE_PORT", port.to_string())
                .env("APW_TASK_ID", task_id)
                .env("APW_CHROME_PATH", chrome)
                .output()
        })
        .await
        .unwrap()
        .unwrap();
        server.abort();
        assert!(
            output.status.success(),
            "Chrome fixture failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let bridge = app.state::<PaymentBridge>();
        assert_eq!(bridge.status().unwrap().stage, "paymentLink");
        let link = bridge.latest().unwrap();
        assert_eq!(link.kind, "qr");
        assert_eq!(link.url, "https://qr.alipay.com/apw-local-test-only");
        let saved = read_session(&path, now()).unwrap();
        assert_eq!(
            saved.latest.unwrap().url,
            "https://qr.alipay.com/apw-local-test-only"
        );
    }

    async fn read_webhook_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let count = socket.read(&mut chunk).await.unwrap();
            assert!(count > 0, "webhook request ended before its body");
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let header = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + content_length {
                    return bytes;
                }
            }
        }
    }

    #[tokio::test]
    async fn discord_payment_link_retries_a_rate_limit_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, headers, body) in [
                (
                    "429 Too Many Requests",
                    "Retry-After: 0.01\r\n",
                    r#"{"retry_after":0.01}"#,
                ),
                ("200 OK", "", r#"{"id":"123456"}"#),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_webhook_request(&mut socket).await;
                assert!(
                    String::from_utf8_lossy(&request)
                        .starts_with("POST /api/webhooks/123/token?wait=true ")
                );
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let discord = Discord::new(
            format!("http://{addr}/api/webhooks/123/token"),
            reqwest::Client::new(),
        );
        let notice = Notification::new("Apple 支付宝付款链接", "付款链接")
            .with_url("https://qr.alipay.com/test");
        let delivered = send_discord_link(&discord, &notice, None, || true)
            .await
            .unwrap();
        assert_eq!(delivered.as_deref(), Some("123456"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discord_payment_link_drops_a_stale_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let current = Arc::new(AtomicBool::new(true));
        let server_current = current.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_webhook_request(&mut socket).await;
            server_current.store(false, Ordering::SeqCst);
            let body = r#"{"retry_after":0.01}"#;
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let discord = Discord::new(
            format!("http://{addr}/api/webhooks/123/token"),
            reqwest::Client::new(),
        );
        let notice = Notification::new("Apple 支付宝付款链接", "过期链接")
            .with_url("https://qr.alipay.com/old");
        let result = send_discord_link(&discord, &notice, None, || current.load(Ordering::SeqCst))
            .await
            .unwrap();
        assert!(result.is_none());
        server.await.unwrap();
    }

    fn persistent_bridge(path: &Path, session: BridgeSession) -> PaymentBridge {
        let mut bridge = PaymentBridge::disabled();
        bridge.session_path = Some(path.to_owned());
        *bridge.session.write().unwrap() = session;
        bridge
    }

    #[test]
    fn restart_restores_checkout_identity_and_payment_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout-session.v1.json");
        let bridge = persistent_bridge(&path, BridgeSession::default());
        let task = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        bridge
            .advance_task("review", "MJYE4CH/A", "R683", &task.task_id)
            .unwrap();
        let mut saved = bridge.session.read().unwrap().clone();
        saved.status.as_mut().unwrap().updated_at = now().saturating_sub(2 * 60 * 60);
        write_session(&path, &saved).unwrap();

        let resumed = persistent_bridge(&path, read_session(&path, now()).unwrap());
        assert!(!resumed.is_active_task(&IncomingTask {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: task.task_id.clone(),
        }));
        let restored_status = resumed.status().unwrap();
        assert_eq!(restored_status.stage, "review");
        assert!(!restored_status.automation_active);
        resumed
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &task.task_id)
            .unwrap();
        let incoming = IncomingLink {
            url: "https://qr.alipay.com/current-order".into(),
            kind: "qr".into(),
            part_number: Some("MJYE4CH/A".into()),
            store_number: Some("R683".into()),
            task_id: Some(task.task_id.clone()),
        };
        let link = PaymentLink {
            url: incoming.url.clone(),
            kind: incoming.kind.clone(),
            received_at: now(),
            task_id: task.task_id.clone(),
        };
        assert!(
            resumed
                .record_current(&incoming, link.clone())
                .unwrap()
                .unwrap()
                .0
        );

        let after_payment = persistent_bridge(&path, read_session(&path, now()).unwrap());
        assert_eq!(after_payment.status().unwrap().stage, "paymentLink");
        assert_eq!(after_payment.latest().unwrap().url, link.url);
        after_payment.start_task("MG724CH/A", "R683").unwrap();
        let after_next_task = persistent_bridge(&path, read_session(&path, now()).unwrap());
        assert!(after_next_task.latest().is_none());
        assert!(
            after_next_task
                .record_current(&incoming, link)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_or_damaged_session_never_resumes_and_damaged_file_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout-session.v1.json");
        let bridge = persistent_bridge(&path, BridgeSession::default());
        bridge.start_task("MJYE4CH/A", "R683").unwrap();
        assert!(
            read_session(&path, now() + 60 * 60 + 1)
                .unwrap()
                .status
                .is_some()
        );
        assert!(
            read_session(&path, now() + 4 * 60 * 60 + 1)
                .unwrap()
                .status
                .is_none()
        );
        bridge.pause_active_task().unwrap();
        assert!(
            read_session(&path, now() + 60 * 60 + 1)
                .unwrap()
                .status
                .is_none()
        );
        let task = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        bridge
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &task.task_id)
            .unwrap();
        assert!(
            read_session(&path, now() + 60 * 60 + 1)
                .unwrap()
                .status
                .is_none()
        );
        std::fs::write(&path, b"{broken").unwrap();
        let (loaded, notice) = load_session(dir.path()).unwrap();
        assert!(loaded.status.is_none());
        assert!(notice.unwrap().contains("留档"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_session_write_does_not_replace_current_task() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout-session.v1.json");
        let mut bridge = persistent_bridge(&path, BridgeSession::default());
        let first = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &first.task_id)
            .unwrap();
        let old_task = IncomingTask {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: first.task_id.clone(),
        };
        assert!(bridge.is_active_task(&old_task));
        let occupied = dir.path().join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        bridge.session_path = Some(occupied);
        assert!(bridge.start_task("MG724CH/A", "R683").is_err());
        assert!(!bridge.is_active_task(&old_task));
        assert_eq!(bridge.status().unwrap().task_id, first.task_id);
        assert_eq!(
            read_session(&path, now()).unwrap().status.unwrap().task_id,
            first.task_id
        );
    }

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

    #[test]
    fn discord_message_is_reused_only_within_the_same_task_and_webhook_set() {
        let mut delivery = DiscordDelivery::default();
        delivery.use_context("task-one", "hook-a;hook-b");
        delivery.record(0, "101".into());
        delivery.record(1, "102".into());
        delivery.use_context("task-one", "hook-a;hook-b");
        assert_eq!(delivery.message_id(0), Some("101"));
        assert_eq!(delivery.message_id(1), Some("102"));

        delivery.use_context("task-two", "hook-a;hook-b");
        assert_eq!(delivery.message_id(0), None);
        delivery.record(0, "201".into());
        delivery.use_context("task-two", "hook-b;hook-a");
        assert_eq!(delivery.message_id(0), None);
    }

    #[test]
    fn task_status_follows_one_target_and_never_moves_backward() {
        let bridge = PaymentBridge::disabled();
        let task_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        assert!(
            bridge
                .advance_task("bag", "MJYE4CH/A", "R683", &task_id)
                .unwrap()
                .is_some()
        );
        assert!(bridge.has_progressed(&task_id, "launched"));
        assert!(!bridge.has_progressed("other-task", "launched"));
        assert!(
            bridge
                .advance_task("guest", "MJYE4CH/A", "R683", &task_id)
                .unwrap()
                .is_some()
        );
        assert!(
            bridge
                .advance_task("checkout", "MJYE4CH/A", "R683", &task_id)
                .unwrap()
                .is_none()
        );
        assert!(
            bridge
                .advance_task("payment", "MJYE4CH/A", "R999", &task_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(bridge.status().unwrap().stage, "guest");
        assert!(valid_task_status(&IncomingStatus {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: task_id.clone(),
            stage: "profile".into(),
        }));
        assert!(valid_task_status(&IncomingStatus {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: task_id.clone(),
            stage: "failed".into(),
        }));
        assert!(!valid_task_status(&IncomingStatus {
            part_number: "../bag".into(),
            store_number: "R683".into(),
            task_id,
            stage: "profile".into(),
        }));
    }

    #[test]
    fn active_task_check_rejects_replaced_failed_and_submitted_tasks() {
        let bridge = PaymentBridge::disabled();
        let first_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        let first = IncomingTask {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: first_id.clone(),
        };
        assert!(!bridge.is_active_task(&first));
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &first_id)
            .unwrap();
        assert!(bridge.is_active_task(&first));
        let second_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        assert!(!bridge.is_active_task(&first));
        let second = IncomingTask {
            task_id: second_id.clone(),
            ..first
        };
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &second_id)
            .unwrap();
        bridge
            .advance_task("review", "MJYE4CH/A", "R683", &second_id)
            .unwrap();
        assert!(bridge.is_active_task(&second));
        bridge
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &second_id)
            .unwrap();
        assert!(!bridge.is_active_task(&second));
        let third_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &third_id)
            .unwrap();
        bridge.fail_if_stage(&third_id, "launched").unwrap();
        assert!(!bridge.is_active_task(&IncomingTask {
            task_id: third_id,
            ..second
        }));
    }

    #[test]
    fn paused_monitor_disables_old_cart_and_checkout_actions() {
        let bridge = PaymentBridge::disabled();
        let task_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        let task = IncomingTask {
            part_number: "MJYE4CH/A".into(),
            store_number: "R683".into(),
            task_id: task_id.clone(),
        };
        assert!(bridge.is_live_cart_task_id(&task_id));
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &task_id)
            .unwrap();
        assert!(bridge.is_active_task(&task));
        let paused = bridge.pause_active_task().unwrap().unwrap();
        assert_eq!(paused.stage, "launched");
        assert!(!paused.automation_active);
        assert!(!bridge.is_active_task(&task));
        assert!(!bridge.is_live_cart_task_id(&task_id));
        assert!(bridge.pause_active_task().unwrap().is_none());
        let next = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        assert_ne!(next.task_id, task_id);
        assert!(!bridge.is_active_task(&task));
    }

    #[test]
    fn pausing_after_order_submission_keeps_payment_link_handoff() {
        let bridge = PaymentBridge::disabled();
        let task_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        bridge
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &task_id)
            .unwrap();
        bridge.pause_active_task().unwrap();
        let incoming = IncomingLink {
            url: "https://qr.alipay.com/paused-order".into(),
            kind: "qr".into(),
            part_number: Some("MJYE4CH/A".into()),
            store_number: Some("R683".into()),
            task_id: Some(task_id.clone()),
        };
        let link = PaymentLink {
            url: incoming.url.clone(),
            kind: incoming.kind.clone(),
            received_at: now(),
            task_id,
        };
        assert!(bridge.record_current(&incoming, link).unwrap().unwrap().0);
        assert_eq!(bridge.latest().unwrap().url, incoming.url);
    }

    #[test]
    fn stale_launch_watchdog_cannot_fail_a_progressed_or_new_task() {
        let bridge = PaymentBridge::disabled();
        let old_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        bridge
            .advance_task("launched", "MJYE4CH/A", "R683", &old_id)
            .unwrap();
        bridge
            .advance_task("bag", "MJYE4CH/A", "R683", &old_id)
            .unwrap();
        assert!(bridge.fail_if_stage(&old_id, "launched").unwrap().is_none());
        assert_eq!(bridge.status().unwrap().stage, "bag");

        let new_id = bridge.start_task("MG724CH/A", "R683").unwrap().task_id;
        bridge
            .advance_task("launched", "MG724CH/A", "R683", &new_id)
            .unwrap();
        assert!(bridge.fail_if_stage(&old_id, "launched").unwrap().is_none());
        assert_eq!(
            bridge
                .fail_if_stage(&new_id, "launched")
                .unwrap()
                .unwrap()
                .stage,
            "failed"
        );
    }

    #[test]
    fn manual_checkout_actions_advance_before_payment() {
        let bridge = PaymentBridge::disabled();
        let task_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        for stage in [
            "launched",
            "bag",
            "checkout",
            "guest",
            "privacy",
            "profile",
            "profileAction",
            "payment",
            "review",
        ] {
            assert_eq!(
                bridge
                    .advance_task(stage, "MJYE4CH/A", "R683", &task_id)
                    .unwrap()
                    .unwrap()
                    .stage,
                stage
            );
        }
        assert!(
            bridge
                .advance_task("privacy", "MJYE4CH/A", "R683", &task_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn payment_link_cannot_complete_another_active_task() {
        let bridge = PaymentBridge::disabled();
        let first_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        let link = IncomingLink {
            url: "https://qr.alipay.com/abc123".into(),
            kind: "qr".into(),
            part_number: Some("MJYE4CH/A".into()),
            store_number: Some("R683".into()),
            task_id: Some(first_id.clone()),
        };
        assert!(!link_matches_task(&link, None));
        assert!(!link_matches_task(&link, bridge.status().as_ref()));
        bridge
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &first_id)
            .unwrap();
        assert!(link_matches_task(&link, bridge.status().as_ref()));
        let second_id = bridge.start_task("MJYE4CH/A", "R683").unwrap().task_id;
        assert_ne!(first_id, second_id);
        assert!(
            bridge
                .advance_task("review", "MJYE4CH/A", "R683", &first_id)
                .unwrap()
                .is_none()
        );
        assert!(!link_matches_task(&link, bridge.status().as_ref()));
    }

    #[test]
    fn starting_another_task_clears_its_predecessors_link_and_rejects_late_delivery() {
        let bridge = PaymentBridge::disabled();
        let old_status = bridge.start_task("MJYE4CH/A", "R683").unwrap();
        let old_id = old_status.task_id.clone();
        assert_eq!(serde_json::to_value(&old_status).unwrap()["taskId"], old_id);
        bridge
            .advance_task("orderSubmitted", "MJYE4CH/A", "R683", &old_id)
            .unwrap();
        let incoming = IncomingLink {
            url: "https://qr.alipay.com/old-order".into(),
            kind: "qr".into(),
            part_number: Some("MJYE4CH/A".into()),
            store_number: Some("R683".into()),
            task_id: Some(old_id),
        };
        let link = PaymentLink {
            url: incoming.url.clone(),
            kind: incoming.kind.clone(),
            received_at: 1,
            task_id: incoming.task_id.clone().unwrap(),
        };
        let (fresh, advanced) = bridge
            .record_current(&incoming, link.clone())
            .unwrap()
            .unwrap();
        assert!(fresh);
        assert_eq!(advanced.unwrap().stage, "paymentLink");
        assert_eq!(bridge.latest().unwrap().url, link.url);
        assert_eq!(
            serde_json::to_value(bridge.latest().unwrap()).unwrap()["taskId"],
            incoming.task_id.as_deref().unwrap()
        );

        let refreshed = IncomingLink {
            url: "https://qr.alipay.com/new-order-code".into(),
            kind: incoming.kind.clone(),
            part_number: incoming.part_number.clone(),
            store_number: incoming.store_number.clone(),
            task_id: incoming.task_id.clone(),
        };
        let updated_link = PaymentLink {
            url: refreshed.url.clone(),
            ..link.clone()
        };
        let (fresh, advanced) = bridge
            .record_current(&refreshed, updated_link)
            .unwrap()
            .unwrap();
        assert!(fresh);
        assert!(advanced.is_none(), "二维码刷新不应重复推进任务阶段");

        let new_status = bridge.start_task("MG724CH/A", "R683").unwrap();
        assert!(bridge.latest().is_none());
        assert!(bridge.record_current(&incoming, link).unwrap().is_none());
        assert!(bridge.latest().is_none());
        assert_eq!(bridge.status().unwrap().task_id, new_status.task_id);
        assert_eq!(bridge.status().unwrap().stage, "starting");
    }

    #[test]
    fn same_cashier_url_is_new_for_a_new_task() {
        let bridge = PaymentBridge::disabled();
        let link = PaymentLink {
            url: "https://excashier.alipay.com/payment.htm".into(),
            kind: "cashier".into(),
            received_at: 1,
            task_id: "first".into(),
        };
        assert!(bridge.record(link.clone()));
        assert!(!bridge.record(link.clone()));
        assert!(bridge.record(PaymentLink {
            task_id: "second".into(),
            ..link
        }));
    }

    #[test]
    fn cashier_fallback_cannot_replace_a_qr_from_the_same_task() {
        let bridge = PaymentBridge::disabled();
        let cashier = PaymentLink {
            url: "https://excashier.alipay.com/payment.htm".into(),
            kind: "cashier".into(),
            received_at: 1,
            task_id: "task-one".into(),
        };
        let qr = PaymentLink {
            url: "https://qr.alipay.com/test-code".into(),
            kind: "qr".into(),
            received_at: 2,
            task_id: "task-one".into(),
        };
        assert!(bridge.record(cashier.clone()));
        assert!(bridge.record(qr.clone()));
        assert!(!bridge.record(cashier.clone()));
        assert_eq!(bridge.latest().unwrap().url, qr.url);
        assert!(bridge.record(PaymentLink {
            task_id: "task-two".into(),
            ..cashier
        }));
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
