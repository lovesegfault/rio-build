// docs/assets/theme.js
(() => {
  const KEY = "rio-theme";
  const root = document.documentElement;
  const stored = localStorage.getItem(KEY);
  const initial =
    stored || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  root.dataset.theme = initial;

  const isEditable = (el) =>
    el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable);

  addEventListener("DOMContentLoaded", () => {
    if (window.PagefindUI && document.querySelector("#search")) {
      new PagefindUI({ element: "#search", showSubResults: true });
      document
        .querySelector("#search input")
        ?.setAttribute("aria-label", "Search documentation");
    }
    const btn = document.querySelector(".rio-theme-toggle");
    if (btn) {
      btn.addEventListener("click", () => {
        const next = root.dataset.theme === "dark" ? "light" : "dark";
        root.dataset.theme = next;
        localStorage.setItem(KEY, next);
      });
    }

    // ─── hamburger mobile drawer ────────────────────────────────────
    const navToggle = document.querySelector(".rio-nav-toggle");
    const setNavOpen = (open) => {
      document.body.classList.toggle("nav-open", open);
      if (navToggle) navToggle.setAttribute("aria-expanded", String(open));
    };
    if (navToggle) {
      navToggle.addEventListener("click", () =>
        setNavOpen(!document.body.classList.contains("nav-open")),
      );
      // Click outside the drawer closes it.
      document.addEventListener("click", (ev) => {
        if (!document.body.classList.contains("nav-open")) return;
        if (ev.target.closest(".rio-nav, .rio-nav-toggle")) return;
        setNavOpen(false);
      });
    }

    // ─── keyboard nav: ←/→ chapter, S to focus search ───────────────
    addEventListener("keydown", (ev) => {
      if (ev.altKey || ev.ctrlKey || ev.metaKey || ev.shiftKey) return;
      // Escape is meaningful from inside the search input (clear+blur),
      // so it bypasses the isEditable early-out the other keys use.
      if (ev.key === "Escape") {
        setNavOpen(false);
        document.querySelector(".pagefind-ui__search-clear")?.click();
        document.querySelector("#search input")?.blur();
        return;
      }
      if (isEditable(ev.target)) return;
      if (ev.key === "ArrowLeft") {
        const a = document.querySelector(".mobile-nav-chapters.previous");
        if (a) location.href = a.href;
      } else if (ev.key === "ArrowRight") {
        const a = document.querySelector(".mobile-nav-chapters.next");
        if (a) location.href = a.href;
      } else if ((ev.key === "s" || ev.key === "S") && !ev.repeat) {
        const input = document.querySelector(
          "#search input, .pagefind-ui__search-input",
        );
        if (input) {
          ev.preventDefault();
          setNavOpen(true);
          input.focus();
        }
      }
    });

    // ─── code-block copy button ─────────────────────────────────────
    for (const b of document.querySelectorAll(".rio-copy")) {
      const orig = b.textContent;
      b.addEventListener("click", () => {
        // The button may be wrapped in an auto-<p> by typst's html
        // export, so parentElement isn't reliably .rio-code — climb.
        const pre = b.closest(".rio-code")?.querySelector("pre");
        if (!pre) return;
        const flash = (txt) => {
          b.textContent = txt;
          setTimeout(() => {
            b.classList.remove("copied");
            b.textContent = orig;
          }, 1500);
        };
        // navigator.clipboard is undefined on non-secure origins.
        const p = navigator.clipboard?.writeText(pre.textContent);
        if (!p) return flash("✗");
        p.then(() => {
          b.classList.add("copied");
          flash("✓");
        }).catch(() => flash("✗"));
      });
    }

    // ─── on-this-page scroll-spy ────────────────────────────────────
    const tocLinks = [...document.querySelectorAll(".rio-toc a")];
    if (tocLinks.length > 0) {
      const byId = new Map(
        tocLinks.map((a) => [a.getAttribute("href").slice(1), a]),
      );
      const heads = [...document.querySelectorAll(".rio-main h2[id], .rio-main h3[id]")]
        .filter((h) => byId.has(h.id));
      const visible = new Set();
      const setActive = (id) => {
        for (const a of tocLinks) a.classList.toggle("active", a === byId.get(id));
      };
      const io = new IntersectionObserver(
        (entries) => {
          for (const e of entries) {
            if (e.isIntersecting) visible.add(e.target);
            else visible.delete(e.target);
          }
          // Active = topmost visible heading; if none visible (between
          // sections on a tall viewport), keep the last one.
          if (visible.size > 0) {
            const top = heads.find((h) => visible.has(h));
            if (top) setActive(top.id);
          }
        },
        { rootMargin: "0px 0px -70% 0px" },
      );
      for (const h of heads) io.observe(h);
    }
  });
})();
