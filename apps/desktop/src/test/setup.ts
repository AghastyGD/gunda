Object.defineProperty(HTMLDialogElement.prototype, "showModal", {
  configurable: true,
  value(this: HTMLDialogElement) { this.setAttribute("open", ""); },
});

Object.defineProperty(HTMLDialogElement.prototype, "close", {
  configurable: true,
  value(this: HTMLDialogElement) {
    this.removeAttribute("open");
    this.dispatchEvent(new Event("close"));
  },
});

globalThis.requestAnimationFrame = (callback) => setTimeout(() => callback(0), 25) as unknown as number;
