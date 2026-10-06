// 以 Chrome tab 为单位保存一次购买流程。session 存储会在扩展重载或浏览器重启时清空。
const FLOW_PREFIX = 'apwCheckoutFlow:';
const PARENT_PREFIX = 'apwCheckoutParent:';
const FLOW_TTL_MS = 60 * 60 * 1000;
const BRIDGE_URL = 'http://127.0.0.1:43849/payment-link';

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
  const parentId = (await chrome.storage.session.get(`${PARENT_PREFIX}${tabId}`))[`${PARENT_PREFIX}${tabId}`];
  if (Number.isInteger(parentId)) {
    const parent = (await chrome.storage.session.get(`${FLOW_PREFIX}${parentId}`))[`${FLOW_PREFIX}${parentId}`];
    if (parent && (!flow || parent.startedAt === flow.startedAt && parent.phase === 'order-submitted')) {
      flow = parent;
    }
  }
  if (!flow) return null;
  if (Date.now() - flow.startedAt <= FLOW_TTL_MS) return flow;
  await chrome.storage.session.remove(key);
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
    const response = await fetch(BRIDGE_URL, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${apwBridgeToken}`,
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({ url: message.url, kind: message.kind }),
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
      const flow = { startedAt: Date.now(), phase: 'bag', storeNumber: message.storeNumber, partNumber: message.partNumber };
      await chrome.storage.session.set({ [key]: flow });
      return flow;
    }
    const flow = await flowFor(tabId);
    if (message?.type === 'apw-get-checkout') return flow;
    if (message?.type === 'apw-checkout-step' && flow && applePage(sender.url)) {
      const next = { bag: 'bag-clicked', 'bag-clicked': 'guest-clicked', 'guest-clicked': 'order-submitted' }[flow.phase];
      if (next !== message.phase) return null;
      const updated = { ...flow, phase: next };
      await chrome.storage.session.set({ [key]: updated });
      return updated;
    }
    if (message?.type === 'apw-payment-link') {
      return forwardPaymentLink(message, sender, flow);
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

chrome.tabs.onRemoved.addListener((tabId) => {
  chrome.storage.session.remove(`${FLOW_PREFIX}${tabId}`).catch(() => {});
  chrome.storage.session.remove(`${PARENT_PREFIX}${tabId}`).catch(() => {});
});
