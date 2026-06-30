# rs-sc — Rust Screen Capture

> A minimalist Windows screenshot translator: hotkey → box-select → AI-vision translate → floating result.
> Built with Rust + Tauri 2 + native winit overlay — fast, small, and ~20 MB of binary.

## What it does

1. Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>T</kbd> (customizable)
2. The screen dims; drag a rectangle around the text you want translated
3. Release the mouse — the selection is sent to any OpenAI-compatible multimodal model
4. A floating window pops up next to your cursor with the translation (image + text, with copy button)

That's it. No UI chrome, no settings panels to discover for the main flow. Settings live behind a tray icon and a second <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>P</kbd> hotkey.

## Why

Built because every translate-on-screenshot tool I tried either:
- wanted to be a fullscreen app (ShareX / PowerToy with 100 features you don't use)
- required a paid API key upfront (no easy way to point at DeepSeek / Ollama)
- leaked an extra second to "wake up" before showing the result

`rs-sc` is one shortcut and a single network call. The result window is brought up *before* the AI request is fired, so even on a slow network you get immediate visual confirmation that the work has started.

## Features

- 🖱️ **Native fullscreen overlay** — `winit` + `softbuffer` draws directly to the OS window; no WebView overhead, no white-flash on first paint
- ⚡ **Single capture** — the bitmap stays in memory from screen capture through cropping; no second BitBlt roundtrip
- 🌐 **Any OpenAI-compatible API** — OpenAI, DeepSeek, Azure OpenAI, Ollama, custom proxies — just point the Base URL
- ⌨️ **Two hotkeys, both configurable** from the settings panel: capture and open-settings
- 💾 **Persistent config** — JSON under `%APPDATA%\rs-sc\settings.json` (atomic write)
- 🎯 **Hot-swap everything** — save new API settings or hotkey mid-session; no restart
- 📋 **Copy translation with one click**
- 🪟 **Tray icon** with native right-click menu (show / settings / quit)

## Quick start

```bash
# Prerequisites: Rust toolchain + Tauri CLI
cargo install tauri-cli --version "^2"

# Clone and run
git clone https://github.com/ZHLX2005/rs-sc
cd rs-sc/src-tauri
cargo tauri dev
```

On first launch the app reads `Ollama` defaults (`http://127.0.0.1:11434/v1/chat/completions`). Right-click the tray icon → **设置…** to point at OpenAI / DeepSeek / Azure / anything else that follows the `/chat/completions` schema.

## Configuration

Either edit in the settings panel (recommended) or set environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `RSSC_API_BASE` | `http://127.0.0.1:11434/v1/chat/completions` | Any OpenAI-compatible `/chat/completions` URL |
| `RSSC_API_KEY` | _(empty)_ | Bearer token; empty for unauthenticated local APIs |
| `RSSC_MODEL` | `gpt-4o-mini` | Multimodal model name (vision-capable) |
| `RSSC_PROMPT` | "你是一名专业翻译…" | System instruction sent with the image |
| `RSSC_HOTKEY` | `CommandOrControl+Shift+T` | Capture hotkey (Tauri accelerator syntax) |
| `RSSC_SETTINGS_HOTKEY` | `CommandOrControl+Shift+P` | Open-settings hotkey |

Resolution order: **settings file → environment variable → hard-coded default**. The settings panel takes precedence once you've saved anything.

Config file location:
- Windows: `%APPDATA%\rs-sc\settings.json`
- macOS: `~/Library/Application Support/rs-sc/settings.json`
- Linux: `~/.config/rs-sc/settings.json`

## Architecture

```
┌─────────────────────────┐
│  Ctrl+Shift+T (tauri-plugin-global-shortcut)
└────────────┬────────────┘
             │
             ▼
┌─────────────────────────┐
│  BitBlt screenshot (screenshots crate)
│  → Arc<Vec<u8>> RGBA    │
└────────────┬────────────┘
             │
   ┌─────────┴──────────┐
   │  winit + softbuffer fullscreen overlay
   │  - darkens the screen
   │  - paints the original into the drag rect
   │  - closes ITSELF on mouse-up (immediate)
   │  - emits Selection { x, y, w, h } over mpsc
   └─────────┬──────────┘
             │
             ▼
┌─────────────────────────┐
│  crop_rgba + encode_png │
│  (image crate)          │
└────────────┬────────────┘
             │
       ┌─────┴─────────────────────┐
       │  Show result window with  │ ← Immediate visual feedback
       │  "translating…" state    │   (appears in ~50 ms)
       └─────┬─────────────────────┘
             │
             ▼
┌─────────────────────────┐
│  POST to /chat/completions
│  (reqwest + base64 image_url)
└────────────┬────────────┘
             │
             ▼
┌─────────────────────────┐
│  Emit result:loaded →   │
│  window fills in text   │
└─────────────────────────┘
```

Two design choices worth calling out:

**1. The winit overlay closes itself the instant the user releases the mouse.** The previous version waited for the next pipeline stage; this version drops the dim overlay first and only then starts the AI call. Combined with showing the result window before the network round-trip, the entire post-selection flow feels instant.

**2. The full RGBA buffer lives in an `Arc<Vec<u8>>` shared between the overlay (for painting the un-dimmed rectangle) and the main task (for cropping the selection).** The first version recaptured the screen a second time to get the pixels — a 50-100 ms BitBlt we now save entirely.

## Hotkeys

The two defaults can be edited in the settings panel or via `RSSC_HOTKEY` / `RSSC_SETTINGS_HOTKEY`.

The two hotkeys must differ; the settings panel and the backend both reject saving them as the same value.

Format: Tauri accelerator syntax. Modifiers joined with `+`, then the key:
- `CommandOrControl+Shift+T` — Ctrl on Windows/Linux, Cmd on macOS
- `Alt+Shift+Q`
- `Ctrl+F1` through `F12`
- `CommandOrControl+Space`

The settings panel has a **录制 (record)** button — click it and press the combo you want.

## Tech stack

| Layer | Choice | Why |
|---|---|---|
| Shell | **Tauri 2** | Small binary (≈20 MB), Rust backend, system WebView |
| Global hotkey | `tauri-plugin-global-shortcut` | Cross-platform, dynamic register/unregister |
| Single instance | `tauri-plugin-single-instance` | Don't spam a tray icon when re-launched |
| Screen capture | `screenshots` crate | BitBlt on Windows, pure Rust, no native deps |
| Overlay | `winit` + `softbuffer` | Native window with direct pixel buffer — no WebView overhead |
| Image encode | `image` crate | PNG for multimodal APIs |
| HTTP | `reqwest` | With `rustls-tls` so we don't need OpenSSL on Windows |
| Frontend | Vanilla HTML/CSS/JS | Smallest possible — no framework, no build step |

## Building from source

```bash
# Debug build
cd src-tauri
cargo build
./target/debug/rs-sc.exe

# Release build (smaller, faster)
cargo build --release
./target/release/rs-sc.exe

# Bundled installer (.msi / .exe / .dmg / .deb)
cargo install tauri-cli --version "^2"
cargo tauri build
# Output in src-tauri/target/release/bundle/
```

## Project layout

```
rs-sc/
├── src-tauri/                # Rust backend + Tauri config
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── build.rs
│   ├── capabilities/default.json
│   ├── icons/                # App icons
│   └── src/
│       ├── main.rs           # Entry point, hotkey, tray, commands, pipeline
│       ├── capture.rs        # Screen capture (BitBlt via screenshots crate)
│       ├── capture_window.rs # winit + softbuffer native overlay
│       ├── llm_translate.rs  # Multimodal OpenAI-compatible client
│       ├── settings.rs       # Persistent settings (JSON, atomic write)
│       ├── models.rs
│       └── error.rs
└── ui/                       # Vanilla HTML/CSS/JS frontend
    ├── index.html            # (redirect to result.html)
    ├── result.html           # Translation result window
    ├── result.js
    ├── settings.html         # Settings panel
    ├── settings.js
    └── styles.css
```

## Contributing

Issues and PRs welcome. Two natural next steps:

1. **OCR fallback** — fall back to a local OCR (e.g. `tesseract` crate) when the AI endpoint fails, so the workflow degrades gracefully offline
2. **Multi-monitor handling** — the current implementation targets the monitor under the cursor at capture time; the pipeline could let the user pick which monitor after capture
3. **Linux support** — `winit` + `screenshots` work on Wayland/X11 already; the only platform-specific code is the cursor-position helper. Add `#cfg(target_os = "linux")` paths in `capture.rs`

## License

MIT.

---

Inspired by [Glance](https://github.com/Harukaon/Glance) (which proved the architecture) but stripped down to one shortcut + one tray icon + ~700 LOC of Rust.
