export function measureMenuHeight(shell: HTMLElement): number {
  const style = window.getComputedStyle(shell);
  const borderHeight =
    (Number.parseFloat(style.borderTopWidth) || 0) +
    (Number.parseFloat(style.borderBottomWidth) || 0);

  // scrollHeight excludes borders. The additional logical pixel absorbs
  // fractional device-scale rounding when Tauri converts the requested size.
  return Math.ceil(shell.scrollHeight + borderHeight + 1);
}
