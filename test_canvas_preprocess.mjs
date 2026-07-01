// Quick standalone test for the canvas-preprocessing pipeline.
// Run with: node test_canvas_preprocess.js
//
// We emulate the browser canvas API with @napi-rs/canvas to actually
// draw strokes, run the same preprocessing, and save the result as
// a PNG you can eyeball in /tmp. If you don't have @napi-rs/canvas
// installed, this script falls back to a pure-JS bounding-box sanity
// check that exercises the math.

import fs from "fs";
import path from "path";

// Always write test outputs to ./test_output (next to the script) so they're
// easy to find on Windows.
const OUT_DIR = path.join(process.cwd(), "test_output");
fs.mkdirSync(OUT_DIR, { recursive: true });

const PRE_THRESHOLD = 240; // matches ink.js
const BIN_THRESHOLD = 180; // matches ink.js
const SCALE = 2;           // matches ink.js
const PAD_RATIO = 0.15;    // matches ink.js

function findContentBBox(px, w, h) {
  let minX = w, minY = h, maxX = -1, maxY = -1;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 4;
      const a = px[i + 3];
      if (a < 8) continue;
      const r = px[i], g = px[i + 1], b = px[i + 2];
      const lum = (r * 299 + g * 587 + b * 114) / 1000;
      if (lum < PRE_THRESHOLD) {
        if (x < minX) minX = x;
        if (x > maxX) maxX = x;
        if (y < minY) minY = y;
        if (y > maxY) maxY = y;
      }
    }
  }
  return { minX, minY, maxX, maxY };
}

function binarize(imageData) {
  const px = imageData.data;
  for (let i = 0; i < px.length; i += 4) {
    const lum = (px[i] * 299 + px[i + 1] * 587 + px[i + 2] * 114) / 1000;
    const v = lum < BIN_THRESHOLD ? 0 : 255;
    px[i] = v;
    px[i + 1] = v;
    px[i + 2] = v;
    px[i + 3] = 255;
  }
}

// Try the real canvas binding first.
let realCanvas = null;
try {
  const { createCanvas } = await import("@napi-rs/canvas");
  realCanvas = createCanvas;
  console.log("[test] using @napi-rs/canvas for realistic drawing");
} catch (e) {
  console.log("[test] @napi-rs/canvas not available — running pure-JS bbox test only");
}

if (realCanvas) {
  // ── Real test: draw \"hello\" on a transparent canvas, run the pipeline,
  // save before/after PNGs to /tmp.
  const W = 800, H = 400;
  const canvas = realCanvas(W, H);
  const ctx = canvas.getContext("2d");
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  ctx.strokeStyle = "#111";

  // Hand-drawn \"hello\" — six rough strokes.
  // h
  ctx.beginPath();
  ctx.moveTo(50, 300);
  ctx.lineTo(50, 200);
  ctx.lineTo(50, 300);
  ctx.lineTo(120, 230);
  ctx.lineTo(120, 300);
  ctx.lineWidth = 12;
  ctx.stroke();
  // e
  ctx.beginPath();
  ctx.moveTo(180, 260);
  ctx.lineTo(220, 230);
  ctx.lineTo(240, 250);
  ctx.lineTo(220, 280);
  ctx.lineTo(180, 280);
  ctx.lineTo(180, 310);
  ctx.lineTo(240, 310);
  ctx.lineWidth = 12;
  ctx.stroke();
  // l
  ctx.beginPath();
  ctx.moveTo(280, 200);
  ctx.lineTo(280, 320);
  ctx.lineWidth = 12;
  ctx.stroke();
  // l
  ctx.beginPath();
  ctx.moveTo(320, 200);
  ctx.lineTo(320, 320);
  ctx.lineWidth = 12;
  ctx.stroke();
  // o
  ctx.beginPath();
  ctx.moveTo(400, 270);
  ctx.bezierCurveTo(360, 240, 360, 320, 400, 300);
  ctx.bezierCurveTo(440, 320, 440, 240, 400, 270);
  ctx.lineWidth = 12;
  ctx.stroke();

  // Save \"before\" so we can see what we sent
  const beforeBuf = canvas.toBuffer("image/png");
  fs.writeFileSync("test_output/test_ink_before.png", beforeBuf);
  console.log(`[test] wrote test_output/test_ink_before.png (${beforeBuf.length} bytes, ${W}x${H})`);

  // Run the pipeline (this is a faithful port of ink.js)
  const img = ctx.getImageData(0, 0, W, H);
  const { minX, minY, maxX, maxY } = findContentBBox(img.data, W, H);
  console.log(`[test] content bbox: (${minX},${minY}) - (${maxX},${maxY})`);

  if (maxX < 0) throw new Error("no content detected");

  const contentW = maxX - minX + 1;
  const contentH = maxY - minY + 1;
  const pad = Math.max(20, Math.round(Math.min(contentW, contentH) * PAD_RATIO));
  const cropX = Math.max(0, minX - pad);
  const cropY = Math.max(0, minY - pad);
  const cropW = Math.min(W - cropX, contentW + pad * 2);
  const cropH = Math.min(H - cropY, contentH + pad * 2);
  console.log(`[test] crop: (${cropX},${cropY}) ${cropW}x${cropH}  pad=${pad}`);

  // Off-screen canvas at 2x
  const out = realCanvas(cropW * SCALE, cropH * SCALE);
  const octx = out.getContext("2d");
  octx.fillStyle = "#ffffff";
  octx.fillRect(0, 0, out.width, out.height);
  octx.imageSmoothingEnabled = false;
  octx.drawImage(canvas, cropX, cropY, cropW, cropH, 0, 0, out.width, out.height);
  const outImg = octx.getImageData(0, 0, out.width, out.height);
  binarize(outImg);
  octx.putImageData(outImg, 0, 0);

  const afterBuf = out.toBuffer("image/png");
  fs.writeFileSync("test_output/test_ink_after.png", afterBuf);
  console.log(`[test] wrote test_output/test_ink_after.png (${afterBuf.length} bytes, ${out.width}x${out.height})`);
  console.log(`[test] before/after ratio: ${(afterBuf.length / beforeBuf.length).toFixed(2)}x`);
  console.log(`[test] image area ratio: ${((out.width * out.height) / (W * H)).toFixed(2)}x`);
}

// ── Pure-JS sanity checks: math + logic, no actual drawing. ──
function assertEqual(actual, expected, msg) {
  if (actual !== expected) {
    throw new Error(`FAIL: ${msg} — expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
  console.log(`  ok: ${msg}`);
}

console.log("\n[test] running pure-JS unit checks:");

// Test 1: bbox detection on a tiny synthetic image
{
  const W = 10, H = 10;
  // Build a 10x10 image with content in rows 2..4, cols 3..7
  // (alpha=255, rgb=0). All other pixels alpha=0.
  const px = new Uint8ClampedArray(W * H * 4);
  for (let y = 2; y <= 4; y++) {
    for (let x = 3; x <= 7; x++) {
      const i = (y * W + x) * 4;
      px[i] = 0; px[i+1] = 0; px[i+2] = 0; px[i+3] = 255;
    }
  }
  const bbox = findContentBBox(px, W, H);
  assertEqual(bbox.minX, 3, "bbox.minX");
  assertEqual(bbox.minY, 2, "bbox.minY");
  assertEqual(bbox.maxX, 7, "bbox.maxX");
  assertEqual(bbox.maxY, 4, "bbox.maxY");
}

// Test 2: bbox ignores transparent pixels
{
  const W = 5, H = 5;
  const px = new Uint8ClampedArray(W * H * 4);
  // Mark (2,2) as fully transparent black — should NOT count
  const i = (2 * W + 2) * 4;
  px[i] = 0; px[i+1] = 0; px[i+2] = 0; px[i+3] = 0;
  // Mark (3,3) as opaque dark — SHOULD count
  const j = (3 * W + 3) * 4;
  px[j] = 0; px[j+1] = 0; px[j+2] = 0; px[j+3] = 255;
  const bbox = findContentBBox(px, W, H);
  assertEqual(bbox.maxX, 3, "transparent ignored");
  assertEqual(bbox.maxY, 3, "transparent ignored");
}

// Test 3: binarization — uses strict < threshold (180)
{
  const img = { data: new Uint8ClampedArray([
    100, 100, 100, 255,  // lum=100 < 180 → black
    179, 179, 179, 255,  // lum=179 < 180 → black (just below threshold)
    180, 180, 180, 255,  // lum=180, NOT < 180 → white (boundary is exclusive)
    200, 200, 200, 255,  // lum=200 → white
    255, 255, 255, 255,  // lum=255 → white (unchanged)
  ]) };
  binarize(img);
  // Pixel 0
  assertEqual(img.data[0], 0, "100 → black");
  // Pixel 1
  assertEqual(img.data[4], 0, "179 → black (just below threshold)");
  // Pixel 2 — exclusive boundary
  assertEqual(img.data[8], 255, "180 → white (exclusive boundary)");
  // Pixel 3
  assertEqual(img.data[12], 255, "200 → white");
  // Pixel 4
  assertEqual(img.data[16], 255, "255 → white");
}

console.log("\n[test] all checks passed");
