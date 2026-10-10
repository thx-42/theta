// theta — interactions, zero dependance.
(function () {
  "use strict";

  var INSTALL_CMD = "curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash";
  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------- plasma ascii du header ---------- */
  var bg = document.getElementById("ascii-bg");
  var hero = bg ? bg.closest(".hero") : null;
  var RAMP = " .:-=+*#%@";
  var CELL_W = 7.2;
  var CELL_H = 12;
  var cols = 0;
  var rows = 0;
  var running = true;
  var rafId = 0;
  var last = 0;
  var t = 0;

  function measure() {
    if (!hero) return;
    var w = hero.clientWidth || window.innerWidth;
    var h = hero.clientHeight || 400;
    cols = Math.max(10, Math.ceil(w / CELL_W));
    rows = Math.max(10, Math.ceil(h / CELL_H));
  }

  function frame(tt) {
    var out = "";
    var n = RAMP.length - 1;
    for (var y = 0; y < rows; y++) {
      for (var x = 0; x < cols; x++) {
        var v = Math.sin(x * 0.3 + tt) + Math.sin(y * 0.25 - tt * 0.7) + Math.sin((x + y) * 0.15 + tt * 0.5);
        var idx = Math.floor(((v + 3) / 6) * n);
        if (idx < 0) idx = 0;
        else if (idx > n) idx = n;
        out += RAMP.charAt(idx);
      }
      if (y < rows - 1) out += "\n";
    }
    return out;
  }

  function paint() {
    if (bg) bg.textContent = frame(t);
  }

  function loop(now) {
    rafId = 0;
    if (!running || document.hidden) return;
    if (now - last >= 1000 / 12) {
      last = now;
      t += 0.12;
      paint();
    }
    rafId = requestAnimationFrame(loop);
  }

  function start() {
    if (reduceMotion || !bg) return;
    if (rafId) return;
    last = 0;
    rafId = requestAnimationFrame(loop);
  }

  function stop() {
    running = false;
    if (rafId) cancelAnimationFrame(rafId);
    rafId = 0;
  }

  if (bg && hero) {
    measure();
    if (reduceMotion) {
      paint();
    } else {
      paint();
      start();
      if ("IntersectionObserver" in window) {
        new IntersectionObserver(function (entries) {
          running = entries[0].isIntersecting && !document.hidden;
          if (running) start();
        }, { threshold: 0 }).observe(hero);
      }
      document.addEventListener("visibilitychange", function () {
        running = !document.hidden;
        if (running) start();
      });
      var rzT = 0;
      window.addEventListener("resize", function () {
        clearTimeout(rzT);
        rzT = setTimeout(function () {
          measure();
          if (reduceMotion) paint();
        }, 200);
      });
    }
  }

  /* ---------- copie presse-papiers ---------- */
  function flash(btn, idle) {
    var old = idle || btn.textContent;
    btn.textContent = "[ copié ✔ ]";
    setTimeout(function () { btn.textContent = old; }, 1600);
  }

  function copyText(text, btn) {
    function done() { flash(btn); }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallback(); });
    } else {
      fallback();
    }
    function fallback() {
      var ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand("copy"); } catch (e) { /* noop */ }
      document.body.removeChild(ta);
      done();
    }
  }

  var copyBtn = document.getElementById("copy-btn");
  if (copyBtn) {
    copyBtn.addEventListener("click", function () {
      copyText(INSTALL_CMD, copyBtn);
    });
  }
  document.querySelectorAll(".mini-copy").forEach(function (btn) {
    btn.addEventListener("click", function () {
      copyText(btn.getAttribute("data-copy") || "", btn);
    });
  });

  /* ---------- onglets install ---------- */
  document.querySelectorAll(".tabs").forEach(function (tabs) {
    var tabBtns = Array.prototype.slice.call(tabs.querySelectorAll('[role="tab"]'));
    var panels = document.querySelectorAll(".panel");
    function select(btn) {
      tabBtns.forEach(function (b) {
        var on = b === btn;
        b.classList.toggle("active", on);
        b.setAttribute("aria-selected", on ? "true" : "false");
        b.tabIndex = on ? 0 : -1;
      });
      var name = btn.getAttribute("data-tab");
      panels.forEach(function (p) {
        var on = p.getAttribute("data-panel") === name;
        p.classList.toggle("active", on);
        if (on) p.removeAttribute("hidden");
        else p.setAttribute("hidden", "");
      });
    }
    tabBtns.forEach(function (btn, i) {
      btn.addEventListener("click", function () { select(btn); });
      btn.addEventListener("keydown", function (e) {
        var j = -1;
        if (e.key === "ArrowRight") j = (i + 1) % tabBtns.length;
        else if (e.key === "ArrowLeft") j = (i - 1 + tabBtns.length) % tabBtns.length;
        else if (e.key === "Home") j = 0;
        else if (e.key === "End") j = tabBtns.length - 1;
        if (j >= 0) {
          e.preventDefault();
          tabBtns[j].focus();
          select(tabBtns[j]);
        }
      });
    });
  });

  /* ---------- reveal au scroll ---------- */
  var reveals = document.querySelectorAll(".reveal");
  if (reduceMotion || !("IntersectionObserver" in window)) {
    reveals.forEach(function (el) { el.classList.add("visible"); });
  } else {
    var rio = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("visible");
          rio.unobserve(e.target);
        }
      });
    }, { threshold: 0.12 });
    reveals.forEach(function (el) { rio.observe(el); });
  }
})();
