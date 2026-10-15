// 供桌面应用在发售待命标签页查询。只完成官网必需的选配，不点击加车。
(part) => {
  const visible = (element) => element && !element.disabled &&
    element.getAttribute('aria-disabled') !== 'true' && element.getClientRects().length > 0;
  const form = Array.from(document.querySelectorAll('form[data-part-number]'))
    .find((node) => node.getAttribute('data-part-number')?.toUpperCase() === part);
  const add = form?.querySelector('[data-autom="add-to-cart"]');
  if (!add) return false;
  if (visible(add)) return true;

  const noTradeIn = document.querySelector('input[data-autom="choose-noTradeIn"]');
  if (visible(noTradeIn) && !noTradeIn.checked) {
    noTradeIn.click();
    return false;
  }
  const noCare = document.querySelector('input[data-autom="noapplecare"]');
  if (visible(noCare) && !noCare.checked) {
    noCare.click();
    return false;
  }
  if (!visible(noTradeIn) && !visible(noCare) &&
      !document.documentElement.hasAttribute('data-apw-release-capacity-expanded')) {
    const capacity = document.querySelector('input[name="dimensionCapacity"]:checked');
    if (visible(capacity)) {
      document.documentElement.setAttribute('data-apw-release-capacity-expanded', '1');
      capacity.click();
    }
  }
  return false;
}
