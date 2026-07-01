// rs-sc ink window — pressure-sensitive canvas for handwriting questions.
//
// On `ink:start` event: backend hands us the original screenshot base64,
// we display the thumbnail, clear the canvas, and focus it so the user
// can immediately start writing.
//
// On Confirm: encode canvas as PNG, call `submit_ink_question`, listen
// for ink:busy / ink:done / ink:error to update the status bar.
//
// Key correctness points:
//   - Use `pointer*` events (not mouse*): pressure, tiltX/Y, pointerType
//     are all delivered through PointerEvent on every modern browser engine.
//   - Maintain a `hasContent` flag so the Confirm button is only enabled
//     after at least one stroke is drawn.
//   - `setPointerCapture` on pointerdown so we keep getting move events
//     even if the pen leaves the canvas during a stroke.

(function () {
  const t = window.__TAURI__;
  const statusEl = document.getElementById("status");
  const thumbEl = document.getElementById("thumb");
  const canvas = document.getElementById("ink");
  const emptyHint = document.getElementById("empty-hint");
  const clearBtn = document.getElementById("clear-btn");
  const closeBtn = document.getElementById("close-btn");
  const cancelBtn = document.getElementById("cancel-btn");
  const confirmBtn = document.getElementById("confirm-btn");
  const recognizedEl = document.getElementById("recognized");

  const ctx = canvas.getContext("2d");
  let hasContent = false;
  let busy = false;

  // ── Canvas setup: devicePixelRatio-aware sizing ──────────────────────────
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
  // Each stroke is one continuous pointerdown..pointerup; we use one path
  // per stroke (beginPath at down, lineTo at each move, stroke at up).
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
    // Number between ~1 (hover/no pressure) and 4 (full press).
    // Most tablets report 0..1; clamp & scale.
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
    // Render the starting dot so a tap is visible.
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
  function setStatus(kind, text) {
    statusEl.className = "status " + kind;
    statusEl.textContent = text;
  }

  function clearCanvas() {
    ctx.save();
    // Reset transform before clearing so devicePixelRatio scaling doesn't
    // shrink the cleared region.
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.restore();
    hasContent = false;
    emptyHint.hidden = false;
    confirmBtn.disabled = true;
    recognizedEl.hidden = true;
    recognizedEl.textContent = "";
  }

  function encodeCanvasPng() {
    // Compose the screenshot + the user's handwriting into a single image
    // and return it as base64 PNG.
    //
    // Why a single composite image (not separate image_url + canvas):
    // Vision-language OCR is most accurate when the model can see the
    // handwriting in its *spatial context* — i.e. right next to (or on top
    // of) the original screenshot. Sending two unrelated images and asking
    // the model to "merge" them is error-prone; sending one image where
    // the screenshot is the top half and the handwriting is the bottom
    // half (or vice versa) gives the model a clear, contiguous field.
    //
    // We pick **vertical stacking**: original screenshot on top, handwriting
    // canvas below, separated by a thin neutral gray rule. The order
    // matches the ink window UI exactly (thumbnail pane on top, canvas
    // pane on the bottom) so the model sees the same layout the user did.
    //
    // Pure JS, <20ms, no Rust involvement.

    // 1. Need the original screenshot's bitmap. We keep a hidden <img>
    //    (`thumbEl`) updated from the ink:start payload. Draw that into an
    //    off-screen canvas to get its pixel dimensions.
    const originalImg = thumbEl;
    if (!originalImg.complete || !originalImg.naturalWidth) {
      // Thumb hasn't loaded yet (race condition). Fall back to handwriting-only
      // — worse than the composite, but at least the submit won't fail.
      return canvas.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
    }
    const origW = originalImg.naturalWidth;
    const origH = originalImg.naturalHeight;

    // 2. Compose onto an off-screen canvas.
    //    Layout: top half = screenshot (letterboxed to a max width),
    //            separator,
    //            bottom half = handwriting canvas (also letterboxed to the
    //            same max width so they line up nicely).
    const SEP_HEIGHT = 8;
    const MAX_TOTAL_WIDTH = 1600;   // cap so we don't send huge composites to the API

    // Scale each section down if it would exceed the max width.
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

    // Solid white background — multimodal OCR is most reliable on white.
    cctx.fillStyle = "#ffffff";
    cctx.fillRect(0, 0, compW, compH);

    // 3. Paint the screenshot, centered horizontally.
    cctx.drawImage(
      originalImg,
      (compW - origFit.w) / 2, 0,   // dst
      origFit.w, origFit.h,
    );

    // 4. Thin separator rule.
    cctx.fillStyle = "#cccccc";
    cctx.fillRect(0, origFit.h, compW, SEP_HEIGHT);

    // 5. Paint the handwriting canvas onto a white background, centered.
    //    The user's canvas has transparent background; we first draw a
    //    white rect into the destination area so transparency shows up as
    //    paper, not the renderer's checkered pattern.
    cctx.fillStyle = "#ffffff";
    cctx.fillRect(
      (compW - inkFit.w) / 2,
      origFit.h + SEP_HEIGHT,
      inkFit.w,
      inkFit.h,
    );
    cctx.drawImage(
      canvas,
      (compW - inkFit.w) / 2, origFit.h + SEP_HEIGHT,  // dst
      inkFit.w, inkFit.h,
    );

    return comp.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
  }

  // ── Wire Tauri events ─────────────────────────────────────────────────
  function bindBackend() {
    if (!t || !t.event) {
      return setTimeout(bindBackend, 30);
    }
    // Backend pushes the captured screenshot right when the ink window opens.
    t.event.listen("ink:start", (event) => {
      const p = event.payload || {};
      if (p.imageBase64) {
        thumbEl.src = "data:image/png;base64," + p.imageBase64;
      }
      clearCanvas();
      setStatus("info", "请手写提问");
      canvas.focus();
    });
    t.event.listen("ink:busy", (event) => {
      const p = event.payload || {};
      busy = true;
      confirmBtn.disabled = true;
      clearBtn.disabled = true;
      // The backend now does OCR + QA in a single LLM call, so we only
      // show one busy stage.
      setStatus("info", "提问中...");
    });
    t.event.listen("ink:done", (event) => {
      const p = event.payload || {};
      busy = false;
      confirmBtn.disabled = false;
      clearBtn.disabled = false;
      setStatus("ok", "✓ 已回答");
      // recognizedText is empty in the single-step flow (model does OCR
      // internally and returns the answer); we hide the recognized-text
      // pill so the UI doesn't display an empty chip.
      recognizedEl.hidden = true;
      recognizedEl.textContent = "";
    });
    t.event.listen("ink:error", (event) => {
      const p = event.payload || {};
      busy = false;
      confirmBtn.disabled = !hasContent;
      clearBtn.disabled = false;
      setStatus("error", "失败: " + (p.error || "未知错误"));
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
  cancelBtn.addEventListener("click", closeWindow);

  confirmBtn.addEventListener("click", async () => {
    if (busy || !hasContent) return;
    busy = true;
    confirmBtn.disabled = true;
    clearBtn.disabled = true;
    setStatus("info", "准备发送...");
    try {
      const pngBase64 = encodeCanvasPng();
      await t.core.invoke("submit_ink_question", { canvasPngBase64: pngBase64 });
      // ink:done event will reset busy/disabled and update the status.
    } catch (e) {
      busy = false;
      confirmBtn.disabled = false;
      clearBtn.disabled = false;
      setStatus("error", "提交失败: " + (e?.message || e));
    }
  });

  // Esc closes the window (matches the rest of the app).
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") closeWindow();
  });

  bindBackend();
})();