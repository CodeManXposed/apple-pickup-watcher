//! 每次监控会话只自动打开一次购买入口，避免多个目标同时到货打断结账。

use apw_core::model::region_by_locale;
use std::future::Future;

const MAX_AUTO_ADD_ATTEMPTS: u8 = 3;

#[derive(Default)]
pub(crate) struct AutoOpenBag {
    opened_this_run: bool,
    auto_task_launched: bool,
    auto_add_failures: u8,
    attempted_since_cycle: bool,
}

impl AutoOpenBag {
    fn rearm(&mut self) {
        self.opened_this_run = false;
        self.auto_task_launched = false;
        self.auto_add_failures = 0;
        self.attempted_since_cycle = false;
    }

    pub(crate) fn on_run_state_changed(&mut self, running: bool) {
        // 暂停不能重新布防：本轮已经排队的到货事件仍属于上一次会话。
        // 只有下一次真正开始监控时，才允许重新打开购物袋。
        if running {
            self.rearm();
        }
    }

    /// 普通购物袋已经打开时，运行中启用自动加车仍可启动一次新任务。
    pub(crate) fn on_auto_add_enabled(&mut self) {
        // 关闭再打开设置开关不能绕过「一轮只起一项购买任务」。
        if !self.auto_task_launched {
            self.rearm();
        }
    }

    /// 首次命中所在轮次不立刻重试；持续有货时后续轮次最多重试两次。
    pub(crate) fn should_try_on_cycle(&mut self) -> bool {
        let attempted = std::mem::take(&mut self.attempted_since_cycle);
        !attempted && !self.opened_this_run && self.auto_add_failures < MAX_AUTO_ADD_ATTEMPTS
    }

    pub(crate) fn retry_limit_reached(&self) -> bool {
        self.auto_add_failures >= MAX_AUTO_ADD_ATTEMPTS
    }

    pub(crate) fn has_opened(&self) -> bool {
        self.opened_this_run
    }

    /// 发售任务提前占用本轮唯一购买任务名额，避免其他补货命中打断等待页。
    pub(crate) fn reserve_release(&mut self) -> bool {
        if self.opened_this_run {
            return false;
        }
        self.opened_this_run = true;
        self.auto_task_launched = true;
        true
    }

    fn prepare_url(
        &mut self,
        enabled: bool,
        locale: &str,
        auto_add_target: Option<(&str, &str)>,
    ) -> Option<String> {
        if !enabled
            || self.opened_this_run
            || (auto_add_target.is_some() && self.retry_limit_reached())
        {
            return None;
        }
        let region = region_by_locale(locale)?;

        // 整个会话使用首个有效命中的地区。其他地区仍逐项提醒，但不再导航浏览器。
        // 打开失败不消耗这次机会，后续仍有货的轮次可以有限重试。
        let url = match auto_add_target {
            Some((part, store)) => match region.auto_add_url(part) {
                Some(url)
                    if (2..=10).contains(&store.len())
                        && store.bytes().all(|c| c.is_ascii_alphanumeric()) =>
                {
                    format!("{url}&apw-store={store}")
                }
                _ => return None,
            },
            None => region.bag_url(),
        };
        if auto_add_target.is_some() {
            self.attempted_since_cycle = true;
        }
        Some(url)
    }

    fn finish<E>(&mut self, auto_add_target: bool, result: Result<(), E>) -> Result<(), E> {
        match result {
            Ok(()) => {
                self.opened_this_run = true;
                self.auto_task_launched = auto_add_target;
                Ok(())
            }
            Err(error) => {
                if auto_add_target {
                    self.auto_add_failures += 1;
                }
                Err(error)
            }
        }
    }

    pub(crate) fn open_if_needed<E>(
        &mut self,
        enabled: bool,
        locale: &str,
        auto_add_target: Option<(&str, &str)>,
        open: impl FnOnce(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        let Some(url) = self.prepare_url(enabled, locale, auto_add_target) else {
            return Ok(());
        };
        self.finish(auto_add_target.is_some(), open(&url))
    }

    pub(crate) async fn open_if_needed_async<E, F, Fut>(
        &mut self,
        enabled: bool,
        locale: &str,
        auto_add_target: Option<(&str, &str)>,
        open: F,
    ) -> Result<(), E>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let Some(url) = self.prepare_url(enabled, locale, auto_add_target) else {
            return Ok(());
        };
        self.finish(auto_add_target.is_some(), open(url).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 多门店多颜色连续命中只打开一次购物袋() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        let mut opened = Vec::new();
        for _ in 0..15 {
            gate.open_if_needed(true, "zh_CN", None, |url| {
                opened.push(url.to_owned());
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(opened, vec!["https://www.apple.com.cn/shop/bag"]);

        // 下一轮再次有货，或者另一个地区随后到货，都不打断当前购物袋。
        gate.open_if_needed(true, "ja_JP", None, |_| panic!("同一会话不应再次打开"))
            .unwrap_or_else(|_: ()| unreachable!());
    }

    #[test]
    fn 暂停后残余事件不重开而下一次开始可以重新打开() {
        let mut gate = AutoOpenBag::default();
        let mut opened = Vec::new();
        for running in [true, false, true] {
            gate.on_run_state_changed(running);
            gate.open_if_needed(true, "ja_JP", None, |url| {
                opened.push(url.to_owned());
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(opened, vec!["https://www.apple.com/jp/shop/bag"; 2]);
    }

    #[test]
    fn 禁用和无效地区不会消耗会话的打开机会() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        for (enabled, locale) in [(false, "zh_CN"), (true, "invalid")] {
            gate.open_if_needed(enabled, locale, None, |_| panic!("不应打开购物袋"))
                .unwrap_or_else(|_: ()| unreachable!());
        }
        let mut opened = false;
        gate.open_if_needed(true, "zh_CN", None, |_| {
            opened = true;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(opened);
    }

    #[test]
    fn 打开失败会返回错误并允许下次命中重试() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        assert_eq!(
            gate.open_if_needed(true, "zh_CN", None, |_| Err("失败")),
            Err("失败")
        );

        let mut retried = false;
        gate.open_if_needed(true, "zh_CN", None, |_| {
            retried = true;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(retried);
    }

    #[test]
    fn 自动加车只对首个命中目标打开商品页() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        let mut opened = Vec::new();
        for part in ["MJYE4CH/A", "MJYA4CH/A"] {
            gate.open_if_needed(true, "zh_CN", Some((part, "R683")), |url| {
                opened.push(url.to_owned());
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(
            opened,
            vec![
                "https://www.apple.com.cn/shop/pdpAddToBag/MJYE4CH/A#apw-auto-add=MJYE4CH%2FA&apw-store=R683"
            ]
        );
    }

    #[test]
    fn 发售待命占用本轮任务名额() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        assert!(gate.reserve_release());
        assert!(!gate.reserve_release());
        gate.open_if_needed(true, "zh_CN", Some(("MJYE4CH/A", "R683")), |_| {
            panic!("发售待命后不能启动补货任务")
        })
        .unwrap_or_else(|_: ()| unreachable!());
        gate.on_run_state_changed(true);
        assert!(gate.reserve_release());
    }

    #[test]
    fn 无效零件号不消耗自动加车机会() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        gate.open_if_needed(true, "zh_CN", Some(("../bag", "R683")), |_| {
            panic!("无效零件号不应打开页面")
        })
        .unwrap_or_else(|_: ()| unreachable!());
        let mut opened = false;
        gate.open_if_needed(true, "zh_CN", Some(("MJYE4CH/A", "R683")), |_| {
            opened = true;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(opened);
    }

    #[test]
    fn 浏览器启动失败后只在后续库存轮次重试且有上限() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        let mut calls = 0;
        let fail = |gate: &mut AutoOpenBag, calls: &mut usize| {
            gate.open_if_needed(true, "zh_CN", Some(("MJYE4CH/A", "R683")), |_| {
                *calls += 1;
                Err("浏览器暂时不可用")
            })
        };
        assert_eq!(fail(&mut gate, &mut calls), Err("浏览器暂时不可用"));
        assert!(!gate.should_try_on_cycle(), "命中所在轮次不应立刻再试");
        assert!(gate.should_try_on_cycle());
        assert_eq!(fail(&mut gate, &mut calls), Err("浏览器暂时不可用"));
        assert!(!gate.should_try_on_cycle());
        assert!(gate.should_try_on_cycle());
        assert_eq!(fail(&mut gate, &mut calls), Err("浏览器暂时不可用"));
        assert!(gate.retry_limit_reached());
        assert!(!gate.should_try_on_cycle());
        gate.open_if_needed(
            true,
            "zh_CN",
            Some(("MJYE4CH/A", "R683")),
            |_| -> Result<(), ()> { panic!("达到上限后不应再打开浏览器") },
        )
        .unwrap();
        assert_eq!(calls, 3);

        gate.on_run_state_changed(true);
        assert!(!gate.retry_limit_reached());
        assert!(gate.should_try_on_cycle());
    }

    #[test]
    fn 运行中从普通购物袋切到自动加车会重新布防一次() {
        let mut gate = AutoOpenBag::default();
        gate.on_run_state_changed(true);
        gate.open_if_needed(true, "zh_CN", None, |_| Ok::<_, ()>(()))
            .unwrap();
        assert!(!gate.should_try_on_cycle());
        gate.on_auto_add_enabled();
        assert!(gate.should_try_on_cycle());
        let mut opened = false;
        gate.open_if_needed(true, "zh_CN", Some(("MJYE4CH/A", "R683")), |_| {
            opened = true;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(opened);
        assert!(!gate.should_try_on_cycle());
        gate.on_auto_add_enabled();
        assert!(
            !gate.should_try_on_cycle(),
            "切换开关不得启动第二项购买任务"
        );
        gate.open_if_needed(
            true,
            "zh_CN",
            Some(("MJYE4CH/A", "R683")),
            |_| -> Result<(), ()> { panic!("已启动的任务不得被开关重置") },
        )
        .unwrap();
    }
}
