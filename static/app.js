// memory-review client: live updates, theme, keyboard, accept confirmation.
(function () {
  "use strict";

  const $ = (sel, root) => (root || document).querySelector(sel);
  const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));

  // The open card may refresh itself only when the user is not writing in it.
  window.mrCardIdle = function () {
    const ta = $("#card textarea");
    return !ta || (ta.value.trim() === "" && document.activeElement !== ta);
  };

  // ---- Live updates -------------------------------------------------------
  let timer = null;
  function changed() {
    clearTimeout(timer);
    timer = setTimeout(() => htmx.trigger(document.body, "mr:changed"), 300);
  }
  function connect() {
    const es = new EventSource("/events");
    es.addEventListener("changed", changed);
    // EventSource reconnects by itself; on reconnect, catch up once.
    es.addEventListener("open", changed);
  }

  // ---- Theme --------------------------------------------------------------
  function setTheme(mode) {
    if (mode === "light" || mode === "dark") document.documentElement.dataset.theme = mode;
    else delete document.documentElement.dataset.theme;
    try { localStorage.setItem("mr-theme", mode); } catch (e) { /* storage blocked */ }
  }
  function cycleTheme() {
    const cur = document.documentElement.dataset.theme || "auto";
    setTheme(cur === "auto" ? "dark" : cur === "dark" ? "light" : "auto");
  }

  // ---- Selection & navigation --------------------------------------------
  function currentId() {
    const c = $("#card");
    return c && c.dataset.id;
  }
  function markSelected() {
    const id = currentId();
    $$("#queue .row").forEach((r) => r.classList.toggle("sel", r.dataset.id === id));
    document.body.classList.toggle("has-card", !!id);
    $$("#card time").forEach((t) => {
      const d = new Date(t.getAttribute("datetime"));
      if (!isNaN(d)) t.textContent = d.toLocaleString(undefined, { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit" });
    });
  }
  function move(step) {
    const rows = $$("#queue .row");
    if (!rows.length) return;
    const i = rows.findIndex((r) => r.classList.contains("sel"));
    const next = rows[Math.min(rows.length - 1, Math.max(0, i < 0 ? 0 : i + step))];
    next.click();
    next.scrollIntoView({ block: "nearest" });
  }

  // ---- Accept needs two presses -------------------------------------------
  let armTimer = null;
  function pressAccept() {
    const b = $("#act-accept");
    if (!b) return;
    if (b.classList.contains("armed")) {
      clearTimeout(armTimer);
      b.disabled = true;
      htmx.trigger($("#accept-form"), "submit");
      return;
    }
    b.classList.add("armed");
    b.firstChild.textContent = b.dataset.armed + " ";
    armTimer = setTimeout(() => {
      b.classList.remove("armed");
      b.firstChild.textContent = b.dataset.label + " ";
    }, 2500);
  }

  function clickIf(sel) {
    const el = $(sel);
    if (el && !el.disabled) el.click();
  }

  document.addEventListener("click", (e) => {
    if (e.target.closest("#theme-toggle")) cycleTheme();
    if (e.target.closest("#act-accept")) { e.preventDefault(); pressAccept(); }
    const back = e.target.closest("[data-back]");
    if (back && window.matchMedia("(max-width: 760px)").matches) {
      e.preventDefault();
      document.body.classList.remove("has-card");
    }
  });

  document.addEventListener("keydown", (e) => {
    const inField = e.target.matches("textarea, input, select, [contenteditable]");
    if (inField) {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); clickIf("#act-send"); }
      if (e.key === "Escape") e.target.blur();
      return;
    }
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    switch (e.key.toLowerCase()) {
      case "j": move(1); break;
      case "k": move(-1); break;
      case "a": pressAccept(); break;
      case "s": clickIf("#act-snooze"); break;
      case "r": clickIf("#act-regenerate"); break;
      case "c": { const ta = $("#comment"); if (ta) { e.preventDefault(); ta.focus(); } break; }
      default: return;
    }
  });

  document.addEventListener("htmx:afterSettle", markSelected);
  document.addEventListener("DOMContentLoaded", () => { markSelected(); connect(); });
})();
