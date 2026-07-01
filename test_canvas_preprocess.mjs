// Quick standalone test for the **composite** canvas pipeline.
// Run with: node test_canvas_preprocess.js
//
// We emulate the browser canvas API with @napi-rs/canvas. The script:
//   1. Creates a screenshot image (synthetic 600x300 with English sample text)
//   2. Draws a "hello" string on a second canvas (the user's handwriting)
//   3. Composes them vertically with a separator rule (white BG, no binarize)
//   4. Saves the result to test_output/ so the user can eyeball it
//
// This is the production pipeline as of the single-step ink flow — the
// backend no longer does separate OCR; the model receives this single
// composite and does OCR+QA in one shot.

import fs from "fs";
import path from "path";

// Always write test outputs to ./test_output (next to the script) so they're
// easy to find on Windows.
const OUT_DIR = path.join(process.cwd(), "test_output");
fs.mkdirSync(OUT_DIR, { recursive: true });

const MAX_TOTAL_WIDTH = 1600; // matches ui/ink.js
const SEP_HEIGHT = 8;          // matches ui/ink.js

function fit(w, h, maxW) {
  if (w <= maxW) return { w, h };
  const scale = maxW / w;
  return { w: maxW, h: Math.round(h * scale) };
}

// Try the real canvas binding first.
let realCanvas = null;
try {
  const { createCanvas } = await import("@napi-rs/canvas");
  realCanvas = createCanvas;
  console.log("[test] using @napi-rs/canvas for realistic drawing");
} catch (e) {
  console.log("[test] @napi-rs/canvas not available — install with: npm i @napi-rs/canvas");
  process.exit(0);
}

if (realCanvas) {
  // ── Step 1: synthetic \"screenshot\" — a white background with some text ──
  const SC_W = 600, SC_H = 200;
  const screenshot = realCanvas(SC_W, SC_H);
  const sctx = screenshot.getContext("2d");
  sctx.fillStyle = "#ffffff";
  sctx.fillRect(0, 0, SC_W, SC_H);
  sctx.fillStyle = "#222";
  sctx.font = "bold 28px sans-serif";
  sctx.fillText("Hello world. This is a test.", 30, 50);
  sctx.font = "16px sans-serif";
  sctx.fillText("(this is the screenshot the user took)", 30, 90);
  const screenshotBuf = screenshot.toBuffer("image/png");
  fs.writeFileSync(path.join(OUT_DIR, "test_composite_screenshot.png"), screenshotBuf);
  console.log(`[test] wrote test_output/test_composite_screenshot.png (${SC_W}x${SC_H})`);

  // ── Step 2: synthetic \"handwriting\" canvas — transparent with text ──
  const INK_W = 800, INK_H = 240;
  const ink = realCanvas(INK_W, INK_H);
  const ictx = ink.getContext("2d");
  ictx.lineCap = "round";
  ictx.lineJoin = "round";
  ictx.strokeStyle = "#111";
  // Hand-drawn \"你 好\"
  // 你
  ictx.beginPath();
  ictx.moveTo(80, 80);
  ictx.lineTo(80, 30);
  ictx.lineTo(80, 80);
  ictx.lineWidth = 10;
  ictx.stroke();
  ictx.beginPath();
  ictx.moveTo(80, 80);
  ictx.lineTo(140, 80);
  ictx.lineTo(140, 170);
  ictx.lineTo(80, 170);
  ictx.lineWidth = 10;
  ictx.stroke();
  // 好
  ictx.beginPath();
  ictx.moveTo(220, 60);
  ictx.lineTo(220, 170);
  ictx.lineWidth = 10;
  ictx.stroke();
  ictx.beginPath();
  ictx.moveTo(220, 110);
  ictx.lineTo(300, 110);
  ictx.lineWidth = 10;
  ictx.stroke();
  ictx.beginPath();
  ictx.moveTo(260, 110);
  ictx.lineTo(260, 200);
  ictx.lineWidth = 10;
  ictx.stroke();
  ictx.beginPath();
  ictx.moveTo(200, 200);
  ictx.lineTo(300, 200);
  ictx.lineWidth = 10;
  ictx.stroke();
  const inkBuf = ink.toBuffer("image/png");
  fs.writeFileSync(path.join(OUT_DIR, "test_composite_ink.png"), inkBuf);
  console.log(`[test] wrote test_output/test_composite_ink.png (${INK_W}x${INK_H})`);

  // ── Step 3: compose vertically (screenshot on top, ink on bottom) ──
  const scFit = fit(SC_W, SC_H, MAX_TOTAL_WIDTH);
  const inkFit = fit(INK_W, INK_H, MAX_TOTAL_WIDTH);
  const compW = Math.max(scFit.w, inkFit.w);
  const compH = scFit.h + SEP_HEIGHT + inkFit.h;

  const comp = realCanvas(compW, compH);
  const cctx = comp.getContext("2d");
  cctx.fillStyle = "#ffffff";
  cctx.fillRect(0, 0, compW, compH);

  cctx.drawImage(
    screenshot,
    (compW - scFit.w) / 2, 0,
    scFit.w, scFit.h,
  );
  cctx.fillStyle = "#cccccc";
  cctx.fillRect(0, scFit.h, compW, SEP_HEIGHT);
  cctx.fillStyle = "#ffffff";
  cctx.fillRect(
    (compW - inkFit.w) / 2,
    scFit.h + SEP_HEIGHT,
    inkFit.w, inkFit.h,
  );
  cctx.drawImage(
    ink,
    (compW - inkFit.w) / 2, scFit.h + SEP_HEIGHT,
    inkFit.w, inkFit.h,
  );

  const compBuf = comp.toBuffer("image/png");
  fs.writeFileSync(path.join(OUT_DIR, "test_composite.png"), compBuf);
  console.log(`[test] wrote test_output/test_composite.png (${compW}x${compH}, ${compBuf.length} bytes)`);
  console.log(`[test] layout:`);
  console.log(`[test]   screenshot: ${scFit.w}x${scFit.h} (top, white BG, English text)`);
  console.log(`[test]   separator:  ${SEP_HEIGHT}px gray rule`);
  console.log(`[test]   handwriting: ${inkFit.w}x${inkFit.h} (bottom, white BG, 你好)`);
  console.log(`[test] This is exactly the image the LLM receives.`);
  console.log(`[test] The model should see English text on top + 你好 on bottom and answer in one shot.`);
}
