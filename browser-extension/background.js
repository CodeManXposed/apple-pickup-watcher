// 以 Chrome tab 为单位保存一次购买流程。session 存储会在扩展重载或浏览器重启时清空。
const FLOW_PREFIX = 'apwCheckoutFlow:';
const PARENT_PREFIX = 'apwCheckoutParent:';
const CHECKOUT_FLOW_TTL_MS = 4 * 60 * 60 * 1000;
const PAYMENT_FLOW_TTL_MS = 60 * 60 * 1000;
const BRIDGE_URL = 'http://127.0.0.1:43849/payment-link';
const STATUS_URL = 'http://127.0.0.1:43849/task-status';
const TASK_CHECK_URL = 'http://127.0.0.1:43849/task-check';

function applePage(url) {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname;
    return parsed.protocol === 'https:' && (
      host === 'www.apple.com' || host === 'www.apple.com.cn' ||
      host.endsWith('.www.apple.com') || host.endsWith('.www.apple.com.cn') ||
      host.endsWith('.store.apple.com') || host.endsWith('.store.apple.com.cn')
    );
  } catch {
    return false;
  }
}

async function flowFor(tabId) {
  const key = `${FLOW_PREFIX}${tabId}`;
  let flow = (await chrome.storage.session.get(key))[key];
  let currentId = tabId;
  const visited = new Set([tabId]);
  // Apple 可能先打开商店中转标签，再由它打开支付宝。每一跳都只能继承
  // 同一次任务；订单阶段以仍可找到的上游最新状态为准。
  for (let depth = 0; depth < 8; depth++) {
    const parentId = (await chrome.storage.session.get(`${PARENT_PREFIX}${currentId}`))[`${PARENT_PREFIX}${currentId}`];
    if (!Number.isInteger(parentId) || visited.has(parentId)) break;
    visited.add(parentId);
    const parent = (await chrome.storage.session.get(`${FLOW_PREFIX}${parentId}`))[`${FLOW_PREFIX}${parentId}`];
    if (parent && (!flow || parent.startedAt === flow.startedAt && parent.phase === 'order-submitted')) {
      flow = parent;
    }
    currentId = parentId;
  }
  if (!flow) return null;
  const expiresFrom = flow.phase === 'order-submitted'
    ? flow.orderSubmittedAt ?? flow.startedAt : flow.startedAt;
  const ttl = flow.phase === 'order-submitted'
    ? PAYMENT_FLOW_TTL_MS : CHECKOUT_FLOW_TTL_MS;
  if (Number.isFinite(expiresFrom) && Date.now() - expiresFrom <= ttl) return flow;
  await chrome.storage.session.remove(key);
  await chrome.storage.session.remove(`${PARENT_PREFIX}${tabId}`);
  return null;
}

function alipayPage(url) {
  try {
    const parsed = new URL(url);
    return parsed.protocol === 'https:' &&
      (parsed.hostname === 'alipay.com' || parsed.hostname.endsWith('.alipay.com'));
  } catch {
    return false;
  }
}

async function forwardPaymentLink(message, sender, flow) {
  if (!flow || flow.phase !== 'order-submitted' || !alipayPage(sender.url)) {
    return { ok: false, reason: 'no-active-checkout' };
  }
  if (!['qr', 'cashier'].includes(message.kind) || !alipayPage(message.url)) {
    return { ok: false, reason: 'invalid-link' };
  }
  if (message.kind === 'cashier' && message.url !== sender.url) {
    return { ok: false, reason: 'invalid-link' };
  }
  const { apwBridgeToken } = await chrome.storage.local.get('apwBridgeToken');
  if (!/^[a-f0-9]{32}$/i.test(apwBridgeToken || '')) {
    return { ok: false, reason: 'missing-pairing-code' };
  }
  try {
    // 页面跳转很快时，订单阶段的首次回传可能超时；在付款链接前再同步一次。
    await forwardStage('orderSubmitted', sender, flow);
    const response = await fetch(BRIDGE_URL, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apwBridgeToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        url: message.url, kind: message.kind,
        partNumber: flow.partNumber, storeNumber: flow.storeNumber, taskId: flow.taskId,
      }),
      signal: AbortSignal.timeout(5000),
    });
    return response.ok ? { ok: true } : { ok: false, reason: `bridge-http-${response.status}` };
  } catch {
    return { ok: false, reason: 'bridge-unreachable' };
  }
}

async function forwardStage(stage, sender, flow) {
  if (!flow || !(applePage(sender.url) ||
    (stage === 'orderSubmitted' && flow.phase === 'order-submitted' && alipayPage(sender.url)))) {
    return { ok: false, reason: 'no-active-checkout' };
  }
  const allowed = {
    bag: ['bag'],
    'bag-clicked': ['checkout'],
    'guest-clicked': ['guest', 'privacy', 'profile', 'profileAction', 'payment', 'review'],
    'order-submitted': ['orderSubmitted'],
  };
  if (!allowed[flow.phase]?.includes(stage)) return { ok: false, reason: 'invalid-stage' };
  if (!/^[a-f0-9]{32}$/.test(flow.taskId || '')) return { ok: false, reason: 'missing-task-id' };
  const { apwBridgeToken } = await chrome.storage.local.get('apwBridgeToken');
  if (!/^[a-f0-9]{32}$/i.test(apwBridgeToken || '')) {
    return { ok: false, reason: 'missing-pairing-code' };
  }
  try {
    const response = await fetch(STATUS_URL, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apwBridgeToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ partNumber: flow.partNumber, storeNumber: flow.storeNumber, taskId: flow.taskId, stage }),
      signal: AbortSignal.timeout(1000),
    });
    return response.ok ? { ok: true } : { ok: false, reason: `bridge-http-${response.status}` };
  } catch {
    return { ok: false, reason: 'bridge-unreachable' };
  }
}

async function activeTask(sender, flow) {
  if (!flow || !applePage(sender.url) ||
      !/^[a-f0-9]{32}$/.test(flow.taskId || '')) return false;
  const { apwBridgeToken } = await chrome.storage.local.get('apwBridgeToken');
  if (!/^[a-f0-9]{32}$/i.test(apwBridgeToken || '')) return false;
  try {
    const response = await fetch(TASK_CHECK_URL, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apwBridgeToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        partNumber: flow.partNumber, storeNumber: flow.storeNumber, taskId: flow.taskId,
      }),
      signal: AbortSignal.timeout(5000),
    });
    return response.ok;
  } catch {
    return false;
  }
}

async function forwardCartFailure(message, sender) {
  const page = new URL(sender.url);
  if (!applePage(sender.url) || !page.pathname.includes('/shop/buy-') ||
      page.searchParams.get('add-to-cart') !== 'add-to-cart' ||
      page.searchParams.get('product')?.toUpperCase() !== message.partNumber ||
      !/^[A-Z0-9]{4,20}\/[A-Z]$/.test(message.partNumber || '') ||
      !/^[A-Z0-9]{2,10}$/.test(message.storeNumber || '') ||
      !/^[a-f0-9]{32}$/.test(message.taskId || '')) {
    return { ok: false, reason: 'invalid-cart-failure' };
  }
  return postCartFailure(message);
}

async function forwardCartTimeout(message, sender) {
  const page = new URL(sender.url);
  if (!applePage(sender.url) || !page.pathname.includes('/shop/') ||
      !Number.isInteger(sender.tab?.id) ||
      !/^[A-Z0-9]{4,20}\/[A-Z]$/.test(message.partNumber || '') ||
      !/^[A-Z0-9]{2,10}$/.test(message.storeNumber || '') ||
      !/^[a-f0-9]{32}$/.test(message.taskId || '')) {
    return { ok: false, reason: 'invalid-cart-timeout' };
  }
  if (!await activeTask(sender, message)) return { ok: false, reason: 'inactive-task' };
  return postCartFailure(message);
}

async function postCartFailure(message) {
  const { apwBridgeToken } = await chrome.storage.local.get('apwBridgeToken');
  if (!/^[a-f0-9]{32}$/i.test(apwBridgeToken || '')) {
    return { ok: false, reason: 'missing-pairing-code' };
  }
  try {
    const response = await fetch(STATUS_URL, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apwBridgeToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        partNumber: message.partNumber, storeNumber: message.storeNumber,
        taskId: message.taskId, stage: 'failed',
      }),
      signal: AbortSignal.timeout(5000),
    });
    return response.ok ? { ok: true } : { ok: false, reason: `bridge-http-${response.status}` };
  } catch {
    return { ok: false, reason: 'bridge-unreachable' };
  }
}

chrome.runtime.onMessage.addListener((message, sender, respond) => {
  if ((!applePage(sender.url) && !alipayPage(sender.url)) || !Number.isInteger(sender.tab?.id)) return false;
  const tabId = sender.tab.id;
  const key = `${FLOW_PREFIX}${tabId}`;
  (async () => {
    if (message?.type === 'apw-begin-checkout' && applePage(sender.url)) {
      if (!/\/shop\/bag\/?$/.test(new URL(sender.url).pathname)) return null;
      if (!/^[A-Z0-9]{2,10}$/.test(message.storeNumber || '')) return null;
      if (!/^[A-Z0-9]{4,20}\/[A-Z]$/.test(message.partNumber || '')) return null;
      if (message.taskId && !/^[a-f0-9]{32}$/.test(message.taskId)) return null;
      if (message.productName && (typeof message.productName !== 'string' ||
          message.productName.length > 160 || /[\u0000-\u001f\u007f]/.test(message.productName))) return null;
      const area = message.area;
      if (area && Object.values(area).some((value) => value !== null &&
          (typeof value !== 'string' || value.length > 30 || !/^[\p{Script=Han}A-Za-z\s-]+$/u.test(value)))) return null;
      const flow = { startedAt: Date.now(), phase: 'bag', storeNumber: message.storeNumber, partNumber: message.partNumber, taskId: message.taskId || null, productName: message.productName || null, area };
      if (!await activeTask(sender, flow)) return null;
      await chrome.storage.session.set({ [key]: flow });
      return flow;
    }
    if (message?.type === 'apw-cart-failed') {
      return forwardCartFailure(message, sender);
    }
    if (message?.type === 'apw-cart-timeout') {
      return forwardCartTimeout(message, sender);
    }
    if (message?.type === 'apw-cart-active' && applePage(sender.url)) {
      const path = new URL(sender.url).pathname;
      if (!(/\/shop\/pdpAddToBag\//.test(path) || /\/shop\/buy-[^/]+\//.test(path)) ||
          !/^[A-Z0-9]{4,20}\/[A-Z]$/.test(message.partNumber || '') ||
          !/^[A-Z0-9]{2,10}$/.test(message.storeNumber || '') ||
          !/^[a-f0-9]{32}$/.test(message.taskId || '')) return false;
      return activeTask(sender, message);
    }
    const flow = await flowFor(tabId);
    if (message?.type === 'apw-get-checkout') return flow;
    if (message?.type === 'apw-active-task') {
      return ['bag', 'bag-clicked', 'guest-clicked'].includes(flow?.phase) && await activeTask(sender, flow);
    }
    if (message?.type === 'apw-confirm-store' && flow && applePage(sender.url) &&
        flow.phase === 'guest-clicked' && message.storeNumber === flow.storeNumber &&
        /\/shop\/checkout(?:\/|$)/.test(new URL(sender.url).pathname)) {
      const updated = { ...flow, pickupConfirmed: true };
      await chrome.storage.session.set({ [key]: updated });
      return updated;
    }
    if (message?.type === 'apw-confirm-alipay' && flow && applePage(sender.url) &&
        flow.phase === 'guest-clicked' && flow.pickupConfirmed &&
        /\/shop\/checkout(?:\/|$)/.test(new URL(sender.url).pathname)) {
      const updated = { ...flow, alipayConfirmed: true };
      await chrome.storage.session.set({ [key]: updated });
      return updated;
    }
    if (message?.type === 'apw-checkout-step' && flow && applePage(sender.url)) {
      const next = { bag: 'bag-clicked', 'bag-clicked': 'guest-clicked', 'guest-clicked': 'order-submitted' }[flow.phase];
      if (next !== message.phase) return null;
      const updated = { ...flow, phase: next,
        ...(next === 'order-submitted' ? { orderSubmittedAt: Date.now() } : {}) };
      await chrome.storage.session.set({ [key]: updated });
      return updated;
    }
    if (message?.type === 'apw-payment-link') {
      return forwardPaymentLink(message, sender, flow);
    }
    if (message?.type === 'apw-task-stage') {
      return forwardStage(message.stage, sender, flow);
    }
    return null;
  })().then(respond, () => respond(null));
  return true;
});

chrome.tabs.onCreated.addListener((tab) => {
  if (!Number.isInteger(tab.id) || !Number.isInteger(tab.openerTabId)) return;
  chrome.storage.session.set({ [`${PARENT_PREFIX}${tab.id}`]: tab.openerTabId }).catch(() => {});
  flowFor(tab.openerTabId).then(async (flow) => {
    if (flow) await chrome.storage.session.set({ [`${FLOW_PREFIX}${tab.id}`]: flow });
  }).catch(() => {});
});

// Apple 可能用 noopener 打开新标签，此时没有 openerTabId；付款前也可能
// 先经过 Apple 商店中转页。导航事件仍带来源标签页，只关联本次结账发起的导航。
chrome.webNavigation.onCreatedNavigationTarget.addListener((details) => {
  if (!Number.isInteger(details.tabId) || !Number.isInteger(details.sourceTabId) ||
      !(alipayPage(details.url) || applePage(details.url) &&
        new URL(details.url).pathname.startsWith('/shop/'))) return;
  flowFor(details.sourceTabId).then(async (flow) => {
    if (!flow || !['guest-clicked', 'order-submitted'].includes(flow.phase)) return;
    await chrome.storage.session.set({
      [`${PARENT_PREFIX}${details.tabId}`]: details.sourceTabId,
      [`${FLOW_PREFIX}${details.tabId}`]: flow,
    });
  }).catch(() => {});
});

chrome.tabs.onRemoved.addListener((tabId) => {
  // 付款标签可能比来源标签更晚完成任务阶段同步。保留已进入访客结账的
  // 来源身份，供付款页在订单提交后的一小时内追溯；其他阶段立即清理。
  flowFor(tabId).then(async (flow) => {
    if (flow && ['guest-clicked', 'order-submitted'].includes(flow.phase)) return;
    await chrome.storage.session.remove(`${FLOW_PREFIX}${tabId}`);
    await chrome.storage.session.remove(`${PARENT_PREFIX}${tabId}`);
  }).catch(() => {});
});
