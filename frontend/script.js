// θ theta — interactions du site, zéro dépendance.
(function () {
  "use strict";

  var INSTALL_CMD = "curl -fsSL https://raw.githubusercontent.com/thx-42/theta/main/install.sh | bash";
  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------- copie presse-papiers ---------- */
  function flash(btn, labelEl, idleText) {
    var old = labelEl ? labelEl.textContent : btn.textContent;
    if (labelEl) labelEl.textContent = "copié ✔";
    else btn.textContent = "copié ✔";
    setTimeout(function () {
      if (labelEl) labelEl.textContent = idleText || old;
      else btn.textContent = idleText || old;
    }, 1600);
  }

  function copyText(text, btn, labelEl, idleText) {
    function done() { flash(btn, labelEl, idleText); }
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
  var copyLabel = document.getElementById("copy-label");
  if (copyBtn) {
    copyBtn.addEventListener("click", function () {
      copyText(INSTALL_CMD, copyBtn, copyLabel, "copier");
    });
  }
  var copyBtn2 = document.getElementById("copy-btn-2");
  if (copyBtn2) {
    copyBtn2.addEventListener("click", function () {
      copyText(INSTALL_CMD, copyBtn2, null, "copier la commande");
    });
  }
  document.querySelectorAll(".mini-copy").forEach(function (btn) {
    btn.addEventListener("click", function () {
      copyText(btn.getAttribute("data-copy") || "", btn, null, "copier");
    });
  });

  /* ---------- onglets install ---------- */
  document.querySelectorAll(".tabs").forEach(function (tabs) {
    var tabBtns = tabs.querySelectorAll(".tab");
    tabBtns.forEach(function (btn) {
      btn.addEventListener("click", function () {
        tabBtns.forEach(function (b) {
          b.classList.remove("active");
          b.setAttribute("aria-selected", "false");
        });
        btn.classList.add("active");
        btn.setAttribute("aria-selected", "true");
        var name = btn.getAttribute("data-tab");
        document.querySelectorAll(".panel").forEach(function (p) {
          p.classList.toggle("active", p.getAttribute("data-panel") === name);
        });
      });
    });
  });

  /* ---------- démo terminal ---------- */
  var typed = document.getElementById("typed");
  var caret = document.getElementById("caret");
  var termOut = document.getElementById("term-out");
  var termBody = document.getElementById("term-body");

  var userCmd = 'theta -p "explique src/main.rs"';
  var outLines = [
    { cls: "tool", text: "⟳  read src/main.rs · grep main( · todo 2 tâches" },
    { cls: "ok", text: "✔ src/main.rs : point d'entrée — parse les args (clap), démarre le daemon, ouvre la TUI." },
    { cls: "dim", text: "2 read · 0 write · 1 cmd · 3 tools · ~1,2k tokens" }
  ];

  function renderInstant() {
    if (!typed || !termOut) return;
    typed.textContent = userCmd;
    if (caret) caret.style.display = "none";
    termOut.innerHTML = "";
    outLines.forEach(function (l) {
      var div = document.createElement("div");
      div.className = "line show " + l.cls;
      div.textContent = l.text;
      termOut.appendChild(div);
    });
  }

  function renderAnimated() {
    if (!typed || !termOut) return;
    var i = 0;
    var typeTimer = setInterval(function () {
      typed.textContent = userCmd.slice(0, ++i);
      if (i >= userCmd.length) {
        clearInterval(typeTimer);
        if (caret) caret.style.display = "none";
        showLines(0);
      }
    }, 45);
    function showLines(n) {
      if (n >= outLines.length) return;
      var l = outLines[n];
      var div = document.createElement("div");
      div.className = "line " + l.cls;
      div.textContent = l.text;
      termOut.appendChild(div);
      requestAnimationFrame(function () { div.classList.add("show"); });
      setTimeout(function () { showLines(n + 1); }, 650);
    }
  }

  if (termBody) {
    if (reduceMotion) {
      renderInstant();
    } else if ("IntersectionObserver" in window) {
      var started = false;
      var io = new IntersectionObserver(function (entries) {
        if (entries[0].isIntersecting && !started) {
          started = true;
          renderAnimated();
          io.disconnect();
        }
      }, { threshold: 0.35 });
      io.observe(termBody);
    } else {
      renderAnimated();
    }
  }

  /* ---------- reveal on scroll ---------- */
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
