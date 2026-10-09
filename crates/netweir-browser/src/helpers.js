// netweir's helpers. They run in an isolated world: they see the page's
// DOM, but the page can't see them, and nothing the page does to its own
// globals reaches them.
(() => {
  // The next animation frame, or 100 ms if frames aren't running.
  const frame = () =>
    new Promise((resolve) => {
      requestAnimationFrame(() => resolve());
      setTimeout(resolve, 100);
    });

  const box = (el) => {
    const r = el.getBoundingClientRect();
    return [r.x, r.y, r.width, r.height];
  };

  const visible = (el) => {
    const [, , w, h] = box(el);
    return w > 0 && h > 0 && getComputedStyle(el).visibility !== "hidden";
  };

  const enabled = (el) => !el.matches(":disabled") && !el.closest('[aria-disabled="true"]');

  globalThis.__netweir = {
    // Whether the element `selector` finds passes `checks`, in order. The
    // first that fails is named; otherwise its centre is returned.
    async check(selector, checks, scroll) {
      const el = document.querySelector(selector);
      if (!el) return { failed: "attached" };
      if (scroll) el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      let b = box(el);
      for (const check of checks) {
        if (check === "visible" && !visible(el)) return { failed: "visible" };
        if (check === "enabled" && !enabled(el)) return { failed: "enabled" };
        if (check === "editable") {
          const field = el.matches("input, textarea, select") || el.isContentEditable;
          const readOnly = el.readOnly || el.getAttribute("aria-readonly") === "true";
          if (!field || readOnly || !enabled(el)) return { failed: "editable" };
        }
        if (check === "stable") {
          await frame();
          const before = box(el);
          await frame();
          b = box(el);
          if (before.some((v, i) => v !== b[i])) return { failed: "stable" };
        }
        if (check === "receives events") {
          const hit = document.elementFromPoint(b[0] + b[2] / 2, b[1] + b[3] / 2);
          if (!hit || (hit !== el && !el.contains(hit))) return { failed: "receives events" };
        }
      }
      return { x: b[0] + b[2] / 2, y: b[1] + b[3] / 2 };
    },

    // "detached", "hidden" or "visible".
    state(selector) {
      const el = document.querySelector(selector);
      if (!el) return "detached";
      return visible(el) ? "visible" : "hidden";
    },

    // Focuses the element and selects what's in it, so typing replaces it.
    focus(selector) {
      const el = document.querySelector(selector);
      if (!el) return false;
      el.focus();
      if (typeof el.select === "function") {
        el.select();
      } else if (el.isContentEditable) {
        const range = document.createRange();
        range.selectNodeContents(el);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      }
      return document.activeElement === el;
    },

    content() {
      const doctype = document.doctype ? new XMLSerializer().serializeToString(document.doctype) : "";
      return doctype + (document.documentElement ? document.documentElement.outerHTML : "");
    },
  };
})();
