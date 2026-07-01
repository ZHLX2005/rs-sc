//! Multimodal LLM client — speaks the OpenAI `/chat/completions` schema.
//!
//! Compatible with OpenAI itself, Azure OpenAI (set base_url to the deployment URL),
//! DeepSeek, Ollama (`http://127.0.0.1:11434/v1`), and any other provider that follows
//! the same JSON schema. The user passes the image as a `data:image/png;base64,...` URL.
//!
//! The active configuration lives behind an `RwLock` so the settings panel can hot-swap
//! it at runtime — the next `translate_png` call picks up the new values without a
//! process restart.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::{AppError, AppResult};

/// User-tunable configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub prompt: String,
}

impl LlmConfig {
    /// Hard-coded fallbacks used when neither the settings file nor environment
    /// variables provide a value. Defaults to a local Ollama instance so the app
    /// runs out of the box with `ollama serve`.
    pub fn defaults() -> Self {
        Self {
            base_url: "http://127.0.0.1:11434/v1/chat/completions".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            prompt:
                "你是一名专业翻译。请把图片里的文字翻译成简体中文，仅输出译文本身，不要解释、不要加引号。"
                    .to_string(),
        }
    }

    /// Layer environment variables on top of the defaults. Used as the bootstrap
    /// configuration before the user has saved anything to disk.
    pub fn from_env_with_defaults() -> Self {
        let mut cfg = Self::defaults();
        if let Ok(v) = std::env::var("RSSC_API_BASE") {
            cfg.base_url = v;
        }
        if let Ok(v) = std::env::var("RSSC_API_KEY") {
            cfg.api_key = v;
        }
        if let Ok(v) = std::env::var("RSSC_MODEL") {
            cfg.model = v;
        }
        if let Ok(v) = std::env::var("RSSC_PROMPT") {
            cfg.prompt = v;
        }
        cfg
    }
}

#[derive(Clone)]
pub struct LlmTranslateClient {
    http: Arc<reqwest::Client>,
    config: Arc<RwLock<LlmConfig>>,
}

/// Extract the model's reasoning from a `...` block and return
/// `(thinking, answer)`. Several reasoning models (Qwen3, DeepSeek-R1,
/// Kimi K2 Thinking, OpenAI o1) emit their chain-of-thought inside
/// `` tags before the actual response. We never expose the
/// thinking to the user — it's only used to drive a "思考中…" status —
/// so the answer is what's persisted, displayed, and forwarded to
/// the next pipeline step.
///
/// We strip the FIRST outermost `` ... `` pair, case-insensitively,
/// and treat everything after the closing tag as the answer. If the
/// model never closes the block (truncation, timeout), the whole
/// response is discarded as thinking and the answer is empty —
/// callers should treat that as an error.
///
/// We tolerate common typos / variants some models emit:
///   - ``  /  ``
///   - ``  /  `` (less common but seen)
fn strip_thinking_block(raw: &str) -> (&str, &str) {
    let bytes = raw.as_bytes();
    let lower = raw.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();

    let open_tags: &[&[u8]] = &[b"<think>"];
    let close_tags: &[&[u8]] = &[b"</think>"];

    for &open in open_tags {
        if let Some(rel_start) = find_subseq(&lower_bytes, open) {
            let abs_start = rel_start + open.len();
            for &close in close_tags {
                if let Some(rel_end) = find_subseq(&lower_bytes[abs_start..], close) {
                    let abs_end = abs_start + rel_end; // exclusive of close
                    let thinking = std::str::from_utf8(&bytes[rel_start..abs_end]).unwrap_or("");
                    let answer = std::str::from_utf8(&bytes[abs_end + close.len()..]).unwrap_or("");
                    return (thinking.trim(), answer.trim());
                }
            }
            // Open tag found but no matching close tag — the model's
            // response was truncated mid-thinking. Treat the entire
            // response as thinking and discard.
            return (std::str::from_utf8(&bytes[rel_start..]).unwrap_or("").trim(), "");
        }
    }

    ("", raw.trim())
}

/// Boyer-Moore-Horspool-lite: find `needle` in `haystack`, returns the byte
/// offset of the first match or `None`. Case-sensitive — both inputs should
/// already be lowercased when called.
fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return if needle.is_empty() { Some(0) } else { None };
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Strip the cruft that LLM OCR calls like to add even when asked for raw text.
///
/// Common shapes we strip:
///   - Surrounding straight or curly quotes: "hello" → hello
///   - Markdown code fences: ```hello``` → hello
///   - Common Chinese prefixes the model slips in: "识别结果:" "图中文字是:" "用户写的是:"
///   - Trailing sentence-final punctuation if not actually in the image
///     (we can't be 100% sure about this, so we only strip the most common
///     offenders — `.` `。` `?` `?` `!` `!` at the very end if the
///     remaining body is purely CJK / ASCII letters & digits, since
///     handwriting samples rarely end with a period)
///   - Whitespace collapse: runs of spaces / newlines → single space
fn clean_ocr_text(raw: &str) -> String {
    let mut s = raw.trim().to_string();

    // Strip surrounding quotes (most common: " ... " or " ... " or ' ... ')
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\u{201C}') && s.ends_with('\u{201D}') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s = s[1..s.len() - 1].to_string();
    }

    // Strip markdown code fences (some models wrap the answer in ```)
    if s.starts_with("```") && s.ends_with("```") && s.len() >= 6 {
        let inner = &s[3..s.len() - 3];
        // Drop an optional language tag after the opening fence (e.g. ```text\n...)
        if let Some(nl) = inner.find('\n') {
            s = inner[nl + 1..].to_string();
        } else {
            s = inner.to_string();
        }
    }

    // Strip common Chinese prefixes the model likes to add despite the
    // "no prefix" instruction. We match case-insensitively on the leading
    // characters, including any surrounding whitespace or punctuation.
    const PREFIXES: &[&str] = &[
        "识别结果:",
        "识别结果：",
        "图中文字是:",
        "图中文字是：",
        "图中文字:",
        "图中文字：",
        "用户写的是:",
        "用户写的是：",
        "文字内容:",
        "文字内容：",
        "内容:",
        "内容：",
        "answer:",
        "answer：",
        "result:",
        "result：",
        "ocr:",
        "ocr：",
    ];
    let lower = s.to_lowercase();
    for p in PREFIXES {
        if lower.starts_with(p) {
            s = s[p.len()..].trim_start().to_string();
            break;
        }
    }

    // Collapse internal whitespace.
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    s = out.trim().to_string();

    // If the result is purely CJK + Latin letters/digits and ends with a
    // sentence-final punctuation mark, strip the trailing mark — most
    // handwriting samples don't end with `.` `。` etc.
    if s.ends_with('.') && s.chars().all(|c| c.is_alphanumeric() || c == '.' || c == ' ') {
        s.pop();
    }
    if s.ends_with('。') && s.chars().all(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}' | '0'..='9' | 'a'..='z' | 'A'..='Z')) {
        s.pop();
    }

    s
}

impl LlmTranslateClient {
    pub fn new(http: Arc<reqwest::Client>, config: LlmConfig) -> Self {
        Self {
            http,
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// Replace the live configuration. Subsequent `translate_png` calls use the new
    /// values. The previous config is dropped only after all in-flight reads finish,
    /// so this is safe to call concurrently with translation.
    pub async fn set_config(&self, new_config: LlmConfig) {
        *self.config.write().await = new_config;
    }

    /// Send a PNG to the multimodal model and return the translated text.
    ///
    /// `cancel` is checked before starting the network call and raced against
    /// the in-flight request via `tokio::select!`. When the user presses the
    /// capture hotkey again, the previous pipeline flips this flag, the
    /// `select!` arm wins, and we return immediately so the new capture can
    /// start without waiting for a slow model response.
    pub async fn translate_png(
        &self,
        png_bytes: &[u8],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> AppResult<(String, bool)> {
        // Cheap fast-path: if we've already been cancelled, don't even build
        // the request body.
        if let Some(c) = cancel {
            if c.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(AppError::Capture("cancelled".into()));
            }
        }

        // Take a snapshot of the config so we don't hold the read lock across the
        // network round-trip.
        let cfg = self.config.read().await.clone();

        let b64 = BASE64.encode(png_bytes);
        let data_url = format!("data:image/png;base64,{}", b64);

        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: vec![
                    ContentPart::Text { text: cfg.prompt.clone() },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: data_url },
                    },
                ],
            }],
            temperature: 0.2,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        // Build a future for the network round-trip. We can't await it twice
        // (it'd move req), so we keep it as a pinned Send future and select!
        // against either it or a cancellation signal.
        let send_fut = req.send();
        tokio::pin!(send_fut);

        let resp = if let Some(c) = cancel {
            // Poll both the send and the cancel flag every 100ms. We can't use
            // a notify channel here without restructuring the LLM client, but
            // 100ms is well under any user's hotkey-press latency, so the
            // detection lag is imperceptible.
            loop {
                if c.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(AppError::Capture("cancelled".into()));
                }
                tokio::select! {
                    biased;
                    r = &mut send_fut => break r,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => continue,
                }
            }
        } else {
            send_fut.await
        }
        .map_err(|e| AppError::Ai(format!("translate request failed: {e}")))?;

        // Re-check after the response comes back: even if we weren't cancelled
        // mid-flight, the user may have pressed the hotkey while we were
        // reading the body.
        if let Some(c) = cancel {
            if c.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(AppError::Capture("cancelled".into()));
            }
        }

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Ai(format!("read body failed: {e}")))?;

        if !status.is_success() {
            let detail: String = body.chars().take(500).collect();
            return Err(AppError::Ai(format!(
                "AI returned HTTP {}: {detail}",
                status.as_u16()
            )));
        }

        let body_preview: String = body.chars().take(200).collect();
        let parsed: ChatResponse = serde_json::from_str(&body).map_err(|e| {
            AppError::Ai(format!(
                "parse response failed: {e}; body={body_preview}"
            ))
        })?;

        // Always strip the `` block before doing anything else. Some reasoning
        // models (Qwen3 / DeepSeek-R1 / o1 / Kimi K2) emit a thinking trace
        // before their actual answer. The user only cares about the answer;
        // the thinking is logged at debug level and never sent over IPC.
        let raw = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();
        let (_thinking, answer) = strip_thinking_block(&raw);
        let text = answer.trim().to_string();

        if text.is_empty() {
            return Err(AppError::Ai("model returned empty content".into()));
        }
        Ok((text, !_thinking.is_empty()))
    }

    /// Send a minimal probe (text only) to verify the API is reachable and returns
    /// a valid response. Used by the "Test connection" button in the settings panel.
    /// Returns `(status_text, had_thinking)`. The probe response is so short we
    /// never expect a think block, but we keep the API consistent.
    pub async fn probe(&self) -> AppResult<(String, bool)> {
        let cfg = self.config.read().await.clone();

        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: vec![ContentPart::Text {
                    text: "ping".to_string(),
                }],
            }],
            temperature: 0.0,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Ai(format!("connect failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let detail: String = body.chars().take(200).collect();
            return Err(AppError::Ai(format!(
                "HTTP {}: {detail}",
                status.as_u16()
            )));
        }
        Ok((format!("HTTP {}", status.as_u16()), false))
    }

    /// Step 1 of the ink flow: send the user's hand-drawn PNG to the model
    /// and ask it to recognize the handwriting.
    ///
    /// Returns the recognized text, **strictly cleaned**: trimmed, with
    /// surrounding quotation marks stripped, common model artefacts like
    /// `用户写的是:` prefixes removed. We need a clean string because Step 2
    /// feeds this verbatim as a question to the QA model and we also display
    /// it in the ink window — any leading "用户写的是:" would be jarring.
    pub async fn ocr_handwriting(
        &self,
        png_bytes: &[u8],
        prompt: &str,
    ) -> AppResult<(String, bool)> {
        let cfg = self.config.read().await.clone();
        let data_url = format!("data:image/png;base64,{}", BASE64.encode(png_bytes));
        // We hard-pin the OCR contract: the model MUST return only the raw
        // transcribed text. We append this instruction to whatever the user
        // set in the settings panel — it can't be removed, only added to. This
        // makes OCR output reliable across model families.
        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: vec![
                    ContentPart::Text {
                        text: format!(
                            "{prompt}\n\n[OCR 契约]\n你的回答必须且只能是图中文字本身的字符序列。\
                             严禁添加以下任何内容:引号、前缀(如 \"识别结果:\")、解释、Markdown 格式、\
                             句末句号(除非原图本身有)、翻译、改写、猜测。\
                             如果图中有多个字符块,按从左到右、从上到下顺序用单个空格连接。"
                        ),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: data_url },
                    },
                ],
            }],
            temperature: 0.0,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Ai(format!("ocr request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Ai(format!("ocr read body failed: {e}")))?;

        if !status.is_success() {
            let detail: String = body.chars().take(500).collect();
            return Err(AppError::Ai(format!(
                "OCR API returned HTTP {}: {detail}",
                status.as_u16()
            )));
        }

        let parsed: ChatResponse = serde_json::from_str(&body).map_err(|e| {
            AppError::Ai(format!(
                "ocr parse failed: {e}; body={}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

        let raw = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        // Reasoning models would otherwise produce "``用户写的是 ... ''"
        // which then survives clean_ocr_text as garbage. Strip the
        // `` block FIRST, then clean.
        let (_thinking, answer) = strip_thinking_block(&raw);
        let text = clean_ocr_text(answer);

        if text.is_empty() {
            return Err(AppError::Ai("OCR returned empty text".into()));
        }
        Ok((text, !_thinking.is_empty()))
    }

    /// Step 2 of the ink flow: take the OCR-recognized question + the original
    /// screenshot and ask the model for the answer.
    pub async fn qa_with_context(
        &self,
        question: &str,
        context_png_bytes: &[u8],
        prompt: &str,
    ) -> AppResult<(String, bool)> {
        let cfg = self.config.read().await.clone();
        let data_url = format!(
            "data:image/png;base64,{}",
            BASE64.encode(context_png_bytes)
        );
        // The user-configured prompt is the system instruction (style, length,
        // tone). The user-role message is fully controlled by us so the OCR'd
        // question is unambiguously the actual question — never metadata.
        //
        // We make the contract explicit: the OCR'd text IS the question, the
        // screenshot is the only visual context, you MUST answer (don't say
        // "I can't see the image" — the screenshot is attached).
        let request = ChatRequest {
            model: cfg.model.clone(),
            messages: vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: vec![ContentPart::Text {
                        text: prompt.to_string(),
                    }],
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: vec![
                        ContentPart::Text {
                            text: format!(
                                "## 用户提问(来自手写 OCR):\n\
                                 {question}\n\n\
                                 ## 上下文:\n\
                                 用户对下方截图手写了上面的提问。请基于截图内容回答这个提问。\n\
                                 必须基于截图回答,不要凭空想象;如截图中无相关信息,请明确说明。",
                                question = question.trim()
                            ),
                        },
                        ContentPart::ImageUrl {
                            image_url: ImageUrl { url: data_url },
                        },
                    ],
                },
            ],
            temperature: 0.3,
        };

        let mut req = self
            .http
            .post(&cfg.base_url)
            .header("Content-Type", "application/json")
            .json(&request);
        if !cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Ai(format!("qa request failed: {e}")))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::Ai(format!("qa read body failed: {e}")))?;

        if !status.is_success() {
            let detail: String = body.chars().take(500).collect();
            return Err(AppError::Ai(format!(
                "QA API returned HTTP {}: {detail}",
                status.as_u16()
            )));
        }

        let parsed: ChatResponse = serde_json::from_str(&body).map_err(|e| {
            AppError::Ai(format!(
                "qa parse failed: {e}; body={}",
                body.chars().take(200).collect::<String>()
            ))
        })?;

        let raw = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        // Same `` handling as translate_png — reasoning models that answer
        // the user's question often wrap their final answer in a fresh
        // think block. We discard the trace and keep only the answer.
        let (_thinking, answer) = strip_thinking_block(&raw);
        let text = answer.trim().to_string();

        if text.is_empty() {
            return Err(AppError::Ai("QA returned empty answer".into()));
        }
        Ok((text, !_thinking.is_empty()))
    }
}

// ── Wire types ────────────────────────────────────────────────────────────────
// We only model the shape we actually use; serde will ignore unknown fields.

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: Vec<ContentPart>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
struct ImageUrl {
    url: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}


#[cfg(test)]
mod thinking_strip_tests {
    use super::strip_thinking_block;

    #[test]
    fn plain_text_unchanged() {
        let (_t, a) = strip_thinking_block("Hello world.");
        assert_eq!(a, "Hello world.");
        assert!(_t.is_empty());
    }

    #[test]
    fn strips_lowercase_think_block() {
        let (_t, a) = strip_thinking_block(
            "<think>The user asked a question. The image shows...</think>The answer is 42."
        );
        assert_eq!(a, "The answer is 42.");
        assert!(_t.contains("The user asked"));
    }

    #[test]
    fn strips_uppercase_think_block() {
        let (_t, a) = strip_thinking_block(
            "<think>reasoning here</think>FINAL ANSWER"
        );
        assert_eq!(a, "FINAL ANSWER");
    }

    #[test]
    fn strips_multiline_think_block() {
        let raw = "<think>\nLine 1 of thinking\nLine 2 of thinking\n</think>\nThe real answer\n";
        let (_t, a) = strip_thinking_block(raw);
        assert_eq!(a, "The real answer");
        assert!(_t.contains("Line 1"));
        assert!(_t.contains("Line 2"));
    }

    #[test]
    fn drops_response_when_think_block_never_closes() {
        let (_t, a) = strip_thinking_block("<think>thinking but truncated mid-stream");
        assert_eq!(a, "");
        assert!(_t.contains("truncated"));
    }

    #[test]
    fn multiple_think_blocks_takes_first_pair() {
        // We only strip the FIRST outermost pair. If a model restarts its
        // thinking after already producing text, we keep the leading text
        // (which is rarer than the typical case of "all thinking, then answer").
        let raw = "<think>first thinking</think>real answer<think>restart thinking never closes";
        let (_t, a) = strip_thinking_block(raw);
        assert_eq!(a, "real answer<think>restart thinking never closes");
    }

    #[test]
    fn empty_input_returns_empty() {
        let (_t, a) = strip_thinking_block("");
        assert_eq!(a, "");
        assert_eq!(_t, "");
    }

    #[test]
    fn case_insensitive_opening() {
        let (_t, a) = strip_thinking_block("<think>x</think>answer");
        assert_eq!(a, "answer");
        let (_t, a) = strip_thinking_block("<THINK>x</THINK>answer");
        assert_eq!(a, "answer");
    }

    #[test]
    fn whitespace_around_think_block_is_handled() {
        let (_t, a) = strip_thinking_block("  <think>x</think>   answer   ");
        assert_eq!(a, "answer");
    }

    #[test]
    fn realistic_deepseek_r1_response() {
        // Real-world test case — a DeepSeek-R1 style response with a long
        // thinking block followed by the actual answer.
        let raw = r#"<think>
用户发送了一张图片，但图片似乎非常模糊，无法清晰识别内容。让我仔细观察一下。

图片显示的内容非常模糊，看起来像是一张带有文字或数字的图像，但由于模糊程度太高，无法清楚地辨认具体内容。可能是某种数据表、图表或代码截图，但由于分辨率和清晰度问题，无法准确解读。

用户说"帮助我完成复习"，可能是希望我帮助他们复习某些内容，但图片质量太差，无法读取具体信息。我需要诚实地告诉用户图片太模糊了，无法识别内容，并请求他们提供更清晰的图片或文字描述。
</think>抱歉，您发送的图片非常模糊，我无法清晰地识别其中的内容。从模糊的轮廓来看，似乎包含一些表格或字符，但无法辨认具体是什么内容。

请您可以：
1. **重新上传一张更清晰的图片**（确保文字内容可以清楚辨认）
2. **用文字描述您需要复习的具体内容**
3. **拍照时注意**：
   - 保持手机/相机稳定，避免抖动
   - 确保光线充足，避免反光
   - 尽量让文字充满画面，提高分辨率
   - 如果是PPT或屏幕内容，可以直接截屏而非拍照

这样我才能更好地为您提供复习帮助！😊"#;
        let (_t, a) = strip_thinking_block(raw);
        // The thinking-only content (the model's internal monologue) must
        // NOT appear in the answer. The thinking-only lines are:
        //   "让我仔细观察一下"
        //   "可能是希望我帮助他们复习某些内容"
        //   "由于分辨率和清晰度问题，无法准确解读"
        // These appear ONLY in the thinking block, never in the answer.
        assert!(!a.contains("让我仔细观察"), "answer leaked thinking phrase");
        assert!(!a.contains("分辨率和清晰度"), "answer leaked thinking phrase");
        assert!(!a.contains("帮助他们复习"), "answer leaked thinking phrase");

        // The answer should still contain the legitimate response text
        // (which talks about 模糊 + suggestions). Words like "模糊" appear in
        // BOTH blocks — that's fine, we only check for thinking-exclusive
        // phrases.
        assert!(a.starts_with("抱歉"), "wrong answer prefix");
        assert!(a.contains("重新上传"), "answer body missing");
        assert!(a.ends_with("😊"));

        // The thinking variable DOES contain the discarded text (returned
        // for diagnostics / logging, not for forwarding to the user).
        assert!(_t.contains("仔细观察"));
        assert!(_t.contains("分辨率"));
    }
}

