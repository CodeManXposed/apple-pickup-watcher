//! 为自动购买任务使用独立的 Chrome for Testing 配置文件。
//! 只启动官方浏览器和仓库自带的扩展；Apple 页面上的动作仍由扩展执行。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager};

const RELEASES_URL: &str = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
const VERSION_FILE: &str = "current-version";

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

/// Chromium 对无 manifest key 的解压扩展以路径的 SHA-256 前 16 字节生成 ID。
/// 用启动参数里的同一条绝对路径，才能直接打开这份扩展自己的资料页面。
fn extension_id(path: &Path) -> String {
    #[cfg(windows)]
    let bytes = {
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
    };
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    let hash = Sha256::digest(bytes);
    let mut id = String::with_capacity(32);
    for &byte in &hash[..16] {
        id.push(char::from(b'a' + (byte >> 4)));
        id.push(char::from(b'a' + (byte & 0x0f)));
    }
    id
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

pub(crate) fn ready(app: &AppHandle) -> Result<BrowserReady, String> {
    let platform = platform()?;
    let root = root(app)?;
    current(&root, platform)
        .map(|(version, _)| BrowserReady { version })
        .ok_or_else(|| "请先在 Profile 页准备独立浏览器".into())
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

/// 在独立配置文件里打开一个购买任务。启动参数不会触碰用户日常 Chrome 的资料。
fn launch(app: &AppHandle, url: &str) -> Result<(), String> {
    let platform = platform()?;
    let root = root(app)?;
    let (_, binary) =
        current(&root, platform).ok_or_else(|| "请先在 Profile 页准备独立浏览器".to_owned())?;
    let extension = extension_dir(app)?;
    let profile = root.join("profile");
    fs::create_dir_all(&profile).map_err(|e| format!("建立浏览器资料目录失败：{e}"))?;
    Command::new(binary)
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!(
            "--disable-extensions-except={}",
            extension.display()
        ))
        .arg(format!("--load-extension={}", extension.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(url)
        .spawn()
        .map_err(|e| format!("启动独立浏览器失败：{e}"))?;
    Ok(())
}

pub(crate) fn open_task(app: &AppHandle, url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "购买页面地址无效".to_owned())?;
    if parsed.scheme() != "https"
        || !matches!(
            parsed.host_str(),
            Some("www.apple.com" | "www.apple.com.cn")
        )
    {
        return Err("只能打开 Apple 官方购买页面".into());
    }
    launch(app, url)
}

pub(crate) fn open_profile(app: &AppHandle) -> Result<(), String> {
    let id = extension_id(&extension_dir(app)?);
    launch(app, &format!("chrome-extension://{id}/profile.html"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn derives_chromium_unpacked_extension_id_from_its_launch_path() {
        assert_eq!(
            extension_id(Path::new("/tmp/apw-browser-extension")),
            "kicnhjehhdlophpgfhejebhjgmpekapm"
        );
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
