//! 为自动购买任务使用独立的 Chrome for Testing 配置文件。
//! 只启动官方浏览器和仓库自带的扩展；Apple 页面上的动作仍由扩展执行。

use std::fs::{self, File};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};
use tungstenite::{Message, WebSocket};

const RELEASES_URL: &str = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
const VERSION_FILE: &str = "current-version";
const DEVTOOLS_FILE: &str = "DevToolsActivePort";
static BROWSER_LAUNCH: Mutex<()> = Mutex::new(());

#[derive(Debug, Deserialize)]
struct Releases {
    channels: Channels,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Channels {
    stable: Release,
}

#[derive(Debug, Deserialize)]
struct Release {
    version: String,
    downloads: Downloads,
}

#[derive(Debug, Deserialize)]
struct Downloads {
    chrome: Vec<Download>,
}

#[derive(Debug, Deserialize)]
struct Download {
    platform: String,
    url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserReady {
    pub version: String,
}

fn platform() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("windows", "x86_64") => Ok("win64"),
        ("windows", "x86") => Ok("win32"),
        _ => Err("此系统尚不支持独立浏览器".into()),
    }
}

fn executable(dir: &Path, platform: &str) -> PathBuf {
    match platform {
        "mac-arm64" => dir.join("chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
        "mac-x64" => dir.join("chrome-mac-x64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
        "linux-arm64" => dir.join("chrome-linux-arm64/chrome"),
        "linux64" => dir.join("chrome-linux64/chrome"),
        "win64" => dir.join("chrome-win64/chrome.exe"),
        "win32" => dir.join("chrome-win32/chrome.exe"),
        _ => dir.join("missing-browser"),
    }
}

fn root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("browser-bot"))
        .map_err(|e| format!("无法定位浏览器资料目录：{e}"))
}

fn extension_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let extension = app
        .path()
        .resolve("browser-extension", BaseDirectory::Resource)
        .map_err(|e| format!("定位扩展失败：{e}"))?;
    if !extension.join("manifest.json").is_file() {
        return Err("应用包内缺少浏览器扩展".into());
    }
    Ok(extension)
}

/// Chromium 对无 manifest key 的解压扩展以路径的原始字节生成 ID。
/// 固定 manifest key 后，即使应用被移动，扩展也会继续使用原来的 Chrome 存储。
fn extension_path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut path_utf16: Vec<u16> = path.as_os_str().encode_wide().collect();
        if path_utf16.len() >= 2
            && path_utf16[1] == u16::from(b':')
            && (u16::from(b'a')..=u16::from(b'z')).contains(&path_utf16[0])
        {
            path_utf16[0] -= 32;
        }
        path_utf16
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
}

fn extension_id_from_key(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut id = String::with_capacity(32);
    for &byte in &hash[..16] {
        id.push(char::from(b'a' + (byte >> 4)));
        id.push(char::from(b'a' + (byte & 0x0f)));
    }
    id
}

#[cfg(test)]
fn extension_id(path: &Path) -> String {
    extension_id_from_key(&extension_path_bytes(path))
}

#[derive(Debug)]
struct InstalledExtension {
    dir: PathBuf,
    id: String,
}

fn read_extension_key(path: &Path) -> Result<Vec<u8>, String> {
    let encoded = fs::read_to_string(path).map_err(|e| format!("读取扩展身份失败：{e}"))?;
    let key = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| "已保存的扩展身份损坏；请保留浏览器资料并检查 extension-key".to_owned())?;
    if key.is_empty() {
        return Err("已保存的扩展身份为空；请保留浏览器资料并检查 extension-key".into());
    }
    Ok(key)
}

/// Chrome 把旧解压扩展的路径和 ID 放在独立配置的 Secure Preferences 中。
/// 只读取扩展元数据，不读取联系人、地址或结账资料。
fn legacy_extension_key(root: &Path) -> Result<Option<Vec<u8>>, String> {
    let mut candidates = Vec::<Vec<u8>>::new();
    let mut built_in_ids = std::collections::HashSet::<String>::new();
    for file in ["Secure Preferences", "Preferences"] {
        let path = root.join("profile/Default").join(file);
        if !path.exists() {
            continue;
        }
        let data = fs::read(&path).map_err(|e| format!("读取 Chrome 扩展元数据失败：{e}"))?;
        let prefs: Value = serde_json::from_slice(&data)
            .map_err(|e| format!("Chrome 扩展元数据损坏，已保留原文件：{e}"))?;
        let Some(settings) = prefs
            .pointer("/extensions/settings")
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (id, setting) in settings {
            let location = setting.get("location").and_then(Value::as_u64);
            if location == Some(5) {
                built_in_ids.insert(id.clone());
            }
            if location != Some(8) {
                continue;
            }
            let Some(raw_path) = setting.get("path").and_then(Value::as_str) else {
                continue;
            };
            let old_path = Path::new(raw_path);
            if old_path
                .file_name()
                .is_none_or(|name| name != "browser-extension")
                && !old_path.starts_with(root.join("extensions"))
            {
                continue;
            }
            let key = if old_path.join("manifest.json").is_file() {
                let manifest: Value = serde_json::from_slice(
                    &fs::read(old_path.join("manifest.json"))
                        .map_err(|e| format!("读取旧扩展清单失败：{e}"))?,
                )
                .map_err(|e| format!("旧扩展清单损坏：{e}"))?;
                if !matches!(
                    manifest.get("name").and_then(Value::as_str),
                    Some("Apple Pickup Watcher" | "Apple Bot")
                ) {
                    return Err("Chrome 配置中的扩展路径与 Apple Bot 不匹配".into());
                }
                match manifest.get("key").and_then(Value::as_str) {
                    Some(encoded) => base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|_| "旧扩展清单中的身份无效".to_owned())?,
                    None => extension_path_bytes(old_path),
                }
            } else {
                extension_path_bytes(old_path)
            };
            if extension_id_from_key(&key) != *id {
                return Err("Chrome 扩展 ID 与原路径不一致；已保留原浏览器资料".into());
            }
            if !candidates.contains(&key) {
                candidates.push(key);
            }
        }
    }
    match candidates.len() {
        0 => {
            let storage = root.join("profile/Default/Local Extension Settings");
            if storage.is_dir() {
                for entry in
                    fs::read_dir(storage).map_err(|e| format!("检查旧扩展存储失败：{e}"))?
                {
                    let name = entry
                        .map_err(|e| format!("检查旧扩展存储失败：{e}"))?
                        .file_name();
                    let id = name.to_string_lossy();
                    if id.len() == 32
                        && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte))
                        && !built_in_ids.contains(id.as_ref())
                    {
                        return Err(
                            "浏览器中已有扩展资料，但无法确认原扩展身份；已保留原资料".into()
                        );
                    }
                }
            }
            Ok(None)
        }
        1 => Ok(candidates.pop()),
        _ => Err("浏览器资料中找到多个旧扩展身份，无法安全选择；已保留原资料".into()),
    }
}

fn extension_key(root: &Path, bundled: &Path) -> Result<Vec<u8>, String> {
    let path = root.join("extension-key");
    if path.exists() {
        return read_extension_key(&path);
    }
    let key = legacy_extension_key(root)?.unwrap_or_else(|| extension_path_bytes(bundled));
    fs::create_dir_all(root).map_err(|e| format!("建立浏览器资料目录失败：{e}"))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(root).map_err(|e| format!("保存扩展身份失败：{e}"))?;
    temporary
        .write_all(
            base64::engine::general_purpose::STANDARD
                .encode(&key)
                .as_bytes(),
        )
        .map_err(|e| format!("保存扩展身份失败：{e}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| format!("保存扩展身份失败：{e}"))?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => Ok(key),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_extension_key(&path)
        }
        Err(error) => Err(format!("保存扩展身份失败：{}", error.error)),
    }
}

fn extension_files(source: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let mut files = Vec::new();
    let mut dirs = vec![source.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| format!("读取内置扩展失败：{e}"))? {
            let entry = entry.map_err(|e| format!("读取内置扩展失败：{e}"))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("读取内置扩展失败：{e}"))?;
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(source)
                    .map_err(|e| format!("读取内置扩展失败：{e}"))?
                    .to_path_buf();
                files.push((
                    relative,
                    fs::read(entry.path()).map_err(|e| format!("读取内置扩展失败：{e}"))?,
                ));
            } else {
                return Err("内置扩展包含不支持的文件类型".into());
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn install_extension(root: &Path, bundled: &Path) -> Result<InstalledExtension, String> {
    let key = extension_key(root, bundled)?;
    let id = extension_id_from_key(&key);
    let files = extension_files(bundled)?;
    let mut hash = Sha256::new();
    for (path, bytes) in &files {
        let name = path.to_str().ok_or("内置扩展文件名无效")?;
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    let version = format!("{:x}", hash.finalize());
    let manifest = files
        .iter()
        .find(|(path, _)| path == Path::new("manifest.json"))
        .ok_or("应用包内缺少浏览器扩展清单")?;
    let mut manifest: Value =
        serde_json::from_slice(&manifest.1).map_err(|e| format!("内置扩展清单损坏：{e}"))?;
    let object = manifest.as_object_mut().ok_or("内置扩展清单不是对象")?;
    object.insert(
        "key".into(),
        Value::String(base64::engine::general_purpose::STANDARD.encode(&key)),
    );
    let manifest =
        serde_json::to_vec_pretty(&manifest).map_err(|e| format!("生成扩展清单失败：{e}"))?;
    let extensions = root.join("extensions");
    let destination = extensions.join(version);
    let verify = || -> Result<(), String> {
        for (path, bytes) in &files {
            let expected = if path == Path::new("manifest.json") {
                &manifest
            } else {
                bytes
            };
            if fs::read(destination.join(path)).ok().as_deref() != Some(expected) {
                return Err("已安装的扩展副本损坏；请保留浏览器资料并检查扩展目录".into());
            }
        }
        Ok(())
    };
    if destination.exists() {
        verify()?;
        return Ok(InstalledExtension {
            dir: destination,
            id,
        });
    }
    fs::create_dir_all(&extensions).map_err(|e| format!("建立扩展目录失败：{e}"))?;
    let staging = extensions.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        fs::create_dir(&staging).map_err(|e| format!("建立扩展副本失败：{e}"))?;
        for (path, bytes) in &files {
            let target = staging.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("复制扩展失败：{e}"))?;
            }
            fs::write(
                &target,
                if path == Path::new("manifest.json") {
                    &manifest
                } else {
                    bytes
                },
            )
            .map_err(|e| format!("复制扩展失败：{e}"))?;
        }
        match fs::rename(&staging, &destination) {
            Ok(()) => Ok(()),
            Err(_) if destination.exists() => verify(),
            Err(e) => Err(format!("安装扩展失败：{e}")),
        }
    })();
    let _ = fs::remove_dir_all(&staging);
    result?;
    Ok(InstalledExtension {
        dir: destination,
        id,
    })
}

fn installed_extension(app: &AppHandle) -> Result<InstalledExtension, String> {
    install_extension(&root(app)?, &extension_dir(app)?)
}

pub(crate) fn installed_extension_dir(app: &AppHandle) -> Result<PathBuf, String> {
    installed_extension(app).map(|extension| extension.dir)
}

fn current(root: &Path, platform: &str) -> Option<(String, PathBuf)> {
    let version = fs::read_to_string(root.join(VERSION_FILE)).ok()?;
    let version = version.trim();
    if !valid_version(version) {
        return None;
    }
    let binary = executable(&root.join("versions").join(version), platform);
    binary.is_file().then(|| (version.to_owned(), binary))
}

fn valid_version(version: &str) -> bool {
    version.len() <= 40
        && version.split('.').count() == 4
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn trusted_download(url: &str, version: &str, platform: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("storage.googleapis.com")
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.path()
            == format!("/chrome-for-testing-public/{version}/{platform}/chrome-{platform}.zip")
}

fn unpack(archive: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let status = Command::new("ditto")
        .args(["-xk"])
        .arg(archive)
        .arg(destination)
        .status();
    #[cfg(target_os = "linux")]
    let status = Command::new("unzip")
        .arg("-q")
        .arg(archive)
        .arg("-d")
        .arg(destination)
        .status();
    #[cfg(target_os = "windows")]
    let status = {
        let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
        Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "Expand-Archive -LiteralPath '{}' -DestinationPath '{}'",
                quote(archive),
                quote(destination)
            ))
            .status()
    };
    let status = status.map_err(|e| format!("解压浏览器失败：{e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("解压浏览器失败：{status}"))
    }
}

/// 由用户在界面中明确点击准备后下载 Google 官方 Chrome for Testing。
pub(crate) async fn prepare(
    app: &AppHandle,
    http: &reqwest::Client,
) -> Result<BrowserReady, String> {
    let platform = platform()?;
    let root = root(app)?;
    if let Some((version, _)) = current(&root, platform) {
        return Ok(BrowserReady { version });
    }

    let releases = http
        .get(RELEASES_URL)
        .send()
        .await
        .map_err(|e| format!("查询 Chrome for Testing 版本失败：{e}"))?
        .error_for_status()
        .map_err(|e| format!("查询 Chrome for Testing 版本失败：{e}"))?
        .json::<Releases>()
        .await
        .map_err(|e| format!("解析浏览器版本失败：{e}"))?;
    let release = releases.channels.stable;
    if !valid_version(&release.version) {
        return Err("浏览器版本号无效".into());
    }
    let download = release
        .downloads
        .chrome
        .iter()
        .find(|item| item.platform == platform)
        .ok_or_else(|| format!("Google 当前未提供 {platform} 的浏览器"))?;
    if !trusted_download(&download.url, &release.version, platform) {
        return Err("浏览器下载地址不符合 Google 官方格式".into());
    }

    fs::create_dir_all(root.join("versions")).map_err(|e| format!("建立浏览器目录失败：{e}"))?;
    let id = uuid::Uuid::new_v4();
    let archive = root.join(format!("download-{id}.zip"));
    let staging = root.join(format!("extract-{id}"));
    let destination = root.join("versions").join(&release.version);
    let result = async {
        let mut response = http
            .get(&download.url)
            .send()
            .await
            .map_err(|e| format!("下载浏览器失败：{e}"))?
            .error_for_status()
            .map_err(|e| format!("下载浏览器失败：{e}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > 500_000_000)
        {
            return Err("浏览器压缩包异常大".into());
        }
        let mut file = File::create(&archive).map_err(|e| format!("保存浏览器失败：{e}"))?;
        let mut received = 0_u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("下载浏览器中断：{e}"))?
        {
            received += chunk.len() as u64;
            if received > 500_000_000 {
                return Err("浏览器压缩包异常大".into());
            }
            file.write_all(&chunk)
                .map_err(|e| format!("保存浏览器失败：{e}"))?;
        }
        drop(file);
        fs::create_dir_all(&staging).map_err(|e| format!("建立解压目录失败：{e}"))?;
        unpack(&archive, &staging)?;
        if !executable(&staging, platform).is_file() {
            return Err("下载的压缩包中找不到浏览器程序".into());
        }
        if destination.exists() && !executable(&destination, platform).is_file() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("清理未完成的浏览器安装失败：{e}"))?;
        }
        if !destination.exists() {
            fs::rename(&staging, &destination).map_err(|e| format!("安装浏览器失败：{e}"))?;
        }
        let marker = root.join(format!("{VERSION_FILE}-{id}"));
        fs::write(&marker, &release.version).map_err(|e| format!("保存浏览器版本失败：{e}"))?;
        #[cfg(target_os = "windows")]
        if root.join(VERSION_FILE).exists() {
            fs::remove_file(root.join(VERSION_FILE))
                .map_err(|e| format!("更新浏览器版本失败：{e}"))?;
        }
        fs::rename(marker, root.join(VERSION_FILE))
            .map_err(|e| format!("保存浏览器版本失败：{e}"))?;
        Ok(BrowserReady {
            version: release.version,
        })
    }
    .await;
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&staging);
    result
}

fn devtools_endpoint(profile: &Path) -> Option<(SocketAddr, String)> {
    let contents = fs::read_to_string(profile.join(DEVTOOLS_FILE)).ok()?;
    let mut lines = contents.lines();
    let port = lines.next()?.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    let path = lines.next()?;
    let id = path.strip_prefix("/devtools/browser/")?;
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return None;
    }
    Some((
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        path.to_owned(),
    ))
}

fn connect_devtools(profile: &Path) -> Result<WebSocket<TcpStream>, String> {
    let (address, path) =
        devtools_endpoint(profile).ok_or_else(|| "独立浏览器尚未开放调试连接".to_owned())?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(|e| format!("连接独立浏览器失败：{e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("设置浏览器读取超时失败：{e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("设置浏览器写入超时失败：{e}"))?;
    let url = format!("ws://127.0.0.1:{}{path}", address.port());
    tungstenite::client(url, stream)
        .map(|(socket, _)| socket)
        .map_err(|e| format!("连接浏览器调试协议失败：{e}"))
}

fn browser_connection(profile: &Path, binary: &Path) -> Result<WebSocket<TcpStream>, String> {
    if devtools_endpoint(profile).is_some() {
        for _ in 0..10 {
            if let Ok(socket) = connect_devtools(profile) {
                return Ok(socket);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    // 旧版本可能留下已失效的端口文件；启动前清掉，避免连到别的本机进程。
    let _ = fs::remove_file(profile.join(DEVTOOLS_FILE));
    let mut child = Command::new(binary)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0")
        .arg("--enable-unsafe-extension-debugging")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("about:blank")
        .spawn()
        .map_err(|e| format!("启动独立浏览器失败：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut exited_at = None;
    while Instant::now() < deadline {
        if let Ok(socket) = connect_devtools(profile) {
            return Ok(socket);
        }
        if exited_at.is_none() && child.try_wait().ok().flatten().is_some() {
            exited_at = Some(Instant::now());
        }
        if exited_at.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    Err("无法连接独立浏览器；如已打开旧版独立 Chrome，请先关闭它再重试".into())
}

fn cdp_call(
    socket: &mut WebSocket<TcpStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    cdp_call_with_session(socket, id, method, params, None)
}

fn cdp_call_with_session(
    socket: &mut WebSocket<TcpStream>,
    id: u64,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> Result<Value, String> {
    let mut command = serde_json::json!({"id": id, "method": method, "params": params});
    if let Some(session) = session {
        command["sessionId"] = Value::String(session.to_owned());
    }
    socket
        .send(Message::Text(command.to_string().into()))
        .map_err(|e| format!("向浏览器发送命令失败：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() >= deadline {
            return Err(format!("等待浏览器执行 {method} 超时"));
        }
        let message = socket
            .read()
            .map_err(|e| format!("等待浏览器响应失败：{e}"))?;
        let Message::Text(text) = message else {
            continue;
        };
        let response: Value =
            serde_json::from_str(&text).map_err(|e| format!("解析浏览器响应失败：{e}"))?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(format!(
                "浏览器拒绝 {method}：{}",
                error["message"].as_str().unwrap_or("未知错误")
            ));
        }
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("浏览器未返回 {method} 的结果"));
    }
}

/// Chrome 品牌版本从 137 起移除了 --load-extension；先装扩展再开页面。
/// https://developer.chrome.com/blog/extension-news-june-2025
/// 独立配置文件与调试端口都仅用于本应用，不触碰日常 Chrome。
fn ensure_extension_loaded(
    browser: &mut WebSocket<TcpStream>,
    extension: &InstalledExtension,
) -> Result<(), String> {
    let existing = cdp_call(
        browser,
        1,
        "Extensions.getExtensions",
        serde_json::json!({}),
    )?;
    let extensions = existing
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or("浏览器未返回已安装扩展列表")?;
    let path = extension.dir.to_str().ok_or("扩展安装路径包含无效字符")?;
    if let Some(loaded) = extensions
        .iter()
        .find(|loaded| loaded.get("id").and_then(Value::as_str) == Some(&extension.id))
    {
        if loaded.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Err("独立浏览器中的扩展已停用；请先在浏览器扩展管理页启用它".into());
        }
        let same_path = loaded
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|loaded_path| {
                loaded_path == path
                    || matches!(
                        (fs::canonicalize(loaded_path), fs::canonicalize(&extension.dir)),
                        (Ok(actual), Ok(expected)) if actual == expected
                    )
            });
        if same_path {
            // 重复 loadUnpacked 会重载扩展，清空 storage.session 中的结账任务关联。
            return Ok(());
        }
    }
    let loaded = cdp_call(
        browser,
        1,
        "Extensions.loadUnpacked",
        serde_json::json!({"path": path}),
    )?;
    if loaded.get("id").and_then(Value::as_str) != Some(&extension.id) {
        return Err("浏览器加载的扩展 ID 与原 Profile 不一致；已停止本次任务".into());
    }
    Ok(())
}

fn loaded_browser(app: &AppHandle) -> Result<(WebSocket<TcpStream>, InstalledExtension), String> {
    let platform = platform()?;
    let root = root(app)?;
    let (_, binary) =
        current(&root, platform).ok_or_else(|| "请先在 Profile 页准备独立浏览器".to_owned())?;
    let extension = installed_extension(app)?;
    let profile = root.join("profile");
    fs::create_dir_all(&profile).map_err(|e| format!("建立浏览器资料目录失败：{e}"))?;
    let mut browser = browser_connection(&profile, &binary)?;
    ensure_extension_loaded(&mut browser, &extension)?;
    Ok((browser, extension))
}

fn check_checkout_profile(data: &Value, pairing_code: &str) -> Result<(), String> {
    if data.get("apwBridgeToken").and_then(Value::as_str) != Some(pairing_code) {
        return Err(
            "扩展连接码未保存或与桌面应用不匹配；请在 Profile 页打开编辑器，粘贴连接码并保存"
                .into(),
        );
    }
    let Some(profile) = data.get("apwCheckoutProfile") else {
        return Err("尚未保存结账资料；请在 Profile 页打开编辑器填写取货联系人".into());
    };
    if profile.get("enabled").and_then(Value::as_bool) != Some(true) {
        return Err("结账资料自动填充未启用；请在 Profile 编辑器中启用并保存".into());
    }
    let pickup = &profile["pickup"];
    let has = |key: &str| {
        pickup
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    if !((has("familyName") && has("givenName")) || has("fullName"))
        || !has("phone")
        || !has("email")
    {
        return Err("取货联系人缺少姓名、电话或邮箱；请在 Profile 编辑器中补全并保存".into());
    }
    Ok(())
}

fn checkout_storage(
    browser: &mut WebSocket<TcpStream>,
    extension_id: &str,
) -> Result<Value, String> {
    // Extensions.getStorageItems 需要关联到默认浏览器上下文的页面会话。
    // 用临时扩展页读取，确保扩展已完成注册；完成后关闭，不改变 Apple 购物袋。
    let created = cdp_call(
        browser,
        2,
        "Target.createTarget",
        serde_json::json!({
            "url": format!("chrome-extension://{extension_id}/profile.html"),
            "background": true,
        }),
    )?;
    let target_id = created
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or("浏览器未创建预检页面")?;
    let result = (|| {
        let attached = cdp_call(
            browser,
            3,
            "Target.attachToTarget",
            serde_json::json!({"targetId": target_id, "flatten": true}),
        )?;
        let session = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("浏览器未创建预检会话")?;
        let stored = cdp_call_with_session(
            browser,
            4,
            "Extensions.getStorageItems",
            serde_json::json!({
                "id": extension_id,
                "storageArea": "local",
                "keys": ["apwBridgeToken", "apwCheckoutProfile"],
            }),
            Some(session),
        )?;
        stored
            .get("data")
            .cloned()
            .ok_or_else(|| "浏览器未返回扩展资料状态".to_owned())
    })();
    let closed = cdp_call(
        browser,
        5,
        "Target.closeTarget",
        serde_json::json!({"targetId": target_id}),
    );
    let data = result?;
    if closed?.get("success").and_then(Value::as_bool) != Some(true) {
        return Err("浏览器未关闭预检页面".into());
    }
    Ok(data)
}

/// 在库存轮询开始前确认扩展实际已载入，且能继续从购物袋进入结账。
/// 资料只在内存中检查；不写入桌面配置或日志。
pub(crate) fn preflight(app: &AppHandle, pairing_code: &str) -> Result<(), String> {
    let _guard = BROWSER_LAUNCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (mut browser, extension) = loaded_browser(app)?;
    let data = checkout_storage(&mut browser, &extension.id)?;
    check_checkout_profile(&data, pairing_code)
}

fn launch(app: &AppHandle, task_url: Option<&str>) -> Result<String, String> {
    let _guard = BROWSER_LAUNCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (mut browser, extension) = loaded_browser(app)?;
    let url = task_url
        .map(str::to_owned)
        .unwrap_or_else(|| format!("chrome-extension://{}/profile.html", extension.id));
    let created = cdp_call(
        &mut browser,
        2,
        "Target.createTarget",
        serde_json::json!({"url": url}),
    )?;
    created
        .get("targetId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "浏览器未打开购买页面".into())
}

pub(crate) fn open_task(app: &AppHandle, url: &str) -> Result<(), String> {
    prewarm_task(app, url).map(|_| ())
}

pub(crate) fn prewarm_task(app: &AppHandle, url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "购买页面地址无效".to_owned())?;
    if parsed.scheme() != "https"
        || !matches!(
            parsed.host_str(),
            Some("www.apple.com" | "www.apple.com.cn")
        )
    {
        return Err("只能打开 Apple 官方购买页面".into());
    }
    launch(app, Some(url))
}

/// 发售等待页放行后，在原标签页启动加车脚本，保留排队和验证所用的浏览器会话。
/// 页面尚未出现目标 SKU 的官方加车表单时只返回 false，不尝试绕过等待页。
pub(crate) fn activate_release(
    app: &AppHandle,
    target_id: &str,
    part_number: &str,
    task_url: &str,
    refresh_missing: bool,
    cancelled: &AtomicBool,
) -> Result<bool, String> {
    let _guard = BROWSER_LAUNCH
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (mut browser, extension) = loaded_browser(app)?;
    let target = cdp_call(
        &mut browser,
        2,
        "Target.getTargetInfo",
        serde_json::json!({"targetId": target_id}),
    )?;
    let current = target["targetInfo"]["url"]
        .as_str()
        .ok_or("独立浏览器中的发售标签页已关闭")?;
    let current = reqwest::Url::parse(current).map_err(|_| "发售标签页地址无效".to_owned())?;
    let intended = reqwest::Url::parse(task_url).map_err(|_| "任务地址无效".to_owned())?;
    if current.scheme() != "https"
        || current.host_str() != intended.host_str()
        || !current.path().contains("/shop/")
    {
        return Ok(false);
    }
    let attached = cdp_call(
        &mut browser,
        3,
        "Target.attachToTarget",
        serde_json::json!({"targetId": target_id, "flatten": true}),
    )?;
    let session = attached["sessionId"].as_str().ok_or("无法连接发售标签页")?;
    let not_found = current.path().ends_with("/shop/404")
        || target["targetInfo"]["title"]
            .as_str()
            .is_some_and(|title| title.starts_with("Page Not Found"));
    if not_found {
        if refresh_missing && !cancelled.load(Ordering::SeqCst) {
            let mut retry_url = intended.clone();
            retry_url.set_fragment(None);
            cdp_call_with_session(
                &mut browser,
                4,
                "Page.navigate",
                serde_json::json!({"url": retry_url.as_str()}),
                Some(session),
            )?;
        }
        return Ok(false);
    }
    let part = serde_json::to_string(part_number).map_err(|e| e.to_string())?;
    let ready = cdp_call_with_session(
        &mut browser,
        5,
        "Runtime.evaluate",
        serde_json::json!({
            "expression": format!("Array.from(document.querySelectorAll('form[data-part-number]')).some(form => form.getAttribute('data-part-number')?.toUpperCase() === {part} && form.querySelector('[data-autom=\"add-to-cart\"]'))"),
            "returnByValue": true,
        }),
        Some(session),
    )?;
    if ready["result"]["value"].as_bool() != Some(true) {
        return Ok(false);
    }
    if cancelled.load(Ordering::SeqCst) {
        return Ok(false);
    }
    let script = fs::read_to_string(extension.dir.join("content.js"))
        .map_err(|e| format!("读取加车脚本失败：{e}"))?;
    let fragment = intended.fragment().ok_or("任务地址缺少加车标记")?;
    let expression = format!(
        "location.hash = {};\n{}",
        serde_json::to_string(fragment).map_err(|e| e.to_string())?,
        script
    );
    let result = cdp_call_with_session(
        &mut browser,
        6,
        "Runtime.evaluate",
        serde_json::json!({"expression": expression, "returnByValue": true}),
        Some(session),
    )?;
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(format!("启动发售任务脚本失败：{exception}"));
    }
    Ok(true)
}

pub(crate) fn open_profile(app: &AppHandle) -> Result<(), String> {
    launch(app, None).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_preflight_requires_paired_and_complete_pickup_profile() {
        let code = "a".repeat(32);
        let valid = serde_json::json!({
            "apwBridgeToken": code,
            "apwCheckoutProfile": {
                "enabled": true,
                "pickup": {"familyName": "测", "givenName": "试", "phone": "13800000000", "email": "test@example.com"}
            }
        });
        assert!(check_checkout_profile(&valid, &code).is_ok());

        let mut unpaired = valid.clone();
        unpaired["apwBridgeToken"] = Value::String("b".repeat(32));
        assert!(
            check_checkout_profile(&unpaired, &code)
                .unwrap_err()
                .contains("连接码")
        );

        let mut disabled = valid.clone();
        disabled["apwCheckoutProfile"]["enabled"] = Value::Bool(false);
        assert!(
            check_checkout_profile(&disabled, &code)
                .unwrap_err()
                .contains("未启用")
        );

        let mut incomplete = valid;
        incomplete["apwCheckoutProfile"]["pickup"]["email"] = Value::String(" ".into());
        assert!(
            check_checkout_profile(&incomplete, &code)
                .unwrap_err()
                .contains("邮箱")
        );
    }

    #[cfg(unix)]
    #[test]
    fn derives_chromium_unpacked_extension_id_from_its_launch_path() {
        assert_eq!(
            extension_id(Path::new("/tmp/apw-browser-extension")),
            "kicnhjehhdlophpgfhejebhjgmpekapm"
        );
    }

    #[test]
    fn moving_and_updating_the_app_keeps_the_old_extension_storage_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("browser-bot");
        let old_bundle = temp.path().join("old-app/Resources/browser-extension");
        let new_bundle = temp.path().join("new-app/Resources/browser-extension");
        fs::create_dir_all(&new_bundle).unwrap();
        fs::write(
            new_bundle.join("manifest.json"),
            r#"{"manifest_version":3,"name":"Apple Pickup Watcher","version":"1"}"#,
        )
        .unwrap();
        fs::write(new_bundle.join("background.js"), "self.initial = true;").unwrap();
        let original_id = extension_id(&old_bundle);
        let preferences = root.join("profile/Default/Secure Preferences");
        fs::create_dir_all(preferences.parent().unwrap()).unwrap();
        fs::write(
            &preferences,
            serde_json::to_vec(&serde_json::json!({
                "extensions": {"settings": {
                    (original_id.clone()): {"location": 8, "path": old_bundle.to_str().unwrap()}
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        let storage = root
            .join("profile/Default/Local Extension Settings")
            .join(&original_id);
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("sentinel"), "saved profile").unwrap();

        let first = install_extension(&root, &new_bundle).unwrap();
        assert_eq!(first.id, original_id);
        assert_ne!(first.dir, old_bundle);
        let manifest: Value =
            serde_json::from_slice(&fs::read(first.dir.join("manifest.json")).unwrap()).unwrap();
        let key = base64::engine::general_purpose::STANDARD
            .decode(manifest["key"].as_str().unwrap())
            .unwrap();
        assert_eq!(key, extension_path_bytes(&old_bundle));
        assert_eq!(
            fs::read_to_string(storage.join("sentinel")).unwrap(),
            "saved profile"
        );

        fs::write(new_bundle.join("background.js"), "self.updated = true;").unwrap();
        let second = install_extension(&root, &new_bundle).unwrap();
        assert_eq!(second.id, original_id);
        assert_ne!(second.dir, first.dir);
        assert!(
            first.dir.exists(),
            "running Chrome can finish using the old copy"
        );
        assert_eq!(
            fs::read_to_string(storage.join("sentinel")).unwrap(),
            "saved profile"
        );
    }

    #[test]
    fn ambiguous_legacy_ids_stop_before_creating_a_new_profile_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("browser-bot");
        let bundled = temp.path().join("current/browser-extension");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(
            bundled.join("manifest.json"),
            r#"{"manifest_version":3,"name":"Apple Pickup Watcher","version":"1"}"#,
        )
        .unwrap();
        let old_a = temp.path().join("old-a/browser-extension");
        let old_b = temp.path().join("old-b/browser-extension");
        let preferences = root.join("profile/Default/Secure Preferences");
        fs::create_dir_all(preferences.parent().unwrap()).unwrap();
        fs::write(
            &preferences,
            serde_json::to_vec(&serde_json::json!({
                "extensions": {"settings": {
                    (extension_id(&old_a)): {"location": 8, "path": old_a.to_str().unwrap()},
                    (extension_id(&old_b)): {"location": 8, "path": old_b.to_str().unwrap()}
                }}
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            install_extension(&root, &bundled)
                .unwrap_err()
                .contains("多个旧扩展身份")
        );
        assert!(!root.join("extension-key").exists());
        assert!(preferences.exists());
    }

    #[test]
    fn orphaned_extension_storage_does_not_become_an_empty_profile() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("browser-bot");
        let bundled = temp.path().join("new/browser-extension");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(
            bundled.join("manifest.json"),
            r#"{"manifest_version":3,"name":"Apple Pickup Watcher","version":"1"}"#,
        )
        .unwrap();
        let old_id = extension_id(&temp.path().join("missing/browser-extension"));
        let storage = root
            .join("profile/Default/Local Extension Settings")
            .join(old_id);
        fs::create_dir_all(&storage).unwrap();
        fs::write(storage.join("sentinel"), "saved profile").unwrap();

        assert!(
            install_extension(&root, &bundled)
                .unwrap_err()
                .contains("无法确认原扩展身份")
        );
        assert!(!root.join("extension-key").exists());
        assert_eq!(
            fs::read_to_string(storage.join("sentinel")).unwrap(),
            "saved profile"
        );
    }

    #[test]
    #[ignore = "requires APW_CHROME_FOR_TESTING and starts a disposable headless browser"]
    fn chrome_devtools_loads_the_real_profile_page() {
        struct Browser(std::process::Child);
        impl Drop for Browser {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let binary = std::env::var_os("APW_CHROME_FOR_TESTING")
            .map(PathBuf::from)
            .expect("set APW_CHROME_FOR_TESTING to the Chrome for Testing executable");
        let temp = tempfile::tempdir().unwrap();
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../browser-extension");
        let installed = install_extension(&temp.path().join("bot"), &bundled).unwrap();
        let profile = temp.path().join("profile");
        let browser = Browser(
            Command::new(binary)
                .arg("--headless=new")
                .arg("--remote-debugging-address=127.0.0.1")
                .arg("--remote-debugging-port=0")
                .arg("--enable-unsafe-extension-debugging")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg("about:blank")
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut socket = loop {
            if let Ok(socket) = connect_devtools(&profile) {
                break socket;
            }
            assert!(Instant::now() < deadline, "Chrome did not open DevTools");
            thread::sleep(Duration::from_millis(100));
        };
        ensure_extension_loaded(&mut socket, &installed).unwrap();
        let extensions = cdp_call(
            &mut socket,
            1,
            "Extensions.getExtensions",
            serde_json::json!({}),
        )
        .unwrap();
        assert!(
            extensions["extensions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|extension| {
                    extension["id"] == installed.id
                        && fs::canonicalize(extension["path"].as_str().unwrap()).unwrap()
                            == fs::canonicalize(&installed.dir).unwrap()
                        && extension["enabled"] == true
                }),
            "{extensions:?}"
        );
        assert!(
            checkout_storage(&mut socket, &installed.id)
                .unwrap()
                .get("apwCheckoutProfile")
                .is_none()
        );
        let created = cdp_call(
            &mut socket,
            2,
            "Target.createTarget",
            serde_json::json!({"url": format!("chrome-extension://{}/profile.html", installed.id)}),
        )
        .unwrap();
        let attached = cdp_call(
            &mut socket,
            3,
            "Target.attachToTarget",
            serde_json::json!({"targetId": created["targetId"], "flatten": true}),
        )
        .unwrap();
        let session = attached["sessionId"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut command_id = 4;
        loop {
            let result = cdp_call_with_session(
                &mut socket,
                command_id,
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": "Boolean(document.getElementById('profile-form'))",
                    "returnByValue": true,
                }),
                Some(session),
            );
            if result
                .ok()
                .and_then(|value| value.pointer("/result/value").cloned())
                == Some(Value::Bool(true))
            {
                break;
            }
            assert!(Instant::now() < deadline, "Profile page did not load");
            command_id += 1;
            thread::sleep(Duration::from_millis(100));
        }
        command_id += 1;
        let empty = cdp_call_with_session(
            &mut socket,
            command_id,
            "Extensions.getStorageItems",
            serde_json::json!({
                "id": installed.id,
                "storageArea": "local",
                "keys": ["apwBridgeToken", "apwCheckoutProfile"],
            }),
            Some(session),
        )
        .unwrap();
        assert!(empty["data"].get("apwCheckoutProfile").is_none());
        command_id += 1;
        let code = "a".repeat(32);
        cdp_call_with_session(
            &mut socket,
            command_id,
            "Extensions.setStorageItems",
            serde_json::json!({
                "id": installed.id,
                "storageArea": "local",
                "values": {
                    "apwBridgeToken": code,
                    "apwCheckoutProfile": {
                        "enabled": true,
                        "pickup": {"fullName": "测试", "phone": "13800000000", "email": "test@example.com"}
                    }
                },
            }),
            Some(session),
        )
        .unwrap();
        command_id += 1;
        let stored = cdp_call_with_session(
            &mut socket,
            command_id,
            "Extensions.getStorageItems",
            serde_json::json!({
                "id": installed.id,
                "storageArea": "local",
                "keys": ["apwBridgeToken", "apwCheckoutProfile"],
            }),
            Some(session),
        )
        .unwrap();
        check_checkout_profile(&stored["data"], &code).unwrap();
        check_checkout_profile(
            &checkout_storage(&mut socket, &installed.id).unwrap(),
            &code,
        )
        .unwrap();
        command_id += 1;
        cdp_call_with_session(
            &mut socket,
            command_id,
            "Extensions.setStorageItems",
            serde_json::json!({
                "id": installed.id,
                "storageArea": "session",
                "values": {"apwCheckoutFlow-test": {"phase": "checkout"}},
            }),
            Some(session),
        )
        .unwrap();
        ensure_extension_loaded(&mut socket, &installed).unwrap();
        command_id += 1;
        let flow = cdp_call_with_session(
            &mut socket,
            command_id,
            "Extensions.getStorageItems",
            serde_json::json!({
                "id": installed.id,
                "storageArea": "session",
                "keys": ["apwCheckoutFlow-test"],
            }),
            Some(session),
        )
        .unwrap();
        assert_eq!(flow["data"]["apwCheckoutFlow-test"]["phase"], "checkout");
        drop(socket);
        drop(browser);
    }

    #[test]
    fn only_official_chrome_archive_is_accepted() {
        assert!(trusted_download(
            "https://storage.googleapis.com/chrome-for-testing-public/153.0.1234.0/mac-arm64/chrome-mac-arm64.zip",
            "153.0.1234.0",
            "mac-arm64"
        ));
        assert!(!trusted_download(
            "https://evil.example/chrome-for-testing-public/153.0.1234.0/mac-arm64/chrome-mac-arm64.zip",
            "153.0.1234.0",
            "mac-arm64"
        ));
        assert!(!valid_version("../profile"));
        assert!(!valid_version("155..8059.79"));
        assert!(!trusted_download(
            "https://storage.googleapis.com/chrome-for-testing-public/153.0.1234.0/mac-arm64/chrome-mac-arm64.zip?redirect=1",
            "153.0.1234.0",
            "mac-arm64"
        ));
    }

    #[test]
    fn reads_official_stable_channel_shape() {
        let releases: Releases = serde_json::from_str(r#"{"channels":{"Stable":{"version":"155.0.8059.79","downloads":{"chrome":[{"platform":"mac-arm64","url":"https://storage.googleapis.com/chrome-for-testing-public/155.0.8059.79/mac-arm64/chrome-mac-arm64.zip"}]}}}}"#).unwrap();
        assert_eq!(releases.channels.stable.version, "155.0.8059.79");
    }
}
