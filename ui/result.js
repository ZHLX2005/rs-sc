// rs-sc result window script.
// The backend emits "result:loaded" with { text, imageBase64 } every time a new
// capture finishes translating.

(function () {
  const statusEl = document.getElementById("status");
  const resultEl = document.getElementById("result");
  const imagePane = document.getElementById("image-pane");
  const thumbEl = document.getElementById("thumb");
  const copyBtn = document.getElementById("copy-btn");
  const closeBtn = document.getElementById("close-btn");

  let lastText = "";

  function showBusy(imageBase64) {
    statusEl.hidden = false;
    statusEl.textContent = "正在翻译…";
    statusEl.classList.remove("status-error");
    resultEl.hidden = true;
    resultEl.textContent = "";
    if (imageBase64) {
      thumbEl.src = "data:image/png;base64," + imageBase64;
      imagePane.hidden = false;
    }
  }

  function showResult(payload) {
    lastText = payload.text || "(无内容)";
    resultEl.textContent = lastText;
    resultEl.hidden = false;
    statusEl.hidden = true;
    statusEl.classList.remove("status-error");
    copyBtn.disabled = false;

    if (payload.imageBase64) {
      thumbEl.src = "data:image/png;base64," + payload.imageBase64;
      imagePane.hidden = false;
    } else {
      imagePane.hidden = true;
    }
  }

  function showError(msg) {
    statusEl.hidden = false;
    statusEl.textContent = "翻译失败: " + msg;
    statusEl.classList.add("status-error");
    resultEl.hidden = true;
  }

  // Listen for backend events. With withGlobalTauri = true we get window.__TAURI__.
  function bind() {
    const t = window.__TAURI__;
    if (!t || !t.event) {
      return setTimeout(bind, 30);
    }
    // "result:busy" — user just selected a region, AI call is in flight.
    // The window was already shown by the backend, just reset to busy state.
    t.event.listen("result:busy", (event) => {
      const p = event.payload || {};
      showBusy(p.imageBase64);
    });
    // "result:loaded" — AI returned a translation.
    t.event.listen("result:loaded", (event) => {
      showResult(event.payload || {});
    });
    // "result:error" — AI call failed.
    t.event.listen("result:error", (event) => {
      const p = event.payload || {};
      showError(p.error || "未知错误");
    });
  }

  // Buttons.
  copyBtn.addEventListener("click", async () => {
    if (!lastText) return;
    try {
      await navigator.clipboard.writeText(lastText);
      const prev = copyBtn.textContent;
      copyBtn.textContent = "已复制";
      setTimeout(() => (copyBtn.textContent = prev), 1200);
    } catch (e) {
      console.error("clipboard write failed", e);
    }
  });

  closeBtn.addEventListener("click", () => {
    const w = window.__TAURI__ && window.__TAURI__.window;
    if (w && w.getCurrentWindow) {
      w.getCurrentWindow().close();
    } else {
      window.close();
    }
  });

  // Esc closes the window — feels natural in a screenshot tool.
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeBtn.click();
  });

  // Initial state — the backend only shows this window when there's work
  // to display, so a brief flash of "等待结果…" is fine.
  showBusy();
  copyBtn.disabled = true;
  bind();
})();