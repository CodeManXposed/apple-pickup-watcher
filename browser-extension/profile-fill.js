// 运行在 Chrome 的 ISOLATED world。资料只从扩展本地存储读取，不进入 URL、
// 桌面程序配置或页面脚本；填入表单后，Apple 页面自然可以读取这些可见字段。
(() => {
  const STORAGE_KEY = 'apwCheckoutProfile';
  const CHECKOUT_PATH = /\/shop\/checkout(?:\/|$)/;
  const CONTEXT = {
    pickup: /pickup|pick.?up|collection|取货|提货|自提/i,
    delivery: /shipping|delivery|deliver|recipient|收货|配送|送货|寄送/i,
    forbidden: /billing|payment|credit.?card|invoice|账单|付款|支付|银行卡|信用卡|发票/i,
  };
  const FIELD = [
    ['email', /e.?mail|邮箱|电子邮件/i],
    ['phone', /phone|mobile|telephone|tel\b|手机|电话/i],
    ['familyName', /family.?name|last.?name|surname|姓氏|^姓$/i],
    ['givenName', /given.?name|first.?name|名字|^名$/i],
    ['fullName', /full.?name|contact.?name|recipient.?name|\bname\b|姓名|收货人|取货人/i],
    ['postalCode', /postal|post.?code|zip.?code|zipcode|\bzip\b|邮政编码|邮编/i],
    ['district', /district|county|区县|区\/县|区、县/i],
    ['city', /city|城市|市区|^市$/i],
    ['state', /province|state|prefecture|省份|省\/州|^省$|^州$/i],
    ['country', /country|国家/i],
    ['address2', /address.?line.?2|address2|street2|apartment|suite|unit|地址.?2|补充地址/i],
    ['address1', /address.?line.?1|address1|street.?address|street1|street|详细地址|街道地址|地址/i],
  ];
  let profile = null;
  let scheduled = false;
  const filled = new WeakSet();

  function readable(value) {
    return String(value || '').replace(/([a-z])([A-Z])/g, '$1 $2');
  }

  function fieldDescription(field) {
    const labels = Array.from(field.labels || [], (label) => label.textContent || '');
    const labelledBy = (field.getAttribute('aria-labelledby') || '')
      .split(/\s+/)
      .map((id) => document.getElementById(id)?.textContent || '');
    return [
      field.getAttribute('name'), field.id, field.getAttribute('autocomplete'),
      field.getAttribute('aria-label'), field.getAttribute('placeholder'),
      ...labels, ...labelledBy,
    ].map(readable).join(' ');
  }

  function contextOf(text) {
    if (CONTEXT.forbidden.test(text)) return 'forbidden';
    const pickup = CONTEXT.pickup.test(text);
    const delivery = CONTEXT.delivery.test(text);
    if (pickup === delivery) return null;
    return pickup ? 'pickup' : 'delivery';
  }

  function ancestorDescription(node) {
    const heading = Array.from(node.children || []).find((child) =>
      /^(H[1-6]|LEGEND)$/.test(child.tagName));
    return [
      node.id, node.className?.baseVal || node.className,
      node.getAttribute?.('aria-label'), node.getAttribute?.('data-testid'),
      node.getAttribute?.('data-automation-id'), heading?.textContent,
    ].map(readable).join(' ');
  }

  function fieldTarget(field) {
    const description = fieldDescription(field);
    let context = contextOf(description);
    if (context === 'forbidden') return null;
    if (!context) {
      let node = field.parentElement;
      for (let depth = 0; node && depth < 8; depth++, node = node.parentElement) {
        context = contextOf(ancestorDescription(node));
        if (context) break;
      }
    }
    if (!context || context === 'forbidden') return null;
    const key = FIELD.find(([, pattern]) => pattern.test(description))?.[0];
    return key ? { context, key } : null;
  }

  function canFill(field) {
    if (filled.has(field) || field.disabled || field.readOnly || field.value.trim()) return false;
    if (field.getClientRects().length === 0) return false;
    if (field.tagName === 'TEXTAREA') return true;
    if (field.tagName !== 'INPUT') return false;
    if (!['text', 'email', 'tel'].includes(field.type)) return false;
    return true;
  }

  function setValue(field, value) {
    const prototype = field.tagName === 'TEXTAREA'
      ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(prototype, 'value')?.set;
    if (setter) setter.call(field, value);
    else field.value = value;
    filled.add(field);
    field.dispatchEvent(new Event('input', { bubbles: true }));
    field.dispatchEvent(new Event('change', { bubbles: true }));
  }

  function fill() {
    if (!profile?.enabled || !CHECKOUT_PATH.test(location.pathname)) return;
    for (const field of document.querySelectorAll('input, textarea')) {
      if (!canFill(field)) continue;
      const target = fieldTarget(field);
      const value = target && profile[target.context]?.[target.key];
      if (typeof value === 'string' && value.trim()) setValue(field, value.trim());
    }
  }

  function schedule() {
    if (scheduled) return;
    scheduled = true;
    setTimeout(() => { scheduled = false; fill(); }, 120);
  }

  chrome.storage.local.get(STORAGE_KEY).then((result) => {
    profile = result[STORAGE_KEY] || null;
    fill();
    new MutationObserver(schedule).observe(document.documentElement, {
      childList: true, subtree: true, attributes: true,
      attributeFilter: ['class', 'style', 'hidden', 'aria-hidden'],
    });
    window.addEventListener('popstate', schedule);
  });

  chrome.storage.onChanged.addListener((changes, area) => {
    if (area !== 'local' || !changes[STORAGE_KEY]) return;
    profile = changes[STORAGE_KEY].newValue || null;
    schedule();
  });
})();
