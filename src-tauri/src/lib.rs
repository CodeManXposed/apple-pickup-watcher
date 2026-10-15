//! Tauri 应用外壳。
//!
//! 这一层只做装配和转译：起引擎、把命令转成引擎消息、把引擎事件转发给前端、
//! 在有货时发提醒。**所有业务判断都在 `apw-core` 里**，这里不许出现任何
//! 「什么算有货」之类的逻辑 —— 一旦让界面层参与判断，那条核心不变量就多了
//! 一处可以被绕开的地方。
//!
//! 提醒也刻意放在这一层而不是前端：托盘模式下窗口是隐藏的，WebView 可能被
//! 系统节流甚至挂起，把「及时提醒」挂在一个会被挂起的执行环境上是不能接受的。

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use apw_core::apple::{AppleClient, ClientConfig};
use apw_core::catalog::Catalog;
use apw_core::config::{AddMethod, MIN_INTERVAL_SECONDS, Settings, SettingsStore};
use apw_core::model::{Category, Product, REGIONS, Store, Target, region_by_locale};
use apw_core::notify::{Bark, Discord, Feishu, Multi, Notification, Notifier, Sound};
use apw_core::watcher::{Event, TargetState, Watcher, WatcherConfig};
use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Runtime, WindowEvent};
use tauri_plugin_updater::UpdaterExt;

mod auto_open;
mod browser_bot;
mod direct_cart;
mod notification_queue;
mod payment_bridge;
mod release_task;

use auto_open::AutoOpenBag;
use payment_bridge::{PaymentBridge, PaymentLink};

fn browser_task_url(
    base: &str,
    task_id: &str,
    product_name: &str,
    checkout_area: Option<(&str, &str, &str)>,
) -> Result<reqwest::Url, String> {
    let mut task_url = reqwest::Url::parse(base).map_err(|err| err.to_string())?;
    let mut pairs = vec![("apw-task", task_id), ("apw-product", product_name)];
    if let Some((province, city, district)) = checkout_area {
        pairs.extend([
            ("apw-province", province),
            ("apw-city", city),
            ("apw-district", district),
        ]);
    }
    let encoded = reqwest::Url::parse_with_params("https://localhost/", &pairs)
        .map_err(|err| err.to_string())?;
    let fragment = format!(
        "{}&{}",
        task_url.fragment().unwrap_or_default(),
        encoded.query().unwrap_or_default()
    );
    task_url.set_fragment(Some(&fragment));
    Ok(task_url)
}

#[cfg(test)]
mod browser_task_url_tests {
    use super::browser_task_url;

    #[test]
    fn product_and_area_survive_fragment_parameter_encoding() {
        let task = browser_task_url(
            "https://www.apple.com.cn/shop/pdpAddToBag/MJYE4CH/A#apw-auto-add=MJYE4CH%2FA&apw-store=R683",
            &"a".repeat(32),
            "iPhone 18 Pro 256GB 黑色 & 蓝色 + 50%",
            Some(("上海", "上海", "浦东新区 & 附近")),
        )
        .unwrap();
        let fragment = task.fragment().unwrap();
        let decoded = reqwest::Url::parse(&format!("https://localhost/?{fragment}")).unwrap();
        let pairs: std::collections::HashMap<_, _> = decoded.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("apw-auto-add").unwrap(), "MJYE4CH/A");
        assert_eq!(pairs.get("apw-store").unwrap(), "R683");
        assert_eq!(pairs.get("apw-task").unwrap(), &"a".repeat(32));
        assert_eq!(
            pairs.get("apw-product").unwrap(),
            "iPhone 18 Pro 256GB 黑色 & 蓝色 + 50%"
        );
        assert_eq!(pairs.get("apw-district").unwrap(), "浦东新区 & 附近");
    }
}

/// 前端事件通道名。前端用 `listen("watcher://event", ...)` 订阅。
const EVENT_CHANNEL: &str = "watcher://event";
/// 启动过程中的降级说明通道：配置读不出来之类的事必须让用户看见。
const NOTICE_CHANNEL: &str = "watcher://notice";

/// 地区的可序列化形式。
///
/// `model::Region` 的字段都是 `&'static str`，而且界面不需要知道 `base_url`
/// 这类内部细节。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegionDto {
    title: &'static str,
    locale: &'static str,
}

/// 品类的可序列化形式。
///
/// 界面上的品类下拉框由这里驱动，而不是在前端另抄一份常量：抄一份就迟早会有
/// 一边先加了品类、另一边还蒙在鼓里。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoryDto {
    value: Category,
    title: &'static str,
}

struct AppState {
    watcher: Watcher,
    /// 和引擎共用的 Apple 客户端句柄，改代理设置时用它换线路，不用重启。
    client: AppleClient,
    catalog: Catalog,
    http: reqwest::Client,
    browser_install: tokio::sync::Mutex<()>,
    /// 设置的内存副本。写盘失败不该让界面卡住，所以内存副本是权威的展示来源。
    settings: RwLock<Settings>,
    /// 为 `None` 表示配置不可持久化（目录不可写，或上次读取失败已放弃写盘）。
    store: Option<SettingsStore>,
}

impl AppState {
    fn settings_snapshot(&self) -> Settings {
        self.settings
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 更新内存副本并尝试落盘。落盘失败只报错，不回滚内存 ——
    /// 用户的操作已经生效了，没道理因为磁盘问题把界面弹回去。
    fn put_settings(&self, next: Settings) -> Result<(), String> {
        let mut guard = self.settings.write().unwrap_or_else(|e| e.into_inner());
        *guard = next;
        let to_save = guard.clone();
        drop(guard);

        match &self.store {
            Some(store) => store.save(&to_save).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }
}

#[tauri::command]
fn list_regions() -> Vec<RegionDto> {
    REGIONS
        .iter()
        .map(|r| RegionDto {
            title: r.title,
            locale: r.locale,
        })
        .collect()
}

#[tauri::command]
fn list_categories() -> Vec<CategoryDto> {
    Category::ALL
        .iter()
        .map(|c| CategoryDto {
            value: *c,
            title: c.title(),
        })
        .collect()
}

#[tauri::command]
fn list_stores(state: tauri::State<'_, AppState>, locale: String) -> Result<Vec<Store>, String> {
    state.catalog.stores(&locale).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_products(
    state: tauri::State<'_, AppState>,
    locale: String,
) -> Result<Vec<Product>, String> {
    state.catalog.products(&locale).map_err(|e| e.to_string())
}

/// 从 Apple 官网抓最新型号，替换该地区该品类的内存副本，返回抓到的型号数。
///
/// `category` 为 `None` 时抓该地区的全部购买页。界面传的是当前选中的品类：
/// 一次只抓那几页，用户想看新出的 Mac 不必等 iPhone、iPad、Watch 一起抓完。
#[tauri::command]
async fn refresh_products(
    state: tauri::State<'_, AppState>,
    locale: String,
    category: Option<Category>,
) -> Result<usize, String> {
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    state
        .catalog
        .refresh_products(region, category, &state.http)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings_snapshot()
}

#[tauri::command]
fn get_bridge_pairing_code(bridge: tauri::State<'_, PaymentBridge>) -> Result<String, String> {
    bridge.pairing_code().map(str::to_string)
}

#[tauri::command]
fn get_payment_link(bridge: tauri::State<'_, PaymentBridge>) -> Option<PaymentLink> {
    bridge.latest()
}

#[tauri::command]
fn get_bot_status(bridge: tauri::State<'_, PaymentBridge>) -> Option<payment_bridge::BotStatus> {
    bridge.status()
}

#[tauri::command]
fn open_extension_folder(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    let path = browser_bot::installed_extension_dir(&app)?;
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn prepare_bot_browser(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<browser_bot::BrowserReady, String> {
    let _install = state.browser_install.lock().await;
    browser_bot::prepare(&app, &state.http).await
}

#[tauri::command]
async fn open_bot_browser(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || browser_bot::open_profile(&app))
        .await
        .map_err(|e| format!("启动浏览器任务中断：{e}"))?
}

#[tauri::command]
async fn save_settings(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut next = settings;
    next.normalize();
    state.catalog.attach_companions(&mut next.targets);
    state.catalog.attach_locations(&mut next.targets);
    validate_api_task_urls(&next)?;
    let previous = state.settings_snapshot();

    // 无论监控是否已运行，开启自动加车前都做预检。
    // 预检放在任何引擎或配置改动之前，失败时保持旧设置。
    if next.auto_add_to_bag && !previous.auto_add_to_bag {
        preflight_checkout(app.clone()).await?;
    }
    let auto_add_disabled = previous.auto_add_to_bag && !next.auto_add_to_bag;
    if auto_add_disabled {
        pause_checkout_task(&app);
    } else {
        pause_removed_target(&app, &next.targets);
    }

    // 设置里的目标列表和查询间隔要同步给引擎，否则改完设置监控还按旧的跑。
    state.watcher.set_targets(next.targets.clone()).await;
    state.watcher.set_interval(next.interval()).await;
    // 代理变了就换线路，立即生效；地址解析不了就把错误报回界面，其余设置照存。
    if next.proxies != previous.proxies {
        state
            .client
            .set_proxies(&next.proxies)
            .await
            .map_err(|e| format!("代理设置未生效：{e}"))?;
    }

    state.put_settings(next.clone())?;
    Ok(next)
}

fn pause_checkout_task<R: Runtime>(app: &AppHandle<R>) {
    match app.state::<PaymentBridge>().pause_active_task() {
        Ok(Some(status)) => {
            let _ = app.emit(payment_bridge::STATUS_CHANNEL, status);
        }
        Ok(None) => {}
        Err(err) => {
            let _ = app.emit(NOTICE_CHANNEL, format!("暂停自动结账任务失败：{err}"));
        }
    }
}

fn pause_removed_target<R: Runtime>(app: &AppHandle<R>, targets: &[Target]) {
    let removed = app.state::<PaymentBridge>().status().is_some_and(|status| {
        status.automation_active
            && !targets.iter().any(|target| {
                target.part_number == status.part_number
                    && target.store_number == status.store_number
            })
    });
    if removed {
        pause_checkout_task(app);
        let _ = app.emit(
            NOTICE_CHANNEL,
            "进行中的商品目标已移除，自动结账任务已暂停。",
        );
    }
}

fn validate_api_task_urls(settings: &Settings) -> Result<(), String> {
    for target in &settings.targets {
        let Some(plan) = release_task::plan_for(settings, target) else {
            continue;
        };
        if plan.add_method != AddMethod::Api {
            continue;
        }
        let region = region_by_locale(&target.locale)
            .ok_or_else(|| format!("API 任务 {} 的地区无效", target.part_number))?;
        direct_cart::product_url(region, target, Some(plan))
            .map_err(|err| format!("API 任务 {}：{err}", target.part_number))?;
    }
    Ok(())
}

#[cfg(test)]
mod api_task_url_tests {
    use super::validate_api_task_urls;
    use apw_core::config::{AddMethod, Settings, TaskMode, TaskPlan};
    use apw_core::model::Target;

    #[test]
    fn rejects_a_wrong_sku_before_saving_an_api_task() {
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let plan = TaskPlan {
            target_key: target.key().0,
            mode: TaskMode::Restock,
            add_method: AddMethod::Api,
            product_url: Some(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/OTHERCH/A".into(),
            ),
            release_at_ms: None,
            retry_delay_ms: 3_000,
        };
        let mut settings = Settings {
            targets: vec![target],
            task_plans: vec![plan],
            ..Settings::default()
        };
        assert!(
            validate_api_task_urls(&settings)
                .unwrap_err()
                .contains("SKU")
        );
        settings.task_plans[0].product_url =
            Some("https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro".into());
        assert!(validate_api_task_urls(&settings).is_ok());
    }
}

#[cfg(test)]
mod task_removal_tests {
    use super::{PaymentBridge, pause_removed_target};
    use apw_core::model::Target;
    use tauri::Manager;

    #[test]
    fn removing_the_active_target_revokes_its_checkout_task() {
        let app = tauri::test::mock_app();
        app.manage(PaymentBridge::disabled());
        let bridge = app.state::<PaymentBridge>();
        let active = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let mut other = active.clone();
        other.part_number = "OTHERCH/A".into();
        let task = bridge
            .start_task(&active.part_number, &active.store_number)
            .unwrap();
        pause_removed_target(app.handle(), &[other.clone(), active]);
        assert!(bridge.is_live_cart_task_id(&task.task_id));

        pause_removed_target(app.handle(), &[other]);
        assert!(!bridge.is_live_cart_task_id(&task.task_id));
        assert!(!bridge.status().unwrap().automation_active);
    }
}

/// 用界面上的推送地址列表整体替换现有的。
///
/// 每个地址是 Bark 还是飞书由 [`Settings::set_push_urls`] 按长相判断，界面不自己猜。
/// 只动推送地址：不经过 [`save_settings`]，免得顺带把引擎的目标、间隔和代理
/// 也重新下发一遍。
#[tauri::command]
async fn set_push_urls(
    state: tauri::State<'_, AppState>,
    urls: Vec<String>,
) -> Result<Settings, String> {
    let mut next = state.settings_snapshot();
    next.set_push_urls(&urls);
    next.normalize();
    state.put_settings(next.clone())?;
    Ok(next)
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<Vec<TargetState>, String> {
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_targets(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    targets: Vec<Target>,
) -> Result<Vec<TargetState>, String> {
    let mut targets = targets;
    state.catalog.attach_companions(&mut targets);
    state.catalog.attach_locations(&mut targets);
    pause_removed_target(&app, &targets);
    state.watcher.set_targets(targets.clone()).await;
    let mut next = state.settings_snapshot();
    next.targets = targets;
    next.normalize();
    state.put_settings(next)?;
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_interval(state: tauri::State<'_, AppState>, seconds: u64) -> Result<u64, String> {
    let secs = seconds.max(MIN_INTERVAL_SECONDS);
    state.watcher.set_interval(Duration::from_secs(secs)).await;
    let mut next = state.settings_snapshot();
    next.interval_seconds = secs;
    state.put_settings(next)?;
    Ok(secs)
}

#[tauri::command]
async fn start_watching(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let settings = state.settings_snapshot();
    if settings.auto_add_to_bag {
        validate_api_task_urls(&settings)?;
        preflight_checkout(app).await?;
    }
    state.watcher.start().await;
    Ok(())
}

async fn preflight_checkout(app: AppHandle) -> Result<(), String> {
    let pairing_code = app.state::<PaymentBridge>().pairing_code()?.to_owned();
    tauri::async_runtime::spawn_blocking(move || browser_bot::preflight(&app, &pairing_code))
        .await
        .map_err(|e| format!("检查自动结账环境中断：{e}"))?
}

#[tauri::command]
async fn stop_watching(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    pause_checkout_task(&app);
    state.watcher.stop().await;
    Ok(())
}

#[tauri::command]
async fn is_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.watcher.is_running().await)
}

/// 一个待安装的更新。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

/// 查询有没有新版本。返回 `None` 表示已经是最新。
///
/// **刻意不做静默自动安装**：给用户装东西这件事应该由用户点头。何况这是个会在
/// 抢购当口挂着的程序，自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        // 检查更新失败不是错误状态，只是这次没查到 —— 网络不通、GitHub 抽风都
        // 会走到这里，没必要弹给用户看，写进日志即可。
        Err(err) => Err(err.to_string()),
    }
}

/// 下载并安装更新。安装完成后需要重启应用才生效。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "已经是最新版本".to_string())?;

    let handle = app.clone();
    update
        .download_and_install(
            move |downloaded, total| {
                // 进度只发给界面，不做任何决策。
                let _ = handle.emit(
                    "watcher://update-progress",
                    serde_json::json!({ "downloaded": downloaded, "total": total }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 手动试一次提醒，让用户在真正抢购之前确认铃声和推送都通。
#[tauri::command]
async fn test_notify(app: AppHandle) -> Result<(), String> {
    dispatch_notification(
        &app,
        // 标题刻意和真实到货提醒一样带「有货了」：飞书自定义机器人的关键词
        // 安全模式只放行含关键词的消息，测试文案若不含，用户会误以为配置坏了。
        Notification::new("有货了（测试）", "如果你看到并听到了这条，说明提醒是通的"),
    )
    .await
    .map_err(|e| e.to_string())
}

/// 发出一条提醒：系统通知、提示音和配置的远程渠道。
async fn dispatch_notification(
    app: &AppHandle,
    notification: Notification,
) -> Result<(), apw_core::notify::NotifyError> {
    let settings = match app.try_state::<AppState>() {
        Some(state) => state.settings_snapshot(),
        None => return Ok(()),
    };

    // 系统通知总是发。它是最轻量也最可靠的一条，没有理由让用户关掉它之后
    // 就完全收不到东西。
    emit_system_notification(app, &notification);

    let mut channels = Multi::new();
    if settings.sound_enabled {
        channels.push(Sound::embedded());
    }
    if !settings.bark_url.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // Bark 每次现构造：地址是用户随时可改的设置项，缓存实例会在改完地址后
        // 继续往旧地址推。共享的 http 客户端一并传进去，连接池仍然复用。
        // 用户可以填多个地址（分号分隔），每个地址一条渠道，并发推送、各自报错。
        for bark in Bark::from_list(&settings.bark_url, http) {
            channels.push(bark);
        }
    }
    if !settings.feishu_webhook.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // 飞书与 Bark 同样的构造规则：地址随时可改、支持分号分隔多个、
        // 共享连接池，渠道各自报错。
        for feishu in Feishu::from_list(&settings.feishu_webhook, http) {
            channels.push(feishu);
        }
    }
    if !settings.discord_webhook.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        for discord in Discord::from_list(&settings.discord_webhook, http) {
            channels.push(discord);
        }
    }
    if channels.is_empty() {
        return Ok(());
    }
    channels.notify(&notification).await
}

fn emit_system_notification(app: &AppHandle, n: &Notification) {
    use tauri_plugin_notification::NotificationExt;
    // 通知发不出去（用户在系统里关了权限）不该影响其他渠道，也不该让流程中断。
    if let Err(err) = app
        .notification()
        .builder()
        .title(&n.title)
        .body(&n.body)
        .show()
    {
        let _ = app.emit(NOTICE_CHANNEL, format!("系统通知发送失败：{err}"));
    }
}

async fn open_task_browser(app: &AppHandle, url: reqwest::Url) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || browser_bot::open_task(&app, url.as_str()))
        .await
        .map_err(|err| format!("浏览器任务中断：{err}"))?
}

async fn open_direct_bag_browser(
    app: &AppHandle,
    url: String,
    cookies: Vec<browser_bot::SessionCookie>,
) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser_bot::open_direct_bag(&app, &url, &cookies, None)
    })
    .await
    .map_err(|err| format!("浏览器购物袋交接中断：{err}"))?
}

async fn open_direct_product_browser(
    app: &AppHandle,
    url: String,
    cookies: Vec<browser_bot::SessionCookie>,
) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        browser_bot::open_direct_product(&app, &url, &cookies, None)
    })
    .await
    .map_err(|err| format!("浏览器商品页交接中断：{err}"))?
}

async fn open_auto_add(
    app: &AppHandle,
    gate: &mut AutoOpenBag,
    target: &Target,
) -> Result<(), String> {
    let settings = app.state::<AppState>().settings_snapshot();
    let plan = release_task::plan_for(&settings, target).cloned();
    let add_method = plan
        .as_ref()
        .map_or(AddMethod::Browser, |plan| plan.add_method);
    let checkout_area = app
        .state::<AppState>()
        .catalog
        .store_by_number(&target.locale, &target.store_number)
        .and_then(|store| {
            store
                .checkout_area(&target.locale)
                .map(|(province, city, district)| {
                    (province.to_string(), city.to_string(), district.to_string())
                })
        });
    gate.open_if_needed_async(
        true,
        &target.locale,
        Some((target.part_number.as_str(), target.store_number.as_str())),
        |url| async move {
            let bridge = app.state::<PaymentBridge>();
            let status = bridge.start_task(&target.part_number, &target.store_number)?;
            let task_id = status.task_id.clone();
            let _ = app.emit(payment_bridge::EVENT_CHANNEL, Option::<PaymentLink>::None);
            let _ = app.emit(payment_bridge::STATUS_CHANNEL, status);
            let area = checkout_area.as_ref().map(|(province, city, district)| {
                (province.as_str(), city.as_str(), district.as_str())
            });
            let mut api_add_confirmed = false;
            let mut api_stop_retries = false;
            let result = match add_method {
                AddMethod::Browser => match browser_task_url(
                    &url,
                    &task_id,
                    &target.product_name,
                    area,
                ) {
                    Ok(task_url) => open_task_browser(app, task_url).await,
                    Err(err) => Err(err),
                },
                AddMethod::Api => {
                    async {
                        let region = region_by_locale(&target.locale).ok_or("任务地区无效")?;
                        let session = direct_cart::DirectCart::new(region, target, plan.as_ref())?;
                        let host = reqwest::Url::parse(region.base_url)
                            .map_err(|_| "地区官网 URL 无效")?
                            .host_str()
                            .ok_or("地区官网缺少域名")?
                            .to_owned();
                        let app_copy = app.clone();
                        match tauri::async_runtime::spawn_blocking(move || {
                            browser_bot::apple_cookies(&app_copy, &host)
                        })
                        .await
                        {
                            Ok(Ok(cookies)) => session.seed_browser_cookies(&cookies),
                            Ok(Err(err)) => {
                                let _ = app.emit(NOTICE_CHANNEL, format!("读取独立浏览器 Apple 会话失败：{err}；本轮使用新 HTTP 会话"));
                            }
                            Err(err) => {
                                let _ = app.emit(NOTICE_CHANNEL, format!("读取独立浏览器 Apple 会话中断：{err}；本轮使用新 HTTP 会话"));
                            }
                        }
                        let outcome = session
                            .add(|| bridge.is_live_cart_task_id(&task_id))
                            .await?;
                        if !bridge.is_live_cart_task_id(&task_id) {
                            api_stop_retries = true;
                            return Err(match outcome {
                                direct_cart::AddOutcome::Confirmed(_) => "Apple 已确认直连加车，但任务已停止；请在独立浏览器核对购物袋。本轮不会自动结账。".to_owned(),
                                direct_cart::AddOutcome::Uncertain { reason, .. } => format!("{reason}；任务已停止，请核对购物袋。本轮不会再次加车或自动结账。"),
                                _ => "任务已停止；本轮不会继续直连加车或自动结账。".to_owned(),
                            });
                        }
                        let cookies = match outcome {
                            direct_cart::AddOutcome::Confirmed(cookies) => cookies,
                            direct_cart::AddOutcome::Cancelled => {
                                api_stop_retries = true;
                                return Err("任务已停止；本轮不会继续直连加车或自动结账。".into());
                            }
                            direct_cart::AddOutcome::Stop { reason, cookies } => {
                                api_stop_retries = true;
                                let inspection = if cookies.is_empty() {
                                    open_task_browser(app, session.product_page().clone()).await
                                } else {
                                    open_direct_product_browser(
                                        app,
                                        session.product_page().to_string(),
                                        cookies,
                                    )
                                    .await
                                };
                                return Err(match inspection {
                                    Ok(()) => format!("{reason}；已在独立浏览器打开商品页供你处理验证或等待放行。本轮不会自动重试加车。"),
                                    Err(err) => format!("{reason}；无法交接独立浏览器商品页：{err}。本轮不会自动重试加车。"),
                                });
                            }
                            direct_cart::AddOutcome::Uncertain { reason, cookies } => {
                                api_stop_retries = true;
                                let inspection = if cookies.is_empty() {
                                    Err("会话没有可交接的 Apple Cookie".to_owned())
                                } else {
                                    open_direct_bag_browser(app, region.bag_url(), cookies).await
                                };
                                return Err(match inspection {
                                    Ok(()) => format!(
                                        "{reason}；已在独立浏览器打开同一会话的购物袋，请核对商品和数量。本轮不会再次加车或自动结账。"
                                    ),
                                    Err(err) => format!(
                                        "{reason}；无法自动打开同一会话的购物袋：{err}。本轮不会再次加车或自动结账。"
                                    ),
                                });
                            }
                        };
                        api_add_confirmed = true;
                        api_stop_retries = true;
                        let bag = format!(
                            "{}#apw-checkout=1&apw-store={}&apw-part={}",
                            region.bag_url(),
                            target.store_number,
                            target.part_number.to_ascii_uppercase().replace('/', "%2F")
                        );
                        let checkout =
                            browser_task_url(&bag, &task_id, &target.product_name, area)?;
                        open_direct_bag_browser(app, checkout.to_string(), cookies).await
                    }
                    .await
                }
            };
            let stage = if result.is_ok() { "launched" } else { "failed" };
            match bridge.advance_task(stage, &target.part_number, &target.store_number, &task_id) {
                Ok(Some(status)) => {
                    let _ = app.emit(payment_bridge::STATUS_CHANNEL, status);
                }
                Err(err) => {
                    let _ = app.emit(NOTICE_CHANNEL, format!("保存结账任务状态失败：{err}；本轮已停止，避免重复加车"));
                    return Ok(());
                }
                Ok(None) if bridge.has_progressed(&task_id, stage) => {}
                Ok(None) => {
                    let _ = app.emit(NOTICE_CHANNEL, "结账任务已被替换或阶段已变化；本轮停止自动加车");
                    return Ok(());
                }
            }
            if result.is_ok() {
                let watchdog_app = app.clone();
                let (watchdog_delay, watchdog_message) = match add_method {
                    AddMethod::Api => (
                        Duration::from_secs(90),
                        "直连加车已被 Apple 确认，但购物袋在 90 秒内未完成核对；请检查独立浏览器。",
                    ),
                    AddMethod::Browser => (
                        Duration::from_millis(release_task::WAIT_LIMIT_MS),
                        "商品页等待 Apple 放行超过 4 小时，仍未确认加入购物袋；请检查独立浏览器。",
                    ),
                };
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(watchdog_delay).await;
                    let bridge = watchdog_app.state::<PaymentBridge>();
                    match bridge.fail_if_stage(&task_id, "launched") {
                        Ok(Some(status)) => {
                            let _ = watchdog_app.emit(payment_bridge::STATUS_CHANNEL, status);
                            let _ = watchdog_app.emit(NOTICE_CHANNEL, watchdog_message);
                        }
                        Err(err) => { let _ = watchdog_app.emit(NOTICE_CHANNEL, format!("保存结账任务状态失败：{err}")); }
                        Ok(None) => {}
                    }
                });
            }
            if api_stop_retries && let Err(err) = &result {
                let detail = if api_add_confirmed {
                    format!("Apple 已确认直连加车，但浏览器交接失败：{err}。请手动检查购物袋，本轮不会再次加车。")
                } else {
                    err.clone()
                };
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    detail,
                );
                return Ok(());
            }
            result
        },
    )
    .await
}

fn retryable_target<'a>(
    snapshot: &'a [TargetState],
    preferred: Option<&Target>,
    settings: &Settings,
) -> Option<&'a Target> {
    let valid = |state: &TargetState| {
        state.availability.is_in_stock()
            && release_task::is_restock(settings, &state.target)
            && region_by_locale(&state.target.locale)
                .and_then(|region| region.auto_add_url(&state.target.part_number))
                .is_some()
            && (2..=10).contains(&state.target.store_number.len())
            && state
                .target
                .store_number
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
    };
    let preferred_key = preferred.map(Target::key);
    snapshot
        .iter()
        .find(|state| valid(state) && preferred_key.as_ref() == Some(&state.target.key()))
        .or_else(|| snapshot.iter().find(|state| valid(state)))
        .map(|state| &state.target)
}

#[cfg(test)]
mod retry_target_tests {
    use super::*;
    use apw_core::config::{AddMethod, TaskMode, TaskPlan};
    use apw_core::model::{Availability, UnknownReason};

    #[test]
    fn retries_only_a_currently_stocked_valid_target() {
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let mut state = TargetState {
            target,
            availability: Availability::Unknown(UnknownReason::NotYetChecked),
            last_checked_ms: None,
            consecutive_failures: 0,
        };
        let settings = Settings::default();
        assert!(retryable_target(&[state.clone()], None, &settings).is_none());
        state.availability = Availability::OutOfStock;
        assert!(retryable_target(&[state.clone()], None, &settings).is_none());
        state.availability = Availability::InStock;
        assert_eq!(
            retryable_target(&[state.clone()], None, &settings)
                .unwrap()
                .store_number,
            "R683"
        );
        let mut release_settings = Settings::default();
        release_settings.task_plans.push(TaskPlan {
            target_key: state.target.key().0,
            mode: TaskMode::Release,
            add_method: AddMethod::Browser,
            product_url: None,
            release_at_ms: Some(1_800_000_000_000),
            retry_delay_ms: 3_000,
        });
        assert!(retryable_target(&[state.clone()], None, &release_settings).is_none());
        let preferred = state.clone();
        let mut earlier = state.clone();
        earlier.target.store_number = "R001".into();
        let candidates = [earlier, preferred.clone()];
        assert_eq!(
            retryable_target(&candidates, Some(&preferred.target), &settings)
                .unwrap()
                .store_number,
            "R683"
        );
        state.target.part_number = "../bag".into();
        assert!(retryable_target(&[state.clone()], None, &settings).is_none());
        state.target.part_number = "MJYE4CH/A".into();
        state.target.store_number = "bad/store".into();
        assert!(retryable_target(&[state], None, &settings).is_none());
    }
}

/// 先转发库存事件与提醒；购买任务即使等 Apple 或 Chrome，也不能阻塞后续目标的通知。
async fn pump_events(app: AppHandle, mut events: tokio::sync::mpsc::Receiver<Event>) {
    let (automation, pending_automation) = tokio::sync::mpsc::unbounded_channel();
    let automation_app = app.clone();
    let live_run = std::sync::Arc::new(AtomicBool::new(false));
    let automation_task = tauri::async_runtime::spawn(run_automation(
        automation_app,
        pending_automation,
        live_run.clone(),
    ));

    let (notifications, pending) = notification_queue::channel();
    let notification_app = app.clone();
    tauri::async_runtime::spawn(notification_queue::run(pending, move |notification| {
        let app = notification_app.clone();
        async move {
            if let Err(err) = dispatch_notification(&app, notification).await {
                let _ = app.emit(NOTICE_CHANNEL, format!("发送提醒时出错：{err}"));
            }
        }
    }));

    let mut automation_available = true;
    while let Some(event) = events.recv().await {
        if let Event::RunStateChanged { running } = &event {
            live_run.store(*running, Ordering::SeqCst);
            if !running {
                pause_checkout_task(&app);
            }
        }
        let _ = app.emit(EVENT_CHANNEL, &event);
        let notification = if let Event::InStock { state } = &event {
            let target = &state.target;
            let settings = app.state::<AppState>().settings_snapshot();
            let notification = Notification::new(
                "有货了",
                format!("{} {}", target.store_title, target.product_name),
            );
            Some(match region_by_locale(&target.locale) {
                Some(region) if settings.auto_add_to_bag => notification.with_url(
                    region
                        .product_page_url(&target.part_number)
                        .unwrap_or_else(|| region.bag_url()),
                ),
                Some(region) => notification.with_url(region.bag_url()),
                None => notification,
            })
        } else {
            None
        };
        if automation_available
            && matches!(
                &event,
                Event::RunStateChanged { .. } | Event::InStock { .. } | Event::CycleComplete { .. }
            )
            && automation.send(event).is_err()
        {
            automation_available = false;
            let _ = app.emit(
                NOTICE_CHANNEL,
                "自动任务处理已停止；库存事件与提醒仍会继续。",
            );
        }
        if let Some(notification) = notification
            && notifications.send(notification).await.is_err()
        {
            let _ = app.emit(NOTICE_CHANNEL, "提醒发送任务已停止，无法发送到货提醒");
        }
    }
    automation_task.abort();
}

/// 自动任务按库存事件顺序执行，保留单次加车及有限重试语义。
async fn run_automation(
    app: AppHandle,
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    live_run: std::sync::Arc<AtomicBool>,
) {
    let mut auto_open_bag = AutoOpenBag::default();
    let mut watch_running = false;
    let mut auto_add_enabled = app.state::<AppState>().settings_snapshot().auto_add_to_bag;
    let mut pending_retry: Option<Target> = None;
    let mut retry_at: Option<Instant> = None;
    let mut latest_snapshot: Vec<TargetState> = Vec::new();
    let mut release_cancel: Option<std::sync::Arc<AtomicBool>> = None;
    let mut task_tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = task_tick.tick() => {
                let settings = app.state::<AppState>().settings_snapshot();
                if !watch_running || !live_run.load(Ordering::SeqCst) || !settings.auto_add_to_bag {
                    if let Some(cancel) = &release_cancel {
                        cancel.store(true, Ordering::SeqCst);
                    }
                    continue;
                }

                if !auto_open_bag.has_opened()
                    && let Some((target, plan)) = release_task::due_release(&settings, release_task::now_ms())
                    && auto_open_bag.reserve_release()
                {
                    let cancel = std::sync::Arc::new(AtomicBool::new(false));
                    release_cancel = Some(cancel.clone());
                    let release_app = app.clone();
                    let checkout_area = app.state::<AppState>()
                        .catalog
                        .store_by_number(&target.locale, &target.store_number)
                        .and_then(|store| {
                            store.checkout_area(&target.locale)
                                .map(|(province, city, district)| {
                                    (province.to_string(), city.to_string(), district.to_string())
                                })
                        });
                    tauri::async_runtime::spawn(release_task::run(
                        release_app, target, plan, checkout_area, cancel,
                    ));
                }

                if let Some(preferred) = pending_retry.as_ref() {
                    if !auto_open_bag.has_opened()
                        && retry_at.is_some_and(|at| Instant::now() >= at)
                    {
                        if let Some(target) = retryable_target(&latest_snapshot, Some(preferred), &settings).cloned() {
                            if let Err(err) = open_auto_add(&app, &mut auto_open_bag, &target).await {
                                retry_at = Some(Instant::now() + Duration::from_millis(
                                    release_task::plan_for(&settings, &target)
                                        .map_or(apw_core::config::DEFAULT_TASK_RETRY_DELAY_MS, |plan| plan.retry_delay_ms),
                                ));
                                let _ = app.emit(NOTICE_CHANNEL, format!("重试自动加车失败：{err}"));
                            } else if auto_open_bag.has_opened() {
                                pending_retry = None;
                                retry_at = None;
                            }
                        } else {
                            pending_retry = None;
                            retry_at = None;
                        }
                    }
                    if auto_open_bag.retry_limit_reached() {
                        pending_retry = None;
                        retry_at = None;
                        let _ = app.emit(NOTICE_CHANNEL, "自动加车连续三次失败，本次监控不再重试；检查任务日志后暂停并重新开始监控。");
                    }
                }
                continue;
            }
        };
        let Some(event) = event else { break };
        if let Event::RunStateChanged { running } = &event {
            if !running && let Some(cancel) = release_cancel.take() {
                cancel.store(true, Ordering::SeqCst);
            }
            auto_open_bag.on_run_state_changed(*running);
            watch_running = *running;
            pending_retry = None;
            retry_at = None;
            latest_snapshot.clear();
        }

        if let Event::InStock { state } = &event {
            let target = &state.target;
            let settings = app
                .try_state::<AppState>()
                .map(|s| s.settings_snapshot())
                .unwrap_or_default();
            if settings.auto_add_to_bag && !auto_add_enabled {
                auto_open_bag.on_auto_add_enabled();
                pending_retry = None;
            }
            auto_add_enabled = settings.auto_add_to_bag;

            if settings.auto_add_to_bag
                && region_by_locale(&target.locale)
                    .and_then(|r| r.auto_add_url(&target.part_number))
                    .is_none()
            {
                let _ = app.emit(NOTICE_CHANNEL, "零件号无效，已跳过该目标的自动加车");
            }

            if !watch_running || !live_run.load(Ordering::SeqCst) {
                continue;
            }

            use tauri_plugin_opener::OpenerExt;
            let open_result =
                if settings.auto_add_to_bag && release_task::is_restock(&settings, target) {
                    open_auto_add(&app, &mut auto_open_bag, target).await
                } else if settings.auto_add_to_bag {
                    Ok(())
                } else {
                    auto_open_bag.open_if_needed(
                        settings.open_bag_on_hit,
                        &target.locale,
                        None,
                        |url| {
                            app.opener()
                                .open_url(url, None::<&str>)
                                .map_err(|e| e.to_string())
                        },
                    )
                };
            if settings.auto_add_to_bag && release_task::is_restock(&settings, target) {
                if open_result.is_err() {
                    pending_retry = Some(target.clone());
                    retry_at = Some(
                        Instant::now()
                            + Duration::from_millis(
                                release_task::plan_for(&settings, target).map_or(
                                    apw_core::config::DEFAULT_TASK_RETRY_DELAY_MS,
                                    |plan| plan.retry_delay_ms,
                                ),
                            ),
                    );
                } else if auto_open_bag.has_opened() {
                    pending_retry = None;
                    retry_at = None;
                }
            }
            if let Err(err) = open_result {
                let _ = app.emit(NOTICE_CHANNEL, format!("自动加车失败：{err}"));
                if settings.auto_add_to_bag && auto_open_bag.retry_limit_reached() {
                    let _ = app.emit(
                        NOTICE_CHANNEL,
                        "自动加车连续三次失败，本次监控不再重试；检查任务日志后暂停并重新开始监控。",
                    );
                }
            }
        }

        if let Event::CycleComplete { snapshot, .. } = &event {
            latest_snapshot = snapshot.clone();
            let settings = app.state::<AppState>().settings_snapshot();
            if settings.auto_add_to_bag && !auto_add_enabled {
                auto_open_bag.on_auto_add_enabled();
                pending_retry = None;
            }
            auto_add_enabled = settings.auto_add_to_bag;
            if !auto_add_enabled {
                pending_retry = None;
                retry_at = None;
            }
            let ready_for_attempt = auto_open_bag.should_try_on_cycle();
            if !watch_running
                || !live_run.load(Ordering::SeqCst)
                || !settings.auto_add_to_bag
                || !ready_for_attempt
                || pending_retry.is_some()
            {
                continue;
            }
            let Some(target) = retryable_target(snapshot, None, &settings) else {
                continue;
            };
            if let Err(err) = open_auto_add(&app, &mut auto_open_bag, target).await {
                pending_retry = Some(target.clone());
                retry_at = Some(
                    Instant::now()
                        + Duration::from_millis(
                            release_task::plan_for(&settings, target)
                                .map_or(apw_core::config::DEFAULT_TASK_RETRY_DELAY_MS, |plan| {
                                    plan.retry_delay_ms
                                }),
                        ),
                );
                let _ = app.emit(NOTICE_CHANNEL, format!("重试自动加车失败：{err}"));
                if auto_open_bag.retry_limit_reached() {
                    let _ = app.emit(
                        NOTICE_CHANNEL,
                        "自动加车连续三次失败，本次监控不再重试；检查任务日志后暂停并重新开始监控。",
                    );
                }
            } else if auto_open_bag.has_opened() {
                pending_retry = None;
                retry_at = None;
            }
        }
    }
}

/// 建系统托盘。
///
/// 关窗口时不退出而是收进托盘：这个工具的正常用法就是挂上几个小时等发售，
/// 让它一直占着一个窗口和 Dock 图标没有道理。
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("默认窗口图标缺失，无法建立托盘".into())
            })?,
        )
        .tooltip("Apple Bot")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 载入设置。读不出来时放弃写盘并留档原文件。
///
/// 这一条是刻意的：读不到旧配置**不等于**用户没有配置。若照常写盘，界面初始化
/// 时的几次控件赋值就会把仅存的那份原子替换成一份空的默认配置，监控列表再也
/// 找不回来。Go 版正是这么丢过数据。
fn load_settings(notices: &mut Vec<String>) -> (Settings, Option<SettingsStore>) {
    let store = match SettingsStore::new() {
        Ok(s) => s,
        Err(err) => {
            notices.push(format!("配置目录不可用，本次运行的设置不会被保存：{err}"));
            return (Settings::default(), None);
        }
    };

    // 「新版配置文件还不存在」才是首次运行的判据。
    //
    // 不能用「目标列表为空」代替：用户删光目标后保存的是一份合法的空目标配置，
    // 而旧版 settings.json 是刻意保留不删的（为了能回退），于是下次启动会把他
    // 亲手删掉的目标连同 locale、Bark 地址一起原样倒回来。
    let first_run = store.path().symlink_metadata().is_err();

    match store.load() {
        Ok(settings) => {
            if first_run
                && let Some(legacy) = store.import_legacy()
                && !legacy.targets.is_empty()
            {
                notices.push(format!(
                    "已从旧版设置迁移了 {} 条监控目标。",
                    legacy.targets.len()
                ));
                // 立刻落盘。否则用户不改任何设置时新版文件一直不存在，
                // 每次启动都要重迁一遍，用户删掉的目标也会一直复活。
                if let Err(err) = store.save(&legacy) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (legacy, Some(store));
            }
            (settings, Some(store))
        }
        Err(err) => {
            match store.preserve_corrupted() {
                Ok(Some(path)) => {
                    notices.push(format!("读取设置失败，原文件已备份到 {}", path.display()))
                }
                Ok(None) => {}
                Err(backup_err) => {
                    notices.push(format!("读取设置失败，且无法备份原文件：{backup_err}"));
                }
            }
            notices.push(format!(
                "读取设置失败，已回退到默认设置，并且本次运行不会覆盖磁盘上的配置：{err}"
            ));
            (Settings::default(), None)
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        // 扩展桥接使用固定回环端口；只允许一个桌面实例持有它。再次打开时
        // 显示已有窗口，也避免两份后台监控同时查询和发送到货通知。
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            reveal_window(app)
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let mut notices = Vec::new();
            let (mut settings, store) = load_settings(&mut notices);

            // 旧版本保存的 Apple Watch 目标没有搭档表带，查出来会是假的
            // 「无货」；启动时按目录补齐，下次写盘就带上了。
            let catalog = Catalog::new();
            catalog.attach_companions(&mut settings.targets);
            catalog.attach_locations(&mut settings.targets);

            let client = AppleClient::new(ClientConfig {
                proxies: settings.proxies.clone(),
                ..ClientConfig::default()
            })
            .map_err(|e| format!("构造 Apple 客户端失败：{e}"))?;

            // 用 Watcher::new 而不是 Watcher::spawn：setup 回调跑在主线程上，
            // 并不处在 tokio 运行时上下文里，在这里 tokio::spawn 会 panic，
            // 而且因为发生在不可展开的回调中，进程会直接 abort。
            // 引擎任务交给 Tauri 自己的运行时去驱动。
            let (watcher, events, engine) = Watcher::new(client.clone(), WatcherConfig::default());
            tauri::async_runtime::spawn(engine);

            {
                let watcher = watcher.clone();
                let targets = settings.targets.clone();
                let interval = settings.interval();
                tauri::async_runtime::spawn(async move {
                    watcher.set_targets(targets).await;
                    watcher.set_interval(interval).await;
                });
            }

            let handle: AppHandle = app.handle().clone();
            let (bridge, bridge_listener) = match payment_bridge::prepare(&handle) {
                Ok((bridge, listener, notice)) => {
                    if let Some(notice) = notice {
                        notices.push(notice);
                    }
                    (bridge, Some(listener))
                }
                Err(err) => {
                    notices.push(format!("付款链接桥接不可用：{err}"));
                    (PaymentBridge::disabled(), None)
                }
            };
            app.manage(bridge);

            app.manage(AppState {
                watcher,
                client,
                catalog,
                http: reqwest::Client::new(),
                browser_install: tokio::sync::Mutex::new(()),
                settings: RwLock::new(settings),
                store,
            });

            if let Some(listener) = bridge_listener {
                tauri::async_runtime::spawn(payment_bridge::serve(handle.clone(), listener));
            }
            tauri::async_runtime::spawn(pump_events(handle.clone(), events));

            if let Err(err) = setup_tray(&handle) {
                // 托盘建不起来只是少一项能力，不该让程序起不来。
                notices.push(format!("系统托盘不可用：{err}"));
            }

            if !notices.is_empty() {
                // 界面还没订阅上，稍等一下再发。这些是降级说明，用户必须看见 ——
                // 只写到 stderr 是没用的，用户是双击图标启动的。
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    for notice in notices {
                        let _ = handle.emit(NOTICE_CHANNEL, notice);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 收进托盘而不是退出。真要退出走托盘菜单里的「退出」。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_regions,
            list_categories,
            list_stores,
            list_products,
            refresh_products,
            get_settings,
            get_bridge_pairing_code,
            get_payment_link,
            get_bot_status,
            open_extension_folder,
            prepare_bot_browser,
            open_bot_browser,
            save_settings,
            set_push_urls,
            get_snapshot,
            set_targets,
            set_interval,
            start_watching,
            stop_watching,
            is_running,
            test_notify,
            check_for_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
