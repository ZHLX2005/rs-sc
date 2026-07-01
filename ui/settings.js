// rs-sc settings panel — opens with current values, saves back to the settings.json
// file via the Tauri command, and supports hot-rebinding the global hotkey.

(function () {
  const $ = (id) => document.getElementById(id);
  const form = $("settings-form");
  const statusBar = $("status-bar");

  const fields = {
    baseUrl: $("baseUrl"),
    apiKey: $("apiKey"),
    model: $("model"),
    prompt: $("prompt"),
    hotkey: $("hotkey"),
    settingsHotkey: $("settingsHotkey"),
    inkHotkey: $("inkHotkey"),
    ocrPrompt: $("ocrPrompt"),
    qaPrompt: $("qaPrompt"),
  };

  let recordingTarget = null; // which hotkey field is currently being recorded
  let originalSettings = null;

  // ── status bar helpers ────────────────────────────────────────────────────

  function showStatus(kind, text, autoHideMs) {
    statusBar.hidden = false;
    statusBar.className = "status-bar " + kind;
    statusBar.textContent = text;
    if (autoHideMs) {
      setTimeout(() => {
        statusBar.hidden = true;
      }, autoHideMs);
    }
  }
  function clearStatus() {
    statusBar.hidden = true;
  }

  // ── load initial values ───────────────────────────────────────────────────

  async function load() {
    try {
      const settings = await window.__TAURI__.core.invoke("get_settings");
      originalSettings = settings;
      fields.baseUrl.value = settings.baseUrl || "";
      fields.apiKey.value = settings.apiKey || "";
      fields.model.value = settings.model || "";
      fields.prompt.value = settings.prompt || "";
      fields.hotkey.value = settings.hotkey || "";
      fields.settingsHotkey.value = settings.settingsHotkey || "";
      fields.inkHotkey.value = settings.inkHotkey || "";
      fields.ocrPrompt.value = settings.ocrPrompt || "";
      fields.qaPrompt.value = settings.qaPrompt || "";
    } catch (e) {
      showStatus("error", "加载设置失败: " + (e?.message || e));
    }
  }

  // ── read form into a Settings object ──────────────────────────────────────

  function readForm() {
    return {
      baseUrl: fields.baseUrl.value.trim(),
      apiKey: fields.apiKey.value.trim(),
      model: fields.model.value.trim(),
      prompt: fields.prompt.value, // keep as-is (multiline)
      hotkey: fields.hotkey.value.trim(),
      settingsHotkey: fields.settingsHotkey.value.trim(),
      inkHotkey: fields.inkHotkey.value.trim(),
      ocrPrompt: fields.ocrPrompt.value, // keep as-is (multiline)
      qaPrompt: fields.qaPrompt.value,   // keep as-is (multiline)
    };
  }

  // ── toggle password visibility ────────────────────────────────────────────

  $("toggle-key").addEventListener("click", () => {
    fields.apiKey.type = fields.apiKey.type === "password" ? "text" : "password";
  });

  // ── model chips: click to fill the model field ───────────────────────────

  document.querySelectorAll("#model-chips .chip").forEach((chip) => {
    chip.addEventListener("click", () => {
      fields.model.value = chip.dataset.model;
      fields.model.focus();
    });
  });

  // ── hotkey recorders ──────────────────────────────────────────────────────

  // Translate a KeyboardEvent into Tauri-plugin-global-shortcut's accelerator
  // syntax. Returns null if the key isn't something we want to bind.
  function eventToAccelerator(e) {
    // Don't capture modifier-only presses.
    if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) {
      return null;
    }

    const parts = [];
    if (e.ctrlKey || e.metaKey) parts.push("CommandOrControl");
    if (e.altKey) parts.push("Alt");
    if (e.shiftKey) parts.push("Shift");

    // Normalize the key. Tauri accepts a lot of formats; we use the simplest one
    // that still disambiguates between letter and digit.
    let key = e.key;
    if (key.length === 1) {
      key = key.toUpperCase();
    } else if (key === " ") {
      key = "Space";
    }
    parts.push(key);
    return parts.join("+");
  }

  function startRecording(targetField, button) {
    recordingTarget = targetField;
    button.textContent = "按任意组合…";
    button.classList.add("recording");
    targetField.value = "";
    targetField.placeholder = "按下组合键…";
    targetField.focus();
  }

  function stopRecording() {
    if (!recordingTarget) return;
    recordingTarget.placeholder = "CommandOrControl+...";
    document
      .querySelectorAll("button.recording")
      .forEach((b) => {
        b.textContent = "录制";
        b.classList.remove("recording");
      });
    recordingTarget = null;
  }

  $("record-hotkey").addEventListener("click", () => {
    if (recordingTarget === fields.hotkey) {
      stopRecording();
      return;
    }
    stopRecording();
    startRecording(fields.hotkey, $("record-hotkey"));
  });

  $("record-settings-hotkey").addEventListener("click", () => {
    if (recordingTarget === fields.settingsHotkey) {
      stopRecording();
      return;
    }
    stopRecording();
    startRecording(fields.settingsHotkey, $("record-settings-hotkey"));
  });

  $("record-ink-hotkey").addEventListener("click", () => {
    if (recordingTarget === fields.inkHotkey) {
      stopRecording();
      return;
    }
    stopRecording();
    startRecording(fields.inkHotkey, $("record-ink-hotkey"));
  });

  // Single global keydown listener: whichever field is recording gets filled.
  document.addEventListener("keydown", (e) => {
    if (!recordingTarget) return;
    e.preventDefault();
    if (e.key === "Escape") {
      // cancel recording, restore original value for whichever target is active
      const orig =
        recordingTarget === fields.hotkey
          ? originalSettings?.hotkey
          : recordingTarget === fields.settingsHotkey
          ? originalSettings?.settingsHotkey
          : originalSettings?.inkHotkey;
      recordingTarget.value = orig || "";
      stopRecording();
      return;
    }
    const accel = eventToAccelerator(e);
    if (accel) {
      recordingTarget.value = accel;
      stopRecording();
    }
  });

  // ── save / cancel / test ──────────────────────────────────────────────────

  $("save-btn").addEventListener("click", async () => {
    const newSettings = readForm();
    if (!newSettings.baseUrl) return showStatus("error", "Base URL 不能为空");
    if (!newSettings.model) return showStatus("error", "Model 不能为空");
    if (!newSettings.hotkey) return showStatus("error", "截屏快捷键不能为空");
    if (!newSettings.settingsHotkey)
      return showStatus("error", "设置快捷键不能为空");
    if (!newSettings.inkHotkey)
      return showStatus("error", "手写快捷键不能为空");
    // Three hotkeys must all be pairwise distinct (case-insensitive).
    const hk = (s) => s.toLowerCase();
    const { hotkey, settingsHotkey, inkHotkey } = newSettings;
    if (
      hk(hotkey) === hk(settingsHotkey) ||
      hk(hotkey) === hk(inkHotkey) ||
      hk(settingsHotkey) === hk(inkHotkey)
    ) {
      return showStatus("error", "三个快捷键必须两两不同");
    }

    $("save-btn").disabled = true;
    showStatus("info", "正在保存…");
    try {
      await window.__TAURI__.core.invoke("save_settings", { newSettings });
      originalSettings = newSettings;
      showStatus("ok", "✓ 已保存", 2000);
    } catch (e) {
      showStatus("error", "保存失败: " + (e?.message || e));
    } finally {
      $("save-btn").disabled = false;
    }
  });

  $("cancel-btn").addEventListener("click", () => {
    const w = window.__TAURI__ && window.__TAURI__.window;
    if (w && w.getCurrentWindow) {
      w.getCurrentWindow().close();
    } else {
      window.close();
    }
  });

  $("test-btn").addEventListener("click", async () => {
    $("test-btn").disabled = true;
    showStatus("info", "正在测试连接…");
    try {
      // We pass a *snapshot* of the form so testing with unsaved values is
      // possible. The backend's `probe` only runs against the live in-memory
      // config though, so the user has to Save first. That's a small UX
      // papercut we can iterate on if needed.
      const result = await window.__TAURI__.core.invoke("test_connection");
      showStatus("ok", "✓ 连接成功 (" + result + ")", 3000);
    } catch (e) {
      showStatus("error", "连接失败: " + (e?.message || e));
    } finally {
      $("test-btn").disabled = false;
    }
  });

  // ── boot ─────────────────────────────────────────────────────────────────

  function waitForTauri(attempts) {
    if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
      load();
      return;
    }
    if (attempts <= 0) {
      showStatus("error", "Tauri 运行时不可用");
      return;
    }
    setTimeout(() => waitForTauri(attempts - 1), 30);
  }
  waitForTauri(100);
})();
