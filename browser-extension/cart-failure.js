// 加车错误页由 Apple 直接导航到达；隔离环境负责把结果回传给桌面应用。
(() => {
  if (window.top !== window || !/^Page Not Found(?:\s*-|$)/i.test(document.title)) return;
  const query = new URLSearchParams(location.search);
  if (query.get('add-to-cart') !== 'add-to-cart') return;
  let intent;
  try { intent = JSON.parse(sessionStorage.getItem('apwAutoAddIntent') || 'null'); } catch { return; }
  if (!intent || !['submitting', 'failed'].includes(intent.phase) ||
      !/^[A-Z0-9]{4,20}\/[A-Z]$/.test(intent.part) ||
      !/^[A-Z0-9]{2,10}$/.test(intent.storeNumber) ||
      !/^[a-f0-9]{32}$/.test(intent.taskId || '') ||
      Date.now() - (intent.submittedAt || intent.startedAt) > 90000 ||
      query.get('product')?.toUpperCase() !== intent.part) return;
  sessionStorage.removeItem('apwAutoAddIntent');
  void chrome.runtime.sendMessage({
    type: 'apw-cart-failed', partNumber: intent.part,
    storeNumber: intent.storeNumber, taskId: intent.taskId,
  }).catch(() => {});
})();
