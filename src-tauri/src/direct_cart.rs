//! 直接用独立 HTTP 会话提交 Apple 商品页的加车动作。
//! 该端点是官网页面当前使用的 Web 请求，不是 Apple 承诺稳定的公开 API。

use std::sync::Arc;
use std::time::Duration;

use apw_core::config::TaskPlan;
use apw_core::model::{Region, Target};
use reqwest::Url;
use wreq::header::{LOCATION, REFERER};
use wreq::{Client, Response};

use crate::browser_bot::SessionCookie;

pub(crate) enum AddOutcome {
    Confirmed(Vec<SessionCookie>),
    /// 风控阻断或响应不足以确认购物袋状态；本轮停止以免重复加车。
    Stop(String),
}

pub(crate) struct DirectCart {
    client: Client,
    cookies: Arc<wreq::cookie::Jar>,
    base: Url,
    bag: Url,
    product: Url,
    part: String,
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
        || !url
            .path()
            .to_ascii_uppercase()
            .ends_with(&format!("/{}", part.to_ascii_uppercase()))
    {
        return Err("商品 URL 必须是当前地区 Apple 官网且路径末尾与 SKU 一致".into());
    }
    // 商品页 URL 可能来自复制的加车结果或旧任务链接。查询参数和片段
    // 不属于商品身份；保留它们会让预热或会话初始化提前执行加车动作。
    url.set_query(None);
    url.set_fragment(None);
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
    pub(crate) fn new(
        region: &Region,
        target: &Target,
        plan: Option<&TaskPlan>,
    ) -> Result<Self, String> {
        let base = Url::parse(region.base_url).map_err(|_| "地区官网 URL 无效".to_owned())?;
        let product = product_url(region, target, plan)?;
        let bag = Url::parse(&region.bag_url()).map_err(|_| "购物袋 URL 无效".to_owned())?;
        let cookies = Arc::new(wreq::cookie::Jar::default());
        let emulation = wreq::IntoEmulation::into_emulation(wreq_util::Emulation::Chrome149);
        let client = Client::builder()
            .cookie_provider(cookies.clone())
            .timeout(Duration::from_secs(20))
            .redirect(wreq::redirect::Policy::none())
            .tls_options(emulation.tls_options)
            .http1_options(emulation.http1_options)
            .http2_options(emulation.http2_options)
            .user_agent(apw_core::apple::RequestProfile::chrome().user_agent)
            .build()
            .map_err(|err| format!("创建 Apple HTTP 会话失败：{err}"))?;
        Ok(Self {
            client,
            cookies,
            base,
            bag,
            product,
            part: target.part_number.clone(),
        })
    }

    async fn get_following(&self, initial: Url, referer: Option<&Url>) -> Result<Response, String> {
        let mut current = initial;
        for _ in 0..8 {
            let mut request = self.client.get(current.as_str());
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

    pub(crate) async fn add(&self) -> Result<AddOutcome, String> {
        let page = self.get_following(self.product.clone(), None).await?;
        if !page.status().is_success() {
            if matches!(page.status().as_u16(), 403 | 429 | 541) {
                return Ok(AddOutcome::Stop(format!(
                    "Apple 商品页返回 HTTP {}，可能需要验证或等待放行；本轮不再直连重试",
                    page.status()
                )));
            }
            return Err(format!(
                "Apple 商品页返回 HTTP {}，可能需要验证或等待放行",
                page.status()
            ));
        }
        let canonical = match official_url(&page.uri().to_string(), &self.base, &self.part) {
            Ok(url) => url,
            Err(err) => {
                return Ok(AddOutcome::Stop(format!(
                    "{err}；请检查 Apple 等待页，本轮不再直连重试"
                )));
            }
        };
        // 官网商品页中的信标设置 as_atb Cookie；页面脚本把最后一段作为
        // atbtoken 加入表单。HTTP 模式也须先加载信标并使用同一会话。
        let beacon = Url::parse(&format!(
            "{}/shop/beacon/atb",
            self.base.as_str().trim_end_matches('/')
        ))
        .map_err(|_| "Apple 加车信标地址无效")?;
        let beacon_response = self.get_following(beacon, Some(&canonical)).await?;
        if !beacon_response.status().is_success() {
            return Ok(AddOutcome::Stop(format!(
                "Apple 加车信标返回 HTTP {}；本轮不再直连重试",
                beacon_response.status()
            )));
        }
        let Some(token) = atb_token(&self.cookies, &canonical) else {
            return Ok(AddOutcome::Stop(
                "Apple 未提供加车验证令牌；本轮不再直连重试".into(),
            ));
        };
        let request = add_url(&canonical, &self.part, &token);
        let response = self.get_following(request, Some(&canonical)).await?;
        if !response.status().is_success() {
            if matches!(response.status().as_u16(), 403 | 429 | 541) {
                return Ok(AddOutcome::Stop(format!(
                    "Apple 加车请求返回 HTTP {}，商品未确认进入购物袋；本轮不再直连重试",
                    response.status()
                )));
            }
            return Err(format!(
                "Apple 加车请求返回 HTTP {}；商品尚未确认进入购物袋",
                response.status()
            ));
        }
        let final_url =
            Url::parse(&response.uri().to_string()).map_err(|_| "Apple 加车响应地址无效")?;
        if final_url.host_str() != self.base.host_str() || !accepted(&final_url, &self.part) {
            return Ok(AddOutcome::Stop(
                "Apple 未返回目标 SKU 的加车成功页；请手动检查购物袋，本轮不会再次加车".into(),
            ));
        }
        let host = self.bag.host_str().ok_or("Apple 购物袋缺少域名")?;
        let cookies = self
            .cookies
            .matches(self.bag.as_str())
            .map(|cookie| SessionCookie {
                name: cookie.name().to_owned(),
                value: cookie.value().to_owned(),
                domain: cookie.domain().unwrap_or(host).to_owned(),
                path: cookie.path().unwrap_or("/").to_owned(),
                http_only: cookie.http_only(),
                secure: cookie.secure(),
            })
            .collect::<Vec<_>>();
        Ok(AddOutcome::Confirmed(cookies))
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
    }

    #[test]
    fn request_and_success_require_exact_sku() {
        let url = Url::parse("https://www.apple.com.cn/shop/buy-iphone/x/mjye4ch/a").unwrap();
        let add = add_url(&url, "MJYE4CH/A", "abc123");
        assert_eq!(
            add.query(),
            Some(
                "product=MJYE4CH%2FA&purchaseOption=fullPrice&step=select&atbtoken=abc123&add-to-cart=add-to-cart"
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
}
