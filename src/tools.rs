//! The tool-call contract between the agent (model) and the harness.
//!
//! A real model never touches the system directly. It emits a *tool call* — a
//! structured request the harness renders, possibly gates behind approval,
//! executes, and whose output it feeds back to the model.
//!
//! Every tool flows through the same pipeline (see `execute`) and renders with
//! the same `● Verb(args)` / `⎿ output` shape in the transcript. Adding a new
//! tool = a new `ToolCall` variant + an arm in `execute`.

use std::path::Path;

use tokio::sync::oneshot;

use crate::workspace;

/// A request from the agent for the harness to run a tool.
pub enum ToolCall {
    /// Read a file and return its contents to the agent.
    Read { path: String },
    /// Write (create/overwrite) a file. Gated behind approval; shows a diff.
    Write { path: String, content: String },
    /// Surgically replace `old_string` with `new_string` in a file.
    /// `old_string` must match exactly and be unique unless `replace_all`.
    /// Gated behind approval; shows a diff.
    Edit {
        path: String,
        old_string: String,
        new_string: String,
        replace_all: bool,
    },
    /// Run a shell command. Gated behind approval; shows its output.
    Bash { command: String },
    /// Search the web via the Exa API. Shows results.
    WebSearch { query: String },
    /// Fetch a URL and return its content. HTML is converted per `format` —
    /// markdown by default, which keeps structure (headings/links/code) while
    /// saving tokens versus raw HTML.
    Fetch { url: String, format: FetchFormat },
    /// Ask the user a structured multiple-choice question.
    AskQuestion(Question),
}

impl ToolCall {
    /// Tools that can mutate the system or run arbitrary code need a yes/no
    /// from the user before they run.
    pub fn needs_approval(&self) -> bool {
        matches!(
            self,
            ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::Bash { .. }
        )
    }
}

/// How `fetch` renders HTML pages for the model.
///
/// In the tool schema this is a string option: `"markdown"` (default) or
/// `"text"`. Markdown preserves headings/links/code blocks and is the
/// token-efficient choice; plain text is a cruder, smaller fallback.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum FetchFormat {
    #[default]
    Markdown,
    Text,
}

/// ## `ask_question` schema (the JSON the model fills out)
/// ```json
/// { "header": "Language", "question": "...", "multi_select": false,
///   "options": [{ "label": "Rust", "description": "..." }] }
/// ```
pub struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<Choice>,
    pub multi_select: bool,
}

pub struct Choice {
    pub label: String,
    pub description: String,
}

/// The harness's reply to a tool call, sent back to the awaiting agent.
pub enum ToolResult {
    /// Generic command-style result: `text` is what the model sees.
    Output { ok: bool, text: String },
    /// Indices into `Question::options` that the user selected.
    Question { selected: Vec<usize> },
}

pub type Responder = oneshot::Sender<ToolResult>;

/// Lifecycle of a tool call as shown in the transcript.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Pending,  // awaiting approval
    Running,  // executing
    Done,     // completed ok
    Rejected, // user declined
    Failed,   // errored
}

/// Sent from an execution task back to the UI to update a tool's record.
pub struct ToolUpdate {
    pub id: u64,
    pub status: ToolStatus,
    /// The `⎿` summary line (e.g. "exit 0", "Read 142 lines").
    pub summary: Option<String>,
    /// Body text to display under the summary (e.g. command output).
    pub output: Option<String>,
}

/// The product of running a tool: what the model sees vs. what we render.
pub struct Run {
    pub ok: bool,
    /// Returned to the model.
    pub for_agent: String,
    /// One-line `⎿` summary.
    pub summary: String,
    /// Optional body shown in the UI (None = summary only).
    pub output: Option<String>,
}

/// Execute a tool. This is where the harness actually touches the system.
pub async fn execute(workspace_root: &Path, call: ToolCall) -> Run {
    match call {
        ToolCall::Read { path } => read(workspace_root, path).await,
        ToolCall::Write { path, content } => write(workspace_root, path, content).await,
        ToolCall::Edit {
            path,
            old_string,
            new_string,
            replace_all,
        } => edit(workspace_root, path, old_string, new_string, replace_all).await,
        ToolCall::Bash { command } => bash(workspace_root, command).await,
        ToolCall::WebSearch { query } => web_search(query).await,
        ToolCall::Fetch { url, format } => fetch(url, format).await,
        // Questions are resolved by the UI, never executed here.
        ToolCall::AskQuestion(_) => Run {
            ok: true,
            for_agent: String::new(),
            summary: String::new(),
            output: None,
        },
    }
}

async fn read(workspace_root: &Path, path: String) -> Run {
    let resolved = match workspace::resolve_existing(workspace_root, &path) {
        Ok(path) => path,
        Err(error) => return path_error(error),
    };
    match tokio::fs::read_to_string(&resolved).await {
        Ok(content) => {
            let n = content.lines().count();
            Run {
                ok: true,
                for_agent: content,
                summary: format!("Read {n} lines"),
                output: None,
            }
        }
        Err(e) => Run {
            ok: false,
            for_agent: format!("error reading {path}: {e}"),
            summary: format!("error: {e}"),
            output: None,
        },
    }
}

async fn write(workspace_root: &Path, path: String, content: String) -> Run {
    let resolved = match workspace::resolve_for_write(workspace_root, &path) {
        Ok(path) => path,
        Err(error) => return path_error(error),
    };
    if let Some(parent) = resolved.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::write(&resolved, &content).await {
        Ok(_) => {
            let n = content.lines().count();
            Run {
                ok: true,
                for_agent: format!("wrote {path} ({n} lines)"),
                summary: format!("Wrote {n} lines"),
                output: None,
            }
        }
        Err(e) => Run {
            ok: false,
            for_agent: format!("error writing {path}: {e}"),
            summary: format!("write failed: {e}"),
            output: None,
        },
    }
}

/// Validate an edit and compute the resulting file content.
///
/// Returns `(old_content, new_content, replacements)`. Errors when the file
/// can't be read, `old_string` is empty/missing/identical to `new_string`,
/// or matches more than once without `replace_all` — these come back as
/// strings the model can act on (add more context, set `replace_all`, …).
pub fn prepare_edit(
    workspace_root: &Path,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, String, usize), String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty (use the write tool to create files)".into());
    }
    if old_string == new_string {
        return Err("old_string and new_string are identical".into());
    }
    let resolved = workspace::resolve_existing(workspace_root, path)?;
    let content =
        std::fs::read_to_string(resolved).map_err(|e| format!("cannot read {path}: {e}"))?;
    let count = content.matches(old_string).count();
    if count == 0 {
        return Err(format!(
            "old_string not found in {path}; it must match the file content exactly, \
             including whitespace and indentation"
        ));
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string matches {count} times in {path}; include more surrounding \
             context to make it unique, or set replace_all"
        ));
    }
    let (new_content, replaced) = if replace_all {
        (content.replace(old_string, new_string), count)
    } else {
        (content.replacen(old_string, new_string, 1), 1)
    };
    Ok((content, new_content, replaced))
}

async fn edit(
    workspace_root: &Path,
    path: String,
    old_string: String,
    new_string: String,
    replace_all: bool,
) -> Run {
    // Re-validate at execution time — the file may have changed since the
    // diff was shown for approval.
    match prepare_edit(workspace_root, &path, &old_string, &new_string, replace_all) {
        Err(e) => Run {
            ok: false,
            for_agent: format!("ERROR: {e}"),
            summary: e,
            output: None,
        },
        Ok((_, new_content, n)) => {
            let resolved = match workspace::resolve_existing(workspace_root, &path) {
                Ok(path) => path,
                Err(error) => return path_error(error),
            };
            match tokio::fs::write(&resolved, &new_content).await {
                Ok(_) => Run {
                    ok: true,
                    for_agent: format!("edited {path}: replaced {n} occurrence(s)"),
                    summary: format!("Replaced {n} occurrence(s)"),
                    output: None,
                },
                Err(e) => Run {
                    ok: false,
                    for_agent: format!("error writing {path}: {e}"),
                    summary: format!("write failed: {e}"),
                    output: None,
                },
            }
        }
    }
}

async fn bash(workspace_root: &Path, command: String) -> Run {
    let result = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(workspace_root)
        .output()
        .await;

    match result {
        Ok(out) => {
            let mut body = String::new();
            body.push_str(&String::from_utf8_lossy(&out.stdout));
            body.push_str(&String::from_utf8_lossy(&out.stderr));
            let body = body.trim_end().to_string();
            let code = out.status.code().unwrap_or(-1);
            Run {
                ok: out.status.success(),
                for_agent: format!("$ {command}\n{body}\n(exit {code})"),
                summary: format!("exit {code}"),
                output: Some(if body.is_empty() {
                    "(no output)".to_string()
                } else {
                    body
                }),
            }
        }
        Err(e) => Run {
            ok: false,
            for_agent: format!("failed to run `{command}`: {e}"),
            summary: format!("error: {e}"),
            output: Some(e.to_string()),
        },
    }
}

fn path_error(error: String) -> Run {
    Run {
        ok: false,
        for_agent: format!("ERROR: {error}"),
        summary: error,
        output: None,
    }
}

/// Web search via the Exa API (https://docs.exa.ai). Requires `EXA_API_KEY`.
async fn web_search(query: String) -> Run {
    const NUM_RESULTS: u32 = 8;
    const MAX_CHARS: u32 = 1200;

    let key = match std::env::var("EXA_API_KEY") {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            return Run {
                ok: false,
                for_agent: "Web search unavailable: EXA_API_KEY is not set.".to_string(),
                summary: "EXA_API_KEY not set".to_string(),
                output: None,
            };
        }
    };

    let request = ExaRequest {
        query: &query,
        num_results: NUM_RESULTS,
        contents: ExaContents {
            text: ExaText {
                max_characters: MAX_CHARS,
            },
        },
    };

    let send = reqwest::Client::new()
        .post("https://api.exa.ai/search")
        .header("x-api-key", key)
        .json(&request)
        .send()
        .await;

    let response = match send {
        Ok(r) => r,
        Err(e) => {
            return Run {
                ok: false,
                for_agent: format!("Exa request failed: {e}"),
                summary: "request failed".to_string(),
                output: Some(e.to_string()),
            };
        }
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Run {
            ok: false,
            for_agent: format!("Exa returned HTTP {status}: {body}"),
            summary: format!("HTTP {}", status.as_u16()),
            output: Some(body.chars().take(400).collect()),
        };
    }

    let data = match response.json::<ExaResponse>().await {
        Ok(d) => d,
        Err(e) => {
            return Run {
                ok: false,
                for_agent: format!("Failed to parse Exa response: {e}"),
                summary: "parse error".to_string(),
                output: None,
            };
        }
    };

    // What the model sees: title, url, and the extracted text per result.
    let mut for_agent = String::new();
    // What the UI shows: a compact ranked list of titles + urls.
    let mut ui = String::new();
    for (i, r) in data.results.iter().enumerate() {
        let title = r.title.clone().unwrap_or_else(|| r.url.clone());
        for_agent.push_str(&format!("[{}] {}\n{}\n", i + 1, title, r.url));
        if let Some(text) = &r.text {
            for_agent.push_str(text.trim());
            for_agent.push('\n');
        }
        for_agent.push('\n');
        ui.push_str(&format!("{}. {}  —  {}\n", i + 1, title, r.url));
    }

    let n = data.results.len();
    Run {
        ok: true,
        for_agent: if for_agent.is_empty() {
            "No results.".to_string()
        } else {
            for_agent
        },
        summary: format!("{n} results"),
        output: Some(ui.trim_end().to_string()),
    }
}

/// Fetch a URL and return readable content. HTML is converted to markdown
/// (default, token-efficient) or plain text; JSON/plain bodies pass through.
/// Both directions are size-capped.
async fn fetch(url: String, format: FetchFormat) -> Run {
    /// Max chars returned to the model.
    const MAX_AGENT_CHARS: usize = 20_000;
    /// Max chars shown in the transcript body (UI also caps lines).
    const MAX_UI_CHARS: usize = 2_000;

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; fast-rlm-agent/0.1)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Run {
                ok: false,
                for_agent: format!("failed to build http client: {e}"),
                summary: "client error".to_string(),
                output: None,
            };
        }
    };

    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Run {
                ok: false,
                for_agent: format!("fetch failed for {url}: {e}"),
                summary: "request failed".to_string(),
                output: Some(e.to_string()),
            };
        }
    };

    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Run {
            ok: false,
            for_agent: format!("HTTP {status} fetching {url}"),
            summary: format!("HTTP {}", status.as_u16()),
            output: Some(raw.chars().take(400).collect()),
        };
    }

    let raw_kb = raw.len() / 1024;
    let is_html = content_type.contains("html") || raw.trim_start().starts_with('<');
    let (text, rendered_as) = if is_html {
        match format {
            // htmd can fail on pathological markup — fall back to the stripper.
            FetchFormat::Markdown => match htmd::convert(&raw) {
                Ok(md) => (md, "markdown"),
                Err(_) => (html_to_text(&raw), "text"),
            },
            FetchFormat::Text => (html_to_text(&raw), "text"),
        }
    } else {
        (raw, "raw")
    };

    let total_chars = text.chars().count();
    let truncated = total_chars > MAX_AGENT_CHARS;
    let mut for_agent: String = text.chars().take(MAX_AGENT_CHARS).collect();
    if truncated {
        for_agent.push_str(&format!(
            "\n\n[truncated: {total_chars} chars total, showing first {MAX_AGENT_CHARS}]"
        ));
    }

    Run {
        ok: true,
        for_agent,
        summary: format!(
            "{} · {raw_kb} KB → {total_chars} chars as {rendered_as}{}",
            status.as_u16(),
            if truncated { " (truncated)" } else { "" }
        ),
        output: Some(
            text.chars()
                .take(MAX_UI_CHARS)
                .collect::<String>()
                .trim()
                .to_string(),
        ),
    }
}

/// Minimal HTML → plain text: drop script/style, turn block tags into line
/// breaks, strip the rest, decode common entities, collapse blank lines.
fn html_to_text(html: &str) -> String {
    let html = strip_element(html, "script");
    let html = strip_element(&html, "style");

    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        // Consume the tag; note its name to decide whether to break the line.
        let rest = &html[i + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = rest[..end]
            .trim_start_matches('/')
            .split([' ', '\t', '\n', '/'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match tag.as_str() {
            "p" | "div" | "br" | "tr" | "section" | "article" | "ul" | "ol" | "table"
            | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" => out.push('\n'),
            "li" => out.push_str("\n- "),
            "td" | "th" => out.push(' '),
            _ => {}
        }
        // Skip past the '>'.
        for _ in 0..end + 1 {
            if chars.next().is_none() {
                break;
            }
        }
    }

    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");

    // Collapse runs of blank lines and trim line-edges.
    let mut result = String::with_capacity(decoded.len());
    let mut blank_run = 0;
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        result.push_str(line);
        result.push('\n');
    }
    result.trim().to_string()
}

/// Remove `<tag …>…</tag>` blocks entirely (case-insensitive).
fn strip_element(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open) {
        let start = pos + start;
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(end) => pos = start + end + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = r#"<html><head><style>p{color:red}</style>
            <script>var x = "<p>evil</p>";</script></head>
            <body><h1>Title</h1><p>Hello <b>world</b> &amp; friends.</p>
            <ul><li>one</li><li>two</li></ul></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world & friends."));
        assert!(text.contains("- one"));
        assert!(text.contains("- two"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains("evil"));
    }

    #[test]
    fn strip_element_handles_unclosed() {
        assert_eq!(strip_element("a<script>junk", "script"), "a");
        assert_eq!(strip_element("plain text", "script"), "plain text");
    }

    /// A unique temp file seeded with `content`, cleaned up on drop.
    struct TempFile(std::path::PathBuf);
    impl TempFile {
        fn new(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!("fra-test-{}-{name}", std::process::id()));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }
        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }

        fn workspace(&self) -> &Path {
            self.0.parent().unwrap()
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn edit_replaces_unique_match() {
        let f = TempFile::new("unique.rs", "fn main() {\n    let x = 1;\n}\n");
        let (old, new, n) =
            prepare_edit(f.workspace(), f.path(), "let x = 1;", "let x = 2;", false).unwrap();
        assert_eq!(n, 1);
        assert!(old.contains("x = 1"));
        assert!(new.contains("x = 2") && !new.contains("x = 1"));
    }

    #[test]
    fn edit_rejects_missing_old_string() {
        let f = TempFile::new("missing.txt", "hello world\n");
        let err = prepare_edit(f.workspace(), f.path(), "goodbye", "hi", false).unwrap_err();
        assert!(err.contains("not found"), "err: {err}");
    }

    #[test]
    fn edit_rejects_ambiguous_match() {
        let f = TempFile::new("ambig.txt", "foo\nfoo\nfoo\n");
        let err = prepare_edit(f.workspace(), f.path(), "foo", "bar", false).unwrap_err();
        assert!(err.contains("3 times"), "err: {err}");
        assert!(err.contains("replace_all"), "err: {err}");
    }

    #[test]
    fn edit_replace_all_replaces_every_match() {
        let f = TempFile::new("all.txt", "foo\nfoo\nfoo\n");
        let (_, new, n) = prepare_edit(f.workspace(), f.path(), "foo", "bar", true).unwrap();
        assert_eq!(n, 3);
        assert_eq!(new, "bar\nbar\nbar\n");
    }

    #[test]
    fn edit_rejects_empty_and_identical_and_missing_file() {
        let f = TempFile::new("guards.txt", "content\n");
        assert!(prepare_edit(f.workspace(), f.path(), "", "x", false).is_err());
        assert!(prepare_edit(f.workspace(), f.path(), "content", "content", false).is_err());
        assert!(prepare_edit(f.workspace(), "missing.txt", "a", "b", false)
            .unwrap_err()
            .contains("cannot access"));
    }

    #[tokio::test]
    async fn edit_execute_writes_file() {
        let f = TempFile::new("exec.txt", "version = 1\nname = test\n");
        let run = execute(
            f.workspace(),
            ToolCall::Edit {
                path: f.path().to_string(),
                old_string: "version = 1".to_string(),
                new_string: "version = 2".to_string(),
                replace_all: false,
            },
        )
        .await;
        assert!(run.ok, "{}", run.for_agent);
        assert!(run.summary.contains("Replaced 1"));
        let on_disk = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(on_disk, "version = 2\nname = test\n");
    }

    #[tokio::test]
    async fn edit_execute_fails_cleanly_without_writing() {
        let f = TempFile::new("noexec.txt", "aaa\n");
        let run = execute(
            f.workspace(),
            ToolCall::Edit {
                path: f.path().to_string(),
                old_string: "zzz".to_string(),
                new_string: "yyy".to_string(),
                replace_all: false,
            },
        )
        .await;
        assert!(!run.ok);
        assert_eq!(std::fs::read_to_string(f.path()).unwrap(), "aaa\n");
    }

    #[tokio::test]
    async fn execute_rejects_reads_and_writes_outside_workspace() {
        let f = TempFile::new("boundary.txt", "inside\n");

        let read_run = execute(
            f.workspace(),
            ToolCall::Read {
                path: "../outside.txt".to_string(),
            },
        )
        .await;
        assert!(!read_run.ok);
        assert!(read_run.for_agent.contains("path escapes workspace"));

        let outside = f
            .workspace()
            .parent()
            .unwrap()
            .join("fra-outside-write.txt");
        let _ = std::fs::remove_file(&outside);
        let write_run = execute(
            f.workspace(),
            ToolCall::Write {
                path: outside.to_string_lossy().into_owned(),
                content: "must not be written".to_string(),
            },
        )
        .await;
        assert!(!write_run.ok);
        assert!(!outside.exists());
    }

    #[tokio::test]
    async fn execute_writes_new_files_inside_workspace() {
        let f = TempFile::new("write-inside.txt", "seed\n");
        let path = f
            .workspace()
            .join(format!("fra-new-dir-{}/nested.txt", std::process::id()));
        let run = execute(
            f.workspace(),
            ToolCall::Write {
                path: path.to_string_lossy().into_owned(),
                content: "created\n".to_string(),
            },
        )
        .await;
        assert!(run.ok, "{}", run.for_agent);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "created\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn bash_starts_in_workspace() {
        let f = TempFile::new("bash-cwd.txt", "seed\n");
        let run = execute(
            f.workspace(),
            ToolCall::Bash {
                command: "pwd".to_string(),
            },
        )
        .await;
        assert!(run.ok, "{}", run.for_agent);
        let expected = f.workspace().canonicalize().unwrap();
        assert_eq!(run.output.as_deref(), Some(expected.to_str().unwrap()));
    }

    /// Network test — run explicitly with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetch_real_page() {
        let run = fetch("https://example.com".to_string(), FetchFormat::Markdown).await;
        assert!(run.ok, "fetch failed: {}", run.for_agent);
        assert!(run.for_agent.contains("Example Domain"));
        assert!(!run.for_agent.contains("<div"), "html not stripped");
        assert!(run.summary.contains("markdown"), "summary: {}", run.summary);
    }
}

// ---- Exa request/response shapes -----------------------------------------

#[derive(serde::Serialize)]
struct ExaRequest<'a> {
    query: &'a str,
    #[serde(rename = "numResults")]
    num_results: u32,
    contents: ExaContents,
}

#[derive(serde::Serialize)]
struct ExaContents {
    text: ExaText,
}

#[derive(serde::Serialize)]
struct ExaText {
    #[serde(rename = "maxCharacters")]
    max_characters: u32,
}

#[derive(serde::Deserialize)]
struct ExaResponse {
    #[serde(default)]
    results: Vec<ExaResult>,
}

#[derive(serde::Deserialize)]
struct ExaResult {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    text: Option<String>,
}
