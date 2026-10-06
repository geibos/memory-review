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

  // Buttons with data-confirm need a second press within 3 seconds.
  document.addEventListener("click", (e) => {
    const b = e.target.closest("button[data-confirm]");
    if (!b || b.classList.contains("armed")) return;
    e.preventDefault();
    const label = b.textContent;
    b.classList.add("armed");
    b.textContent = b.dataset.confirm;
    setTimeout(() => { b.classList.remove("armed"); b.textContent = label; }, 3000);
  }, true);

  document.addEventListener("click", (e) => {
    if (e.target.closest("#theme-toggle")) cycleTheme();
    if (e.target.closest("#act-accept")) { e.preventDefault(); pressAccept(); }
    const back = e.target.closest("[data-back]");
    if (back && window.matchMedia("(max-width: 760px)").matches) {
      e.preventDefault();
      document.body.classList.remove("has-card");
    }
  });

  // ---- Anchored comments --------------------------------------------------
  function draftSelection() {
    const sel = window.getSelection();
    const body = $("#card .draft-body");
    if (!sel || sel.isCollapsed || !body || !body.contains(sel.anchorNode) || !body.contains(sel.focusNode)) return null;
    const text = sel.toString().trim();
    return text.length >= 3 ? { text, rect: sel.getRangeAt(0).getBoundingClientRect() } : null;
  }
  function anchorCompose(kind, fields, label, slot) {
    const form = $("#compose");
    if (!form) return;
    form.elements.anchor.value = kind;
    form.elements.quote.value = fields.quote || "";
    form.elements.permalink.value = fields.permalink || "";
    form.elements.line.value = fields.line || "";
    const chip = $(".anchor-chip", form);
    $("q", chip).textContent = label.length > 140 ? label.slice(0, 140) + "…" : label;
    chip.hidden = false;
    slot.appendChild(form);
    $("#comment").focus();
  }
  function resetCompose() {
    const form = $("#compose");
    if (!form) return;
    ["anchor", "quote", "permalink", "line"].forEach((n) => (form.elements[n].value = ""));
    $(".anchor-chip", form).hidden = true;
    $("#compose-home").appendChild(form);
  }
  function commentSelection() {
    const s = draftSelection();
    if (!s) return false;
    anchorCompose("draft", { quote: s.text }, s.text, $("#margin-slot"));
    $("#float-c").hidden = true;
    return true;
  }
  document.addEventListener("mouseup", () => {
    const btn = $("#float-c");
    if (!btn) return;
    const s = draftSelection();
    if (!s) { btn.hidden = true; return; }
    btn.style.top = Math.max(8, s.rect.top - 36) + "px";
    btn.style.left = Math.min(window.innerWidth - 180, s.rect.left) + "px";
    btn.hidden = false;
  });
  // Keep the selection when the floating button is pressed.
  document.addEventListener("mousedown", (e) => { if (e.target.closest("#float-c")) e.preventDefault(); });
  document.addEventListener("click", (e) => {
    if (e.target.closest("#float-c")) commentSelection();
    if (e.target.closest(".chip-x")) resetCompose();
    const lc = e.target.closest(".line-c");
    if (lc) {
      const row = lc.closest(".dl");
      let slot = row.nextElementSibling;
      if (!slot || !slot.classList.contains("line-comments")) {
        slot = document.createElement("div");
        slot.className = "line-comments";
        row.after(slot);
      }
      anchorCompose("diff", { permalink: lc.dataset.permalink, line: lc.dataset.line }, lc.dataset.line, slot);
    }
  });
  // Pair a margin note with its highlight.
  function pair(e, on) {
    const el = e.target.closest && e.target.closest("[data-n]");
    if (!el || !el.closest("#card")) return;
    $$('#card [data-n="' + el.dataset.n + '"]').forEach((x) => x.classList.toggle("hl", on));
  }
  document.addEventListener("mouseover", (e) => pair(e, true));
  document.addEventListener("mouseout", (e) => pair(e, false));

  document.addEventListener("keydown", (e) => {
    const inField = e.target.matches("textarea, input:not([type=radio]), select, [contenteditable]");
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
      case "c": {
        e.preventDefault();
        if (!commentSelection()) { const ta = $("#comment"); if (ta) ta.focus(); }
        break;
      }
      default: return;
    }
  });

  document.addEventListener("htmx:afterSettle", markSelected);
  document.addEventListener("DOMContentLoaded", () => { markSelected(); connect(); });
})();
