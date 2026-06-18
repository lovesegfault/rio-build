// docs/assets/theme.js
(() => {
  const KEY = "rio-theme";
  const root = document.documentElement;
  const stored = localStorage.getItem(KEY);
  const initial =
    stored || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  root.dataset.theme = initial;
  addEventListener("DOMContentLoaded", () => {
    if (window.PagefindUI && document.querySelector("#search")) {
      new PagefindUI({ element: "#search", showSubResults: true });
    }
    const btn = document.querySelector(".rio-theme-toggle");
    if (!btn) return;
    btn.addEventListener("click", () => {
      const next = root.dataset.theme === "dark" ? "light" : "dark";
      root.dataset.theme = next;
      localStorage.setItem(KEY, next);
    });
  });
})();
