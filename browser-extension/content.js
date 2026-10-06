// 仅处理桌面程序生成的片段。脚本在 MAIN world 运行，才能使用 Apple 页面
// 自己的 acStore 和当前浏览器购物袋会话；不接触账户、地址或付款步骤。
(() => {
  if (window.top !== window || !location.pathname.includes('/shop/')) return;

  const params = new URLSearchParams(location.hash.slice(1));
  const part = params.get('apw-auto-add');
  if (!part || !/^[A-Z0-9]{4,20}\/[A-Z]$/.test(part)) return;

  // 先移除片段，避免刷新或从购买页返回时再次加车。
  history.replaceState(history.state, '', location.pathname + location.search);

  const notice = (message, failed = false) => {
    let box = document.getElementById('apw-cart-notice');
    if (!box) {
      box = document.createElement('div');
      box.id = 'apw-cart-notice';
      box.setAttribute('role', 'status');
      Object.assign(box.style, {
        position: 'fixed', right: '20px', bottom: '20px', zIndex: '2147483647',
        maxWidth: '340px', padding: '14px 18px', borderRadius: '12px',
        background: '#1d1d1f', color: '#fff', font: '14px/1.5 system-ui, sans-serif',
        boxShadow: '0 4px 24px #0004'
      });
      document.body.appendChild(box);
    }
    box.textContent = message;
    if (failed) box.style.background = '#8b1e1e';
  };

  notice(`正在将 ${part} 加入购物袋…`);
  const started = Date.now();
  const timer = setInterval(async () => {
    const store = window.acStore;
    if (!store || store.isDisabled || typeof store.addItem !== 'function') {
      if (Date.now() - started >= 20000) {
        clearInterval(timer);
        notice('自动加车失败：Apple 购物袋接口不可用。请在当前页面手动添加。', true);
      }
      return;
    }

    clearInterval(timer);
    try {
      await store.addItem(part, 1);
      notice('已加入购物袋，正在打开购物袋…');
      const prefix = location.pathname.split('/shop/')[0];
      location.assign(`${location.origin}${prefix}/shop/bag`);
    } catch (error) {
      const detail = error && error.message ? `：${String(error.message).slice(0, 140)}` : '';
      notice(`自动加车失败${detail}。请在当前页面手动添加。`, true);
    }
  }, 250);
})();
