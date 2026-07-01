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
    // OCR-friendly preprocessing pipeline:
    //
    //   raw canvas (transparent BG, anti-aliased greyscale strokes)
    //     ↓ 1. read pixel data
    //     ↓ 2. find content bounding box (anything visibly drawn)
    //     ↓ 3. crop to bbox with padding
    //     ↓ 4. paint onto off-screen canvas: WHITE background + 2x upscale
    //     ↓ 5. threshold each pixel → pure black or pure white
    //     ↓ 6. export as PNG
    //
    // Why this matters: handwriting samples submitted to multimodal LLMs
    // recognize much better when they're:
    //   (a) on a solid white background (no alpha-channel ambiguity)
    //   (b) tightly cropped (no wasted whitespace the model has to ignore)
    //   (c) high-resolution (smaller crops → bigger relative feature size)
    //   (d) high-contrast b/w (LLM OCRs trained on scanned docs, not
    //       anti-aliased greyscale pen strokes)
    //
    // The whole pipeline runs in <10ms on the user's machine — no extra
    // Rust / server-side work.
    const w = canvas.width;
    const h = canvas.height;
    if (w === 0 || h === 0) return "";

    let src;
    try {
      src = ctx.getImageData(0, 0, w, h);
    } catch (e) {
      // getImageData can throw on a tainted canvas (cross-origin image).
      // Our canvas is fully local, so this shouldn't fire, but we fall
      // back to the un-preprocessed image rather than failing the user's
      // submission.
      return canvas.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
    }
    const px = src.data;

    // 1. Find the content bounding box. A pixel is "ink" if its alpha is
    //    > 8 (the user's stroke is at least faintly visible) and its
    //    greyscale is darker than 240 (i.e. it's not background white).
    //    We accept "somewhat transparent" strokes because a thin pen
    //    tip often renders as alpha=30-80 in the anti-aliasing fringe.
    let minX = w, minY = h, maxX = -1, maxY = -1;
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const i = (y * w + x) * 4;
        const a = px[i + 3];
        if (a < 8) continue; // transparent — definitely background
        const r = px[i], g = px[i + 1], b = px[i + 2];
        // weighted luminance (ITU-R BT.601)
        const lum = (r * 299 + g * 587 + b * 114) / 1000;
        if (lum < 240) {
          if (x < minX) minX = x;
          if (x > maxX) maxX = x;
          if (y < minY) minY = y;
          if (y > maxY) maxY = y;
        }
      }
    }

    // No content? Send a tiny blank PNG so the backend gets a valid image
    // (the user clicked confirm with an empty canvas, but UI already
    // disables confirm in that case — this is just defensive).
    if (maxX < 0) {
      return canvas.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
    }

    // 2. Crop with padding so the OCR model sees whitespace around
    //    characters (improves recognition rate). Pad proportionally
    //    to the smaller dimension.
    const contentW = maxX - minX + 1;
    const contentH = maxY - minY + 1;
    const pad = Math.max(20, Math.round(Math.min(contentW, contentH) * 0.15));
    let cropX = Math.max(0, minX - pad);
    let cropY = Math.max(0, minY - pad);
    let cropW = Math.min(w - cropX, contentW + pad * 2);
    let cropH = Math.min(h - cropY, contentH + pad * 2);
    // Ensure crop width / height are positive
    cropW = Math.max(1, cropW);
    cropH = Math.max(1, cropH);

    // 3. Create an off-screen canvas at 2x scale, with WHITE background
    //    so the LLM has unambiguous input.
    const SCALE = 2;
    const out = document.createElement("canvas");
    out.width = cropW * SCALE;
    out.height = cropH * SCALE;
    const octx = out.getContext("2d");
    octx.fillStyle = "#ffffff";
    octx.fillRect(0, 0, out.width, out.height);

    // 4. Draw the cropped source region onto the off-screen canvas at 2x.
    //    We use imageSmoothingEnabled = false so the upscale is crisp
    //    (nearest-neighbor) — important for tiny pen-tip pixels.
    octx.imageSmoothingEnabled = false;
    octx.drawImage(
      canvas,
      cropX, cropY, cropW, cropH,   // source rect
      0, 0, out.width, out.height,   // dest rect (2x)
    );

    // 5. Binarize — threshold each pixel. The alpha is already settled
    //    (we drew onto a white BG, no transparency), so we just look at
    //    luminance and snap to black-or-white.
    const outImg = octx.getImageData(0, 0, out.width, out.height);
    const opx = outImg.data;
    for (let i = 0; i < opx.length; i += 4) {
      const lum = (opx[i] * 299 + opx[i + 1] * 587 + opx[i + 2] * 114) / 1000;
      // 180 is the sweet spot for handwritten strokes: darker than this
      // is ink (write black), lighter is paper (write white). Tuned to
      // match what scanned-document OCR training sets expect.
      const v = lum < 180 ? 0 : 255;
      opx[i] = v;
      opx[i + 1] = v;
      opx[i + 2] = v;
      opx[i + 3] = 255;
    }
    octx.putImageData(outImg, 0, 0);

    return out.toDataURL("image/png").replace(/^data:image\/png;base64,/, "");
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
      if (p.stage === "ocr") setStatus("info", "识别手写中...");
      else if (p.stage === "qa") setStatus("info", "基于截图回答中...");
      else setStatus("info", "处理中...");
    });
    t.event.listen("ink:done", (event) => {
      const p = event.payload || {};
      busy = false;
      confirmBtn.disabled = false;
      clearBtn.disabled = false;
      setStatus("ok", "✓ 已回答");
      if (p.recognizedText) {
        recognizedEl.hidden = false;
        recognizedEl.textContent = "识别: " + p.recognizedText;
      }
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