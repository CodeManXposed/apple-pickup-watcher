//! 发售任务调度：提前打开官方商品页，在原标签页等待 Apple 放行。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use apw_core::config::{AddMethod, Settings, TaskMode, TaskPlan};
use apw_core::model::{Target, region_by_locale};
use tauri::{AppHandle, Emitter, Manager};

use crate::browser_bot;
use crate::direct_cart::{self, AddOutcome, DirectCart};
use crate::payment_bridge::{self, PaymentBridge, PaymentLink};
use crate::{NOTICE_CHANNEL, browser_task_url};

pub(crate) const PREWARM_MS: u64 = 10 * 60 * 1_000;
pub(crate) const WAIT_LIMIT_MS: u64 = 4 * 60 * 60 * 1_000;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn plan_for<'a>(settings: &'a Settings, target: &Target) -> Option<&'a TaskPlan> {
    let key = target.key().0;
    settings
        .task_plans
        .iter()
        .find(|plan| plan.target_key == key)
}

pub(crate) fn is_restock(settings: &Settings, target: &Target) -> bool {
    plan_for(settings, target).is_none_or(|plan| plan.mode == TaskMode::Restock)
}

/// 只选择本轮最早到时的一条发售任务；旧版设置中的目标仍默认是补货模式。
pub(crate) fn due_release(settings: &Settings, now: u64) -> Option<(Target, TaskPlan)> {
    settings
        .targets
        .iter()
        .filter_map(|target| {
            let plan = plan_for(settings, target)?;
            let at = plan.release_at_ms?;
            if plan.mode != TaskMode::Release
                || now.saturating_add(PREWARM_MS) < at
                || now > at.saturating_add(WAIT_LIMIT_MS)
                || region_by_locale(&target.locale)
                    .and_then(|region| region.auto_add_url(&target.part_number))
                    .is_none()
                || !(2..=10).contains(&target.store_number.len())
                || !target
                    .store_number
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric())
            {
                return None;
            }
            Some((target.clone(), plan.clone()))
        })
        .min_by_key(|(_, plan)| plan.release_at_ms)
}

fn stage(app: &AppHandle, task: &Target, task_id: &str, next: &str) -> Result<(), String> {
    let bridge = app.state::<PaymentBridge>();
    match bridge.advance_task(next, &task.part_number, &task.store_number, task_id)? {
        Some(status) => {
            let _ = app.emit(payment_bridge::STATUS_CHANNEL, status);
        }
        None if bridge.has_progressed(task_id, next) => {}
        None => return Err("任务已被替换或阶段已变化".into()),
    }
    Ok(())
}

fn fail(app: &AppHandle, task_id: &str, reason: &str) {
    let bridge = app.state::<PaymentBridge>();
    for waiting in ["starting", "waitingRelease", "waitingRoom"] {
        match bridge.fail_if_stage(task_id, waiting) {
            Ok(Some(status)) => {
                let _ = app.emit(payment_bridge::STATUS_CHANNEL, status);
                let _ = app.emit(NOTICE_CHANNEL, reason);
                break;
            }
            Err(err) => {
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    format!("{reason}；保存结账任务状态失败：{err}"),
                );
                break;
            }
            Ok(None) => {}
        }
    }
}

async fn wait_or_cancel(cancelled: &AtomicBool, duration: Duration) -> bool {
    tokio::time::sleep(duration).await;
    cancelled.load(Ordering::SeqCst)
}

enum DirectReleaseResult {
    Opened,
    NoRetry(String),
}

async fn direct_release_add(
    app: &AppHandle,
    target: &Target,
    plan: &TaskPlan,
    task_id: &str,
    tab_id: &str,
    checkout_area: Option<&(String, String, String)>,
) -> Result<DirectReleaseResult, String> {
    let region = region_by_locale(&target.locale).ok_or("发售任务地区无效")?;
    let session = DirectCart::new(region, target, Some(plan))?;
    let host = reqwest::Url::parse(region.base_url)
        .map_err(|_| "地区官网 URL 无效")?
        .host_str()
        .ok_or("地区官网缺少域名")?
        .to_owned();
    let app_copy = app.clone();
    match tauri::async_runtime::spawn_blocking(move || browser_bot::apple_cookies(&app_copy, &host))
        .await
    {
        Ok(Ok(cookies)) => session.seed_browser_cookies(&cookies),
        Ok(Err(err)) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("读取独立浏览器 Apple 会话失败：{err}；本轮使用新 HTTP 会话"),
            );
        }
        Err(err) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("读取独立浏览器 Apple 会话中断：{err}；本轮使用新 HTTP 会话"),
            );
        }
    }
    let cookies = match session.add().await? {
        AddOutcome::Confirmed(cookies) => cookies,
        AddOutcome::Stop(reason) => return Ok(DirectReleaseResult::NoRetry(reason)),
        AddOutcome::Uncertain { reason, cookies } => {
            let inspection = if cookies.is_empty() {
                Err("会话没有可交接的 Apple Cookie".to_owned())
            } else {
                let app_copy = app.clone();
                let bag = region.bag_url();
                let tab_id = tab_id.to_owned();
                tauri::async_runtime::spawn_blocking(move || {
                    browser_bot::open_direct_bag(&app_copy, &bag, &cookies, Some(&tab_id))
                })
                .await
                .map_err(|err| err.to_string())
                .and_then(|result| result)
            };
            let detail = match inspection {
                Ok(()) => format!(
                    "{reason}；已在独立浏览器打开同一会话的购物袋，请核对商品和数量。本轮不会再次加车或自动结账。"
                ),
                Err(err) => format!(
                    "{reason}；无法自动打开同一会话的购物袋：{err}。本轮不会再次加车或自动结账。"
                ),
            };
            return Ok(DirectReleaseResult::NoRetry(detail));
        }
    };
    let handoff = async {
        let bag = format!(
            "{}#apw-checkout=1&apw-store={}&apw-part={}",
            region.bag_url(),
            target.store_number,
            target.part_number.to_ascii_uppercase().replace('/', "%2F")
        );
        let checkout = browser_task_url(
            &bag,
            task_id,
            &target.product_name,
            checkout_area.map(|(province, city, district)| {
                (province.as_str(), city.as_str(), district.as_str())
            }),
        )?;
        let app_copy = app.clone();
        let tab_id = tab_id.to_owned();
        tauri::async_runtime::spawn_blocking(move || {
            browser_bot::open_direct_bag(&app_copy, checkout.as_str(), &cookies, Some(&tab_id))
        })
        .await
        .map_err(|err| err.to_string())?
    }
    .await;
    Ok(match handoff {
        Ok(()) => DirectReleaseResult::Opened,
        Err(err) => DirectReleaseResult::NoRetry(format!(
            "Apple 已确认直连加车，但浏览器交接失败：{err}。请手动检查购物袋，本轮不会再次加车。"
        )),
    })
}

pub(crate) async fn run(
    app: AppHandle,
    target: Target,
    plan: TaskPlan,
    checkout_area: Option<(String, String, String)>,
    cancelled: Arc<AtomicBool>,
) {
    let bridge = app.state::<PaymentBridge>();
    let initial = match bridge.start_task(&target.part_number, &target.store_number) {
        Ok(initial) => initial,
        Err(err) => {
            let _ = app.emit(NOTICE_CHANNEL, format!("保存发售任务失败：{err}"));
            return;
        }
    };
    let task_id = initial.task_id.clone();
    let _ = app.emit(payment_bridge::EVENT_CHANNEL, Option::<PaymentLink>::None);
    let _ = app.emit(payment_bridge::STATUS_CHANNEL, initial);

    let Some(region) = region_by_locale(&target.locale) else {
        fail(&app, &task_id, "发售任务地区无效");
        return;
    };
    let prewarm_url = if plan.add_method == AddMethod::Api {
        match direct_cart::product_url(region, &target, Some(&plan)) {
            Ok(url) => url.to_string(),
            Err(err) => {
                fail(&app, &task_id, &format!("发售任务商品地址无效：{err}"));
                return;
            }
        }
    } else {
        let Some(url) = region.product_page_url(&target.part_number) else {
            fail(&app, &task_id, "发售任务零件号无效");
            return;
        };
        url
    };
    let Some(base) = region.auto_add_url(&target.part_number) else {
        fail(&app, &task_id, "发售任务加车入口无效");
        return;
    };
    let base = format!("{base}&apw-store={}", target.store_number);
    let task_url = match browser_task_url(
        &base,
        &task_id,
        &target.product_name,
        checkout_area.as_ref().map(|(province, city, district)| {
            (province.as_str(), city.as_str(), district.as_str())
        }),
    ) {
        Ok(url) => url.to_string(),
        Err(err) => {
            fail(&app, &task_id, &format!("发售任务地址无效：{err}"));
            return;
        }
    };
    let delay = Duration::from_millis(plan.retry_delay_ms);
    let mut tab_id = None;
    for attempt in 0..3 {
        if cancelled.load(Ordering::SeqCst) {
            fail(&app, &task_id, "发售任务已暂停");
            return;
        }
        let app_copy = app.clone();
        let url = prewarm_url.clone();
        let opened = tauri::async_runtime::spawn_blocking(move || {
            browser_bot::prewarm_task(&app_copy, &url)
        })
        .await
        .map_err(|err| err.to_string())
        .and_then(|result| result);
        match opened {
            Ok(id) => {
                tab_id = Some(id);
                break;
            }
            Err(err) if attempt == 2 => {
                fail(&app, &task_id, &format!("发售任务浏览器启动失败：{err}"));
                return;
            }
            Err(_) => {
                if wait_or_cancel(&cancelled, delay).await {
                    fail(&app, &task_id, "发售任务已暂停");
                    return;
                }
            }
        }
    }
    let Some(tab_id) = tab_id else { return };
    if let Err(err) = stage(&app, &target, &task_id, "waitingRelease") {
        let _ = app.emit(NOTICE_CHANNEL, format!("保存发售任务状态失败：{err}"));
        return;
    }
    let _ = app.emit(
        NOTICE_CHANNEL,
        "发售任务已在独立浏览器待命；如遇 Apple 验证，请在原标签页完成。".to_owned(),
    );

    let at = plan.release_at_ms.unwrap_or_default();
    while now_ms() < at {
        if wait_or_cancel(&cancelled, Duration::from_millis(1_000)).await {
            fail(&app, &task_id, "发售任务已暂停");
            return;
        }
    }
    if let Err(err) = stage(&app, &target, &task_id, "waitingRoom") {
        let _ = app.emit(NOTICE_CHANNEL, format!("保存发售任务状态失败：{err}"));
        return;
    }
    let deadline = at.saturating_add(WAIT_LIMIT_MS);
    let mut errors = 0;
    let mut last_missing_refresh: Option<Instant> = None;
    while now_ms() <= deadline {
        if cancelled.load(Ordering::SeqCst) {
            fail(&app, &task_id, "发售任务已暂停");
            return;
        }
        let app_copy = app.clone();
        let id = tab_id.clone();
        let part = target.part_number.clone();
        let url = task_url.clone();
        let refresh_url = prewarm_url.clone();
        let stop = cancelled.clone();
        let refresh_missing =
            last_missing_refresh.is_none_or(|last| last.elapsed() >= Duration::from_secs(60));
        let perform_add = plan.add_method == AddMethod::Browser;
        let result = tauri::async_runtime::spawn_blocking(move || {
            browser_bot::activate_release(
                &app_copy,
                &id,
                &part,
                browser_bot::ReleaseUrls {
                    task: &url,
                    refresh: &refresh_url,
                },
                refresh_missing,
                perform_add,
                &stop,
            )
        })
        .await
        .map_err(|err| err.to_string())
        .and_then(|result| result);
        if refresh_missing {
            last_missing_refresh = Some(Instant::now());
        }
        let result = match result {
            Ok(true) if plan.add_method == AddMethod::Api => {
                if cancelled.load(Ordering::SeqCst) {
                    fail(&app, &task_id, "发售任务已暂停");
                    return;
                }
                match direct_release_add(
                    &app,
                    &target,
                    &plan,
                    &task_id,
                    &tab_id,
                    checkout_area.as_ref(),
                )
                .await
                {
                    Ok(DirectReleaseResult::Opened) => Ok(true),
                    Ok(DirectReleaseResult::NoRetry(err)) => {
                        fail(&app, &task_id, &err);
                        return;
                    }
                    Err(err) => Err(err),
                }
            }
            other => other,
        };
        match result {
            Ok(true) => {
                if let Err(err) = stage(&app, &target, &task_id, "launched") {
                    let _ = app.emit(
                        NOTICE_CHANNEL,
                        format!("保存发售任务状态失败：{err}；本轮已停止，避免重复加车"),
                    );
                    return;
                }
                let watchdog_app = app.clone();
                let watchdog_message = if plan.add_method == AddMethod::Api {
                    "发售任务直连加车已被确认，但购物袋在 90 秒内未完成核对；请检查独立浏览器。"
                } else {
                    "发售任务已放行，但 90 秒内未确认加车；请检查 Apple 页面。"
                };
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(90)).await;
                    let bridge = watchdog_app.state::<PaymentBridge>();
                    match bridge.fail_if_stage(&task_id, "launched") {
                        Ok(Some(status)) => {
                            let _ = watchdog_app.emit(payment_bridge::STATUS_CHANNEL, status);
                            let _ = watchdog_app.emit(NOTICE_CHANNEL, watchdog_message);
                        }
                        Err(err) => {
                            let _ = watchdog_app
                                .emit(NOTICE_CHANNEL, format!("保存发售任务状态失败：{err}"));
                        }
                        Ok(None) => {}
                    }
                });
                return;
            }
            Ok(false) => errors = 0,
            Err(err) => {
                errors += 1;
                if errors >= 3 {
                    fail(&app, &task_id, &format!("发售页连续检查失败：{err}"));
                    return;
                }
            }
        }
        if wait_or_cancel(&cancelled, delay).await {
            fail(&app, &task_id, "发售任务已暂停");
            return;
        }
    }
    fail(
        &app,
        &task_id,
        "发售等待超过四小时，请检查 Apple 页面后重新启动任务。",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_starts_prewarm_ten_minutes_before_and_does_not_depend_on_stock() {
        let mut settings = Settings::default();
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        settings.targets.push(target.clone());
        settings.task_plans.push(TaskPlan {
            target_key: target.key().0,
            mode: TaskMode::Release,
            add_method: apw_core::config::AddMethod::Browser,
            product_url: None,
            release_at_ms: Some(1_000_000),
            retry_delay_ms: 3_000,
        });
        assert!(!is_restock(&settings, &target));
        assert!(due_release(&settings, 399_999).is_none());
        assert!(due_release(&settings, 400_000).is_some());
        assert!(due_release(&settings, 1_000_000 + WAIT_LIMIT_MS).is_some());
        assert!(due_release(&settings, 1_000_000 + WAIT_LIMIT_MS + 1).is_none());
    }
}
