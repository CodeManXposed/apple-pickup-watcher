//! 直接用独立 HTTP 会话提交 Apple 商品页的加车动作。
//! 该端点是官网页面当前使用的 Web 请求，不是 Apple 承诺稳定的公开 API。

use std::sync::Arc;
use std::time::Duration;

use apw_core::apple::RequestProfile;
use apw_core::config::TaskPlan;
use apw_core::model::{Region, Target};
use reqwest::Url;
use wreq::header::{LOCATION, REFERER};
use wreq::{Client, Response};

use crate::browser_bot::SessionCookie;

pub(crate) enum AddOutcome {
    Confirmed(Vec<SessionCookie>),
    Cancelled,
    /// 风控阻断或响应不足以确认购物袋状态；本轮停止以免重复加车。
    Stop {
        reason: String,
        cookies: Vec<SessionCookie>,
    },
    /// 请求可能已被 Apple 执行；交出同一会话供人工核对，绝不自动重试或结账。
    Uncertain {
        reason: String,
        cookies: Vec<SessionCookie>,
    },
}

pub(crate) struct DirectCart {
    client: Client,
    cookies: Arc<wreq::cookie::Jar>,
    profile: RequestProfile,
    accept_language: &'static str,
    base: Url,
    bag: Url,
    product: Url,
    part: String,
}

#[derive(Clone, Copy)]
enum RequestKind {
    Product,
    Beacon,
    Add,
}

fn official_url(input: &str, base: &Url, part: &str) -> Result<Url, String> {
    let mut url = Url::parse(input).map_err(|_| "Apple 商品 URL 无效".to_owned())?;
    if url.scheme() != "https"
        || url.host_str() != base.host_str()
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !url
            .path()
            .starts_with(&format!("{}/shop/", base.path().trim_end_matches('/')))
    {
        return Err("商品 URL 必须是当前地区 Apple 官网商店页面".into());
    }
    // 商品页 URL 可能来自复制的加车结果或旧任务链接。查询参数和片段
    // 不属于商品身份；保留它们会让预热或会话初始化提前执行加车动作。
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_owned();
    if !path
        .to_ascii_uppercase()
        .ends_with(&format!("/{}", part.to_ascii_uppercase()))
    {
        // 官网机型页没有 SKU：例如 /shop/buy-iphone/iphone-18-pro。
        // 只接受机型页这一层，避免把别的 SKU 页面当作机型页继续拼接。
        let shop_prefix = format!("{}/shop/", base.path().trim_end_matches('/'));
        let segments: Vec<_> = path
            .strip_prefix(&shop_prefix)
            .unwrap_or_default()
            .split('/')
            .collect();
        if segments.len() != 2
            || !segments[0].starts_with("buy-")
            || segments[1].is_empty()
            || !segments[1]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err("商品 URL 须为目标 SKU 页面或 Apple 官网机型页".into());
        }
        url.set_path(&format!("{path}/{}", part.to_ascii_uppercase()));
    }
    Ok(url)
}

pub(crate) fn product_url(
    region: &Region,
    target: &Target,
    plan: Option<&TaskPlan>,
) -> Result<Url, String> {
    let base = Url::parse(region.base_url).map_err(|_| "地区官网 URL 无效".to_owned())?;
    let initial = plan
        .and_then(|plan| plan.product_url.as_deref())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| region.product_page_url(&target.part_number))
        .ok_or("SKU 无效，无法生成 Apple 商品 URL")?;
    official_url(&initial, &base, &target.part_number)
}

fn add_url(product: &Url, part: &str, token: &str) -> Url {
    let mut url = product.clone();
    url.set_fragment(None);
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("product", part)
        .append_pair("purchaseOption", "fullPrice")
        .append_pair("step", "select")
        .append_pair("atbtoken", token)
        // 官网在选择“不折抵换购”后会把此标志持久化为隐藏字段。
        .append_pair("igt", "true")
        .append_pair("add-to-cart", "add-to-cart");
    url
}

fn atb_token(cookies: &wreq::cookie::Jar, product: &Url) -> Option<String> {
    cookies
        .matches(product.as_str())
        .find(|cookie| cookie.name() == "as_atb")
        .and_then(|cookie| cookie.value().split('|').next_back().map(str::to_owned))
        .filter(|token| !token.is_empty())
}

fn accepted(url: &Url, part: &str) -> bool {
    let mut step = false;
    let mut product = false;
    for (key, value) in url.query_pairs() {
        if key == "step" && value == "attach" {
            step = true;
        }
        if key == "product" && value.eq_ignore_ascii_case(part) {
            product = true;
        }
    }
    step && product && url.path().contains("/shop/")
}

impl DirectCart {
    pub(crate) fn product_page(&self) -> &Url {
        &self.product
    }

    fn session_cookies(&self, page: Option<&Url>) -> Vec<SessionCookie> {
        let Some(host) = self.bag.host_str() else {
            return Vec::new();
        };
        // 等待页的 Cookie 可能只覆盖商品路径，不能只取购物袋可见的 Cookie。
        let mut selected: Vec<SessionCookie> = Vec::new();
        for url in [Some(&self.bag), Some(&self.product), page]
            .into_iter()
            .flatten()
        {
            for cookie in self.cookies.matches(url.as_str()) {
                let next = SessionCookie {
                    name: cookie.name().to_owned(),
                    value: cookie.value().to_owned(),
                    domain: cookie.domain().unwrap_or(host).to_owned(),
                    path: cookie.path().unwrap_or("/").to_owned(),
                    http_only: cookie.http_only(),
                    secure: cookie.secure(),
                };
                if !selected.iter().any(|existing| {
                    existing.name == next.name
                        && existing.domain == next.domain
                        && existing.path == next.path
                }) {
                    selected.push(next);
                }
            }
        }
        selected
    }

    fn stop(&self, reason: String, page: Option<&Url>) -> AddOutcome {
        AddOutcome::Stop {
            reason,
            cookies: self.session_cookies(page),
        }
    }

    fn uncertain(&self, reason: String, page: Option<&Url>) -> AddOutcome {
        AddOutcome::Uncertain {
            reason,
            cookies: self.session_cookies(page),
        }
    }

    pub(crate) fn new(
        region: &Region,
        target: &Target,
        plan: Option<&TaskPlan>,
    ) -> Result<Self, String> {
        let base = Url::parse(region.base_url).map_err(|_| "地区官网 URL 无效".to_owned())?;
        let product = product_url(region, target, plan)?;
        let bag = Url::parse(&region.bag_url()).map_err(|_| "购物袋 URL 无效".to_owned())?;
        let cookies = Arc::new(wreq::cookie::Jar::default());
        let profile = RequestProfile::chrome();
        let emulation = wreq::IntoEmulation::into_emulation(wreq_util::Emulation::Chrome149);
        let client = Client::builder()
            .cookie_provider(cookies.clone())
            .timeout(Duration::from_secs(20))
            .redirect(wreq::redirect::Policy::none())
            .tls_options(emulation.tls_options)
            .http1_options(emulation.http1_options)
            .http2_options(emulation.http2_options)
            .user_agent(&profile.user_agent)
            .build()
            .map_err(|err| format!("创建 Apple HTTP 会话失败：{err}"))?;
        Ok(Self {
            client,
            cookies,
            profile,
            accept_language: region.accept_language(),
            base,
            bag,
            product,
            part: target.part_number.clone(),
        })
    }

    async fn get_following(
        &self,
        initial: Url,
        referer: Option<&Url>,
        kind: RequestKind,
    ) -> Result<Response, String> {
        let mut current = initial;
        for _ in 0..8 {
            let mut request = self.client.get(current.as_str());
            if let Some(hints) = &self.profile.client_hints {
                request = request
                    .header("sec-ch-ua", hints.ua.as_str())
                    .header("sec-ch-ua-mobile", hints.mobile.as_str())
                    .header("sec-ch-ua-platform", hints.platform.as_str());
            }
            request = request.header("accept-language", self.accept_language);
            match kind {
                RequestKind::Beacon => {
                    request = request
                        .header(
                            "accept",
                            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
                        )
                        .header("sec-fetch-site", "same-origin")
                        .header("sec-fetch-mode", "no-cors")
                        .header("sec-fetch-dest", "image");
                }
                RequestKind::Product | RequestKind::Add => {
                    request = request
                        .header("upgrade-insecure-requests", "1")
                        .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7")
                        .header("sec-fetch-site", if referer.is_some() { "same-origin" } else { "none" })
                        .header("sec-fetch-mode", "navigate")
                        .header("sec-fetch-user", "?1")
                        .header("sec-fetch-dest", "document");
                }
            }
            if let Some(referer) = referer {
                request = request.header(REFERER, referer.as_str());
            }
            let response = request
                .send()
                .await
                .map_err(|err| format!("Apple 请求失败：{err}"))?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let location = response
                .headers()
                .get(LOCATION)
                .ok_or("Apple 重定向缺少目标地址")?
                .to_str()
                .map_err(|_| "Apple 重定向地址无效")?;
            current = current.join(location).map_err(|_| "Apple 重定向地址无效")?;
            if current.scheme() != "https"
                || current.host_str() != self.base.host_str()
                || !current.path().contains("/shop/")
            {
                return Err("Apple 加车请求跳转到了非本地区商店页面".into());
            }
        }
        Err("Apple 商品页跳转次数过多".into())
    }

    pub(crate) async fn add(
        &self,
        is_live: impl Fn() -> bool + Send + Sync,
    ) -> Result<AddOutcome, String> {
        if !is_live() {
            return Ok(AddOutcome::Cancelled);
        }
        let page = self
            .get_following(self.product.clone(), None, RequestKind::Product)
            .await?;
        let page_url = Url::parse(&page.uri().to_string()).ok();
        if !page.status().is_success() {
            if matches!(page.status().as_u16(), 403 | 429 | 541) {
                return Ok(self.stop(
                    format!(
                        "Apple 商品页返回 HTTP {}，可能需要验证或等待放行；本轮不再直连重试",
                        page.status().as_u16()
                    ),
                    page_url.as_ref(),
                ));
            }
            return Err(format!(
                "Apple 商品页返回 HTTP {}，可能需要验证或等待放行",
                page.status().as_u16()
            ));
        }
        let canonical = match official_url(&page.uri().to_string(), &self.base, &self.part) {
            Ok(url) => url,
            Err(err) => {
                return Ok(self.stop(
                    format!("{err}；请检查 Apple 等待页，本轮不再直连重试"),
                    page_url.as_ref(),
                ));
            }
        };
        // 官网商品页中的信标设置 as_atb Cookie；页面脚本把最后一段作为
        // atbtoken 加入表单。HTTP 模式也须先加载信标并使用同一会话。
        let beacon = Url::parse(&format!(
            "{}/shop/beacon/atb",
            self.base.as_str().trim_end_matches('/')
        ))
        .map_err(|_| "Apple 加车信标地址无效")?;
        if !is_live() {
            return Ok(AddOutcome::Cancelled);
        }
        let beacon_response = self
            .get_following(beacon, Some(&canonical), RequestKind::Beacon)
            .await?;
        if !beacon_response.status().is_success() {
            return Ok(self.stop(
                format!(
                    "Apple 加车信标返回 HTTP {}；本轮不再直连重试",
                    beacon_response.status().as_u16()
                ),
                Some(&canonical),
            ));
        }
        let Some(token) = atb_token(&self.cookies, &canonical) else {
            return Ok(self.stop(
                "Apple 未提供加车验证令牌；本轮不再直连重试".into(),
                Some(&canonical),
            ));
        };
        if !is_live() {
            return Ok(AddOutcome::Cancelled);
        }
        let request = add_url(&canonical, &self.part, &token);
        // 此请求会修改购物袋。只要开始发送，网络断开、重定向失败或服务器错误
        // 都无法证明 Apple 没有完成加车；不能交给任务调度器再次提交。
        let response = match self
            .get_following(request, Some(&canonical), RequestKind::Add)
            .await
        {
            Ok(response) => response,
            Err(_) => {
                return Ok(self.uncertain(
                    "Apple 加车请求结果不确定（连接或重定向失败）".into(),
                    Some(&canonical),
                ));
            }
        };
        if !response.status().is_success() {
            return Ok(self.uncertain(
                format!(
                    "Apple 加车请求返回 HTTP {}，商品未确认进入购物袋",
                    response.status().as_u16()
                ),
                Some(&canonical),
            ));
        }
        let final_url = match Url::parse(&response.uri().to_string()) {
            Ok(url) => url,
            Err(_) => {
                return Ok(self.uncertain("Apple 加车响应地址无法识别".into(), Some(&canonical)));
            }
        };
        if final_url.host_str() != self.base.host_str() || !accepted(&final_url, &self.part) {
            return Ok(self.uncertain("Apple 未返回目标 SKU 的加车成功页".into(), Some(&canonical)));
        }
        Ok(AddOutcome::Confirmed(
            self.session_cookies(Some(&canonical)),
        ))
    }

    pub(crate) fn seed_browser_cookies(&self, cookies: &[SessionCookie]) {
        for cookie in cookies {
            if cookie.name.contains(';') || cookie.name.contains('=') || cookie.value.contains(';')
            {
                continue;
            }
            let mut header = format!(
                "{}={}; Domain={}; Path={}",
                cookie.name, cookie.value, cookie.domain, cookie.path
            );
            if cookie.secure {
                header.push_str("; Secure");
            }
            if cookie.http_only {
                header.push_str("; HttpOnly");
            }
            self.cookies.add(header, self.product.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apw_core::config::{AddMethod, TaskMode};
    use apw_core::model::region_by_locale;

    #[test]
    fn official_product_link_is_bound_to_region_and_sku() {
        let cn = region_by_locale("zh_CN").unwrap();
        let base = Url::parse(cn.base_url).unwrap();
        assert!(
            official_url(
                "https://www.apple.com.cn/shop/pdpAddToBag/MJYE4CH/A",
                &base,
                "MJYE4CH/A"
            )
            .is_ok()
        );
        assert!(
            official_url(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/mjye4ch/a",
                &base,
                "MJYE4CH/A"
            )
            .is_ok()
        );
        assert_eq!(
            official_url(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/?cid=example#details",
                &base,
                "MJYE4CH/A"
            )
            .unwrap()
            .as_str(),
            "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A"
        );
        assert!(
            official_url(
                "https://evil.example/shop/pdpAddToBag/MJYE4CH/A",
                &base,
                "MJYE4CH/A"
            )
            .is_err()
        );
        assert!(
            official_url(
                "https://user@www.apple.com.cn/shop/pdpAddToBag/MJYE4CH/A",
                &base,
                "MJYE4CH/A"
            )
            .is_err()
        );
        assert!(
            official_url(
                "https://www.apple.com.cn/shop/pdpAddToBag/OTHERCH/A",
                &base,
                "MJYE4CH/A"
            )
            .is_err()
        );
        assert!(
            official_url(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/OTHERCH/A",
                &base,
                "MJYE4CH/A"
            )
            .is_err()
        );
        assert_eq!(
            official_url(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A?product=MJYE4CH%2FA&add-to-cart=add-to-cart#apw-auto-add=MJYE4CH%2FA",
                &base,
                "MJYE4CH/A"
            )
            .unwrap()
            .as_str(),
            "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A"
        );
    }

    #[test]
    fn configured_product_url_is_used_for_api_session_and_prewarm() {
        let cn = region_by_locale("zh_CN").unwrap();
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
            mode: TaskMode::Release,
            add_method: AddMethod::Api,
            product_url: Some(
                "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A".into(),
            ),
            release_at_ms: Some(1_000_000),
            retry_delay_ms: 3_000,
        };
        assert_eq!(
            product_url(cn, &target, Some(&plan)).unwrap().as_str(),
            plan.product_url.as_deref().unwrap()
        );
        let mut other = plan.clone();
        other.product_url = Some("https://evil.example/shop/buy-iphone/MJYE4CH/A".into());
        assert!(product_url(cn, &target, Some(&other)).is_err());
        other.product_url = Some("https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro".into());
        assert_eq!(
            product_url(cn, &target, Some(&other)).unwrap().as_str(),
            "https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A"
        );
    }

    #[test]
    fn request_and_success_require_exact_sku() {
        let url = Url::parse("https://www.apple.com.cn/shop/buy-iphone/x/mjye4ch/a").unwrap();
        let add = add_url(&url, "MJYE4CH/A", "abc123");
        assert_eq!(
            add.query(),
            Some(
                "product=MJYE4CH%2FA&purchaseOption=fullPrice&step=select&atbtoken=abc123&igt=true&add-to-cart=add-to-cart"
            )
        );
        assert!(accepted(
            &Url::parse(
                "https://www.apple.com.cn/shop/buy-iphone/x?step=attach&product=MJYE4CH%2FA"
            )
            .unwrap(),
            "MJYE4CH/A"
        ));
        assert!(!accepted(
            &Url::parse(
                "https://www.apple.com.cn/shop/buy-iphone/x?step=attach&product=OTHERCH%2FA"
            )
            .unwrap(),
            "MJYE4CH/A"
        ));
    }

    #[test]
    fn atb_token_comes_from_the_last_cookie_segment() {
        let jar = wreq::cookie::Jar::default();
        let page = Url::parse("https://www.apple.com.cn/shop/buy-iphone/test").unwrap();
        assert!(atb_token(&jar, &page).is_none());
        jar.add("as_atb=prefix|abc123; Path=/; Secure", page.as_str());
        assert_eq!(atb_token(&jar, &page).as_deref(), Some("abc123"));
    }

    #[test]
    fn uncertain_add_keeps_the_http_session_for_browser_inspection() {
        let cn = region_by_locale("zh_CN").unwrap();
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let session = DirectCart::new(cn, &target, None).unwrap();
        session.seed_browser_cookies(&[SessionCookie {
            name: "test_session".into(),
            value: "test-value".into(),
            domain: "www.apple.com.cn".into(),
            path: "/".into(),
            http_only: true,
            secure: true,
        }]);
        let AddOutcome::Uncertain { reason, cookies } = session.uncertain("结果不明".into(), None)
        else {
            panic!("结果不明必须保留浏览器核对用的会话");
        };
        assert_eq!(reason, "结果不明");
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "test_session");
        assert_eq!(cookies[0].value, "test-value");
        assert!(cookies[0].secure && cookies[0].http_only);
    }

    #[test]
    fn pre_add_stop_preserves_product_scoped_cookies_for_browser_verification() {
        let cn = region_by_locale("zh_CN").unwrap();
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let session = DirectCart::new(cn, &target, None).unwrap();
        let page =
            Url::parse("https://www.apple.com.cn/shop/buy-iphone/iphone-18-pro/MJYE4CH/A").unwrap();
        session.cookies.add(
            "wait_room=ready; Domain=www.apple.com.cn; Path=/shop/buy-iphone; Secure",
            page.as_str(),
        );
        session.cookies.add(
            "other=skip; Domain=www.apple.com.cn; Path=/shop/buy-mac; Secure",
            "https://www.apple.com.cn/shop/buy-mac/example",
        );
        let AddOutcome::Stop { reason, cookies } = session.stop("等待验证".into(), Some(&page))
        else {
            panic!("加车前停止时必须保留商品页会话");
        };
        assert_eq!(reason, "等待验证");
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "wait_room");
        assert_eq!(cookies[0].path, "/shop/buy-iphone");
    }

    #[tokio::test]
    async fn stopped_task_does_not_start_an_http_cart_session() {
        let cn = region_by_locale("zh_CN").unwrap();
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: "MJYE4CH/A".into(),
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let session = DirectCart::new(cn, &target, None).unwrap();
        assert!(matches!(
            session.add(|| false).await.unwrap(),
            AddOutcome::Cancelled
        ));
    }

    #[tokio::test]
    #[ignore = "sends one real Apple add-to-bag request in a disposable HTTP session"]
    async fn live_direct_add_diagnostic() {
        let part = std::env::var("APW_LIVE_ADD_PART").expect("set APW_LIVE_ADD_PART to a SKU");
        let cn = region_by_locale("zh_CN").unwrap();
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "测试门店".into(),
            part_number: part,
            product_name: "测试商品".into(),
            companion_part: None,
            pickup_location: None,
        };
        let session = DirectCart::new(cn, &target, None).unwrap();
        match session.add(|| true).await.unwrap() {
            AddOutcome::Confirmed(_) => println!("Apple confirmed the add URL"),
            AddOutcome::Cancelled => panic!("live diagnostic unexpectedly cancelled"),
            AddOutcome::Stop { reason, .. } => panic!("Apple stopped before add: {reason}"),
            AddOutcome::Uncertain { reason, .. } => panic!("Apple did not confirm add: {reason}"),
        }
    }
}
