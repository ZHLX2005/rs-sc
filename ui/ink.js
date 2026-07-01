// rs-sc ink window — pressure-sensitive canvas + in-window answer panel.
//
// UX design (single window replaces the old "ink + popup result" pair):
//
//   ┌─ 手写提问 ─────────────── 📌 × ─┐
//   │  [原图缩略图] [画板]          │  ← ask-pane (always visible)
//   │  (用户持续在这里手写提问)        │
//   ├───────────────────────────────┤
//   │  AI 答: …                     │  ← answer-pane (collapsed by default,
//   │  识别: 你好    [复制]            │     expands with 250ms ease-out when
//   └───────────────────────────────┘     AI responds, stays open for follow-up)
//     [清空]              [确认 →]
//
// On `ink:start`: backend pushes the original screenshot; we paint the
// thumbnail and clear the canvas so the user can immediately write.
// On Confirm: encode canvas + screenshot into ONE vertical-stack composite
// PNG; backend runs OCR → QA and emits ink:busy/done/error events. The
// answer renders inline (no popup, no second window). The user can then
// clear canvas, write the next question, hit confirm again — the result
// refreshes in-place, no UI churn.
//
// Key correctness points:
//   - PointerEvent (not mouse*) for pressure-sensitive tablets
//   - setPointerCapture on pointerdown so pen drag survives leaving canvas
//   - Composite image (screenshot on top, handwriting below) gives the
//     OCR step spatial context, hugely improving recognition accuracy
//   - Always-on-top enabled by default; pin button toggles it off

(function () {
  const t = window.__TAURI__;

  // ── DOM refs ─────────────────────────────────────────────────────────
  const statusEl = document.getElementById("status");
  const thoughtBadge = document.getElementById("thought-badge");
  const thumbEl = document.getElementById("thumb");
  const canvas = document.getElementById("ink");
  const emptyHint = document.getElementById("empty-hint");
  const clearBtn = document.getElementById("clear-btn");
  const closeBtn = document.getElementById("close-btn");
  const pinBtn = document.getElementById("pin-btn");
  const confirmBtn = document.getElementById("confirm-btn");

  const answerPane = document.getElementById("answer-pane");
  const answerStatus = document.getElementById("answer-status");
  const recognizedEl = document.getElementById("recognized");
  const copyBtn = document.getElementById("copy-btn");
  const resultEl = document.getElementById("result");

  const ctx = canvas.getContext("2d");
  let hasContent = false;
  let busy = false;
  let lastAnswer = ""; // for the copy button

  // ── Canvas setup: devicePixelRatio-aware sizing ─────────────────────────
  function fitCanvas() {
    const dpr = window.devicePixelRatio || 1;
    const cssW = canvas.parentElement.clientWidth - 2;
    const cssH = canvas.parentElement.clientHeight - 2;
    canvas.style.width = cssW + "px";
    canvas.style.height = cssH + "px";
    canvas.width = Math.round(cssW * dpr);
    canvas.height = Math.round(cssH * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.strokeStyle = "#111";
  }
  fitCanvas();
  window.addEventListener("resize", fitCanvas);

  // ── Pointer drawing ─────────────────────────────────────────────────────
  let drawing = false;
  let lastX = 0, lastY = 0;

  function localXY(ev) {
    const rect = canvas.getBoundingClientRect();
    return {
      x: ev.clientX - rect.left,
      y: ev.clientY - rect.top,
    };
  }

  function pressureWidth(pressure) {
    // 0.8 (light hover) .. 4.0 (firm press). Mouse with no pressure
    // reports 0.5 → ~2.4px which reads comfortably on a standard screen.
    const p = typeof pressure === "number" && pressure > 0 ? pressure : 0.5;
    return 0.8 + p * 3.2;
  }

  canvas.addEventListener("pointerdown", (e) => {
    if (busy) return;
    drawing = true;
    canvas.setPointerCapture(e.pointerId);
    const { x, y } = localXY(e);
    lastX = x; lastY = y;
    ctx.beginPath();
    ctx.moveTo(x, y);
    ctx.lineWidth = pressureWidth(e.pressure);
    // One-tap dot (so a single tap registers a stroke).
    ctx.lineTo(x + 0.01, y + 0.01);
    ctx.stroke();
  });

  canvas.addEventListener("pointermove", (e) => {
    if (!drawing || busy) return;
    const { x, y } = localXY(e);
    ctx.lineWidth = pressureWidth(e.pressure);
    ctx.beginPath();
    ctx.moveTo(lastX, lastY);
    ctx.lineTo(x, y);
    ctx.stroke();
    lastX = x; lastY = y;
  });

  function endStroke(e) {
    if (!drawing) return;
    drawing = false;
    if (e && e.pointerId !== undefined) {
      try { canvas.releasePointerCapture(e.pointerId); } catch (_) {}
    }
    if (!hasContent) {
      hasContent = true;
      emptyHint.hidden = true;
      confirmBtn.disabled = false;
    }
  }
  canvas.addEventListener("pointerup", endStroke);
  canvas.addEventListener("pointercancel", endStroke);
  canvas.addEventListener("pointerleave", endStroke);

  // ── Helpers ────────────────────────────────────────────────────────────
  function setHeaderStatus(kind, text) {
    statusEl.className = "status " + kind;
    statusEl.textContent = text;
  }

  function setAnswerStatus(kind, text) {
    answerStatus.className = "answer-status " + kind;
    answerStatus.textContent = text;
    answerStatus.hidden = !text;
  }

  function collapseAnswer() {
    answerPane.classList.remove("open");
    // Hide children after the open-class transition completes — keeps
    // assistive tech from reading hidden content.
    setTimeout(() => {
      if (!answerPane.classList.contains("open")) {
        resultEl.hidden = true;
        recognizedEl.hidden = true;
        copyBtn.hidden = true;
        answerStatus.hidden = true;
      }
    }, 250);
  }

  function expandAnswer() {
    answerPane.classList.add("open");
    resultEl.hidden = false;
    copyBtn.hidden = false;
  }

  function clearCanvas() {
    ctx.save();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.restore();
    hasContent = false;
    emptyHint.hidden = false;
    confirmBtn.disabled = true;
    // Clear the previous answer too — the user is starting a new question
    // so showing stale AI output would be confusing.
    collapseAnswer();
    recognizedEl.hidden = true;
    recognizedEl.textContent = "";
    lastAnswer = "";
  }

  // ── Composite (screenshot + canvas) PNG for the LLM ─────────────────
  // Vertical stack: screenshot on top, handwriting below, separated by a
  // thin gray rule, white background everywhere. See test_canvas_preprocess.mjs
  // for a Node-based test of the same composition math.
  function encodeCanvasPng() {
    const originalImg = thumbEl;
    if (!originalImg.complete || !originalImg.naturalWidth) {
      return canvas.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
    }
    const origW = originalImg.naturalWidth;
    const origH = originalImg.naturalHeight;

    const SEP_HEIGHT = 8;
    const MAX_TOTAL_WIDTH = 1600;

    function fit(w, h, maxW) {
      if (w <= maxW) return { w, h };
      const scale = maxW / w;
      return { w: maxW, h: Math.round(h * scale) };
    }
    const origFit = fit(origW, origH, MAX_TOTAL_WIDTH);
    const inkFit = fit(canvas.width, canvas.height, MAX_TOTAL_WIDTH);

    const compW = Math.max(origFit.w, inkFit.w);
    const compH = origFit.h + SEP_HEIGHT + inkFit.h;

    const comp = document.createElement("canvas");
    comp.width = compW;
    comp.height = compH;
    const cctx = comp.getContext("2d");

    cctx.fillStyle = "#ffffff";
    cctx.fillRect(0, 0, compW, compH);

    cctx.drawImage(
      originalImg,
      (compW - origFit.w) / 2, 0,
      origFit.w, origFit.h,
    );
    cctx.fillStyle = "#cccccc";
    cctx.fillRect(0, origFit.h, compW, SEP_HEIGHT);
    cctx.fillStyle = "#ffffff";
    cctx.fillRect(
      (compW - inkFit.w) / 2,
      origFit.h + SEP_HEIGHT,
      inkFit.w, inkFit.h,
    );
    cctx.drawImage(
      canvas,
      (compW - inkFit.w) / 2, origFit.h + SEP_HEIGHT,
      inkFit.w, inkFit.h,
    );

    return comp.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
  }

  // ── Pin / unpin ────────────────────────────────────────────────────────
  let pinned = true;
  pinBtn.addEventListener("click", async () => {
    pinned = !pinned;
    pinBtn.classList.toggle("pinned", pinned);
    pinBtn.title = pinned ? "取消置顶" : "置顶窗口";
    pinBtn.textContent = pinned ? "📌" : "📍";
    try {
      await window.__TAURI__.core.invoke("set_ink_always_on_top", { onTop: pinned });
    } catch (e) {
      console.error("set_ink_always_on_top failed:", e);
    }
  });

  // ── Backend events ────────────────────────────────────────────────────
  function bindBackend() {
    if (!t || !t.event) return setTimeout(bindBackend, 30);

    t.event.listen("ink:start", (event) => {
      const p = event.payload || {};
      if (p.imageBase64) thumbEl.src = "data:image/png;base64," + p.imageBase64;
      // New capture session → user is starting fresh. Collapse any prior
      // answer so the ask-pane is the visual focus.
      clearCanvas();
      setHeaderStatus("info", "请手写提问");
      canvas.focus();
    });

    t.event.listen("ink:busy", (event) => {
      const p = event.payload || {};
      busy = true;
      confirmBtn.disabled = true;
      clearBtn.disabled = true;
      if (p.stage === "ocr") setHeaderStatus("info", "识别手写中...");
      else if (p.stage === "qa") setHeaderStatus("info", "基于截图回答中...");
      else setHeaderStatus("info", "处理中...");
      // Show a "thinking…" line in the answer-pane so the user sees the
      // pipeline is alive even before any actual content lands.
      setAnswerStatus("info", "思考中…");
      expandAnswer();
    });

    t.event.listen("ink:done", (event) => {
      const p = event.payload || {};
      busy = false;
      confirmBtn.disabled = !hasContent;
      clearBtn.disabled = false;
      setHeaderStatus("ok", "✓ 已回答");

      // Recognized question pill — useful for the user to verify OCR.
      if (p.recognizedText) {
        recognizedEl.hidden = false;
        recognizedEl.textContent = "提问识别: " + p.recognizedText;
      } else {
        recognizedEl.hidden = true;
      }

      // The actual answer.
      lastAnswer = p.answer || "(无内容)";
      resultEl.textContent = lastAnswer;
      expandAnswer();
      setAnswerStatus("ok", "✓ 已回答");
    });

    t.event.listen("ink:error", (event) => {
      const p = event.payload || {};
      busy = false;
      confirmBtn.disabled = !hasContent;
      clearBtn.disabled = false;
      setHeaderStatus("error", "失败");
      expandAnswer();
      setAnswerStatus("error", p.error || "未知错误");
    });

    // "result:thinking" — reasoning model emitted a `` block that
    // the backend already stripped. We briefly flash a badge so the
    // user knows the model did internal reasoning. The trace itself
    // is never displayed.
    t.event.listen("result:thinking", () => {
      if (!thoughtBadge) return;
      thoughtBadge.hidden = false;
      thoughtBadge.classList.add("flash");
      setTimeout(() => thoughtBadge.classList.remove("flash"), 1200);
    });
  }

  // ── Buttons ───────────────────────────────────────────────────────────
  clearBtn.addEventListener("click", clearCanvas);

  function closeWindow() {
    if (t && t.window && t.window.getCurrentWindow) {
      t.window.getCurrentWindow().close();
    } else {
      window.close();
    }
  }
  closeBtn.addEventListener("click", closeWindow);

  copyBtn.addEventListener("click", async () => {
    if (!lastAnswer) return;
    try {
      await navigator.clipboard.writeText(lastAnswer);
      const prev = copyBtn.textContent;
      copyBtn.textContent = "已复制";
      setTimeout(() => (copyBtn.textContent = prev), 1200);
    } catch (e) {
      console.error("clipboard write failed", e);
    }
  });

  confirmBtn.addEventListener("click", async () => {
    if (busy || !hasContent) return;
    busy = true;
    confirmBtn.disabled = true;
    clearBtn.disabled = true;
    setHeaderStatus("info", "准备发送...");
    try {
      const pngBase64 = encodeCanvasPng();
      await window.__TAURI__.core.invoke("submit_ink_question", { canvasPngBase64: pngBase64 });
      // ink:busy + ink:done / ink:error drive the rest of the UI.
    } catch (e) {
      busy = false;
      confirmBtn.disabled = !hasContent;
      clearBtn.disabled = false;
      setHeaderStatus("error", "提交失败: " + (e?.message || e));
    }
  });

  // Esc closes the window. Cmd/Ctrl+Enter submits if the canvas has ink.
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeWindow();
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter" && !busy && hasContent) {
      e.preventDefault();
      confirmBtn.click();
    }
  });

  bindBackend();
})();
