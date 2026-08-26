//! Mermaid fence rendering for the markdown preview.
//!
//! ```mermaid fences are rendered to diagrams headlessly via `merman` (no
//! browser, no Node): each fence's source is rendered to an SVG file in a
//! temp cache off the main thread, and the fence is then rewritten to an
//! `![](file://…)` image reference before the document reaches the GFM
//! renderer — the same pre-processing seam `absolutize_image_paths` uses.
//! The app's `file://` HttpClient serves the file and gpui rasterizes the
//! SVG. A fence whose render is pending or failed is left verbatim, so
//! degradation is exactly the pre-existing highlighted-code display.
//!
//! The SVG must be resvg-safe: mermaid's default HTML-in-`<foreignObject>`
//! labels only render in real browsers, and gpui rasterizes through
//! usvg/resvg which skips them entirely (blank labels). Hence
//! `render_svg_resvg_safe_sync`, never the plain SVG pipeline.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use gpui::{AppContext as _, Context, Task};

/// One ```mermaid fence found in a document. `range` spans the opening fence
/// line through the closing fence line (newline after the close excluded, so
/// the block separator survives a rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MermaidFence {
    pub range: Range<usize>,
    pub contents: String,
}

/// Render state for one fence, keyed by its contents. The result is a
/// `OnceLock` set by the background task — the render pass only ever reads,
/// so no state mutation has to reach back into the owning view.
struct MermaidEntry {
    /// `None` before the task finishes; `Some(None)` = render failed;
    /// `Some(Some(path))` = SVG written and ready.
    result: Arc<OnceLock<Option<PathBuf>>>,
    _task: Task<()>,
}

/// Per-view cache of rendered mermaid fences. Lives as a field on
/// `EditorView` so two open `.md` tabs never share diagram state.
#[derive(Default)]
pub struct MermaidCache {
    entries: HashMap<u64, MermaidEntry>,
}

impl MermaidCache {
    /// Scan `source` for mermaid fences, kick background renders for unseen
    /// ones, and return the source with every ready fence rewritten to an
    /// image reference — or `None` when nothing was rewritten (no fences, or
    /// none ready yet), so the caller can keep its already-owned string and
    /// the common no-mermaid document pays no extra allocation.
    ///
    /// Called from the owning view's render pass; keyed entries make the
    /// spawns once-per-content, so repeated renders of an idle document do
    /// no new work.
    pub fn process<V: 'static>(
        &mut self,
        source: &str,
        cx: &mut Context<V>,
    ) -> Option<String> {
        let fences = scan_mermaid_fences(source);
        if fences.is_empty() {
            self.entries.clear();
            return None;
        }

        let keyed: Vec<(u64, &MermaidFence)> = fences
            .iter()
            .map(|fence| (fence_key(&fence.contents), fence))
            .collect();

        for (key, fence) in &keyed {
            // A ready entry whose file vanished (macOS purges temp dirs on a
            // multi-day cadence) is dropped so the render re-kicks below.
            if let Some(entry) = self.entries.get(key)
                && matches!(entry.result.get(), Some(Some(path)) if !path.exists())
            {
                self.entries.remove(key);
            }
            if !self.entries.contains_key(key) {
                self.entries
                    .insert(*key, spawn_render(*key, fence.contents.clone(), cx));
            }
        }

        // Bound the cache to the fences currently in the document.
        let live: std::collections::HashSet<u64> = keyed.iter().map(|(k, _)| *k).collect();
        self.entries.retain(|key, _| live.contains(key));

        let replacements: Vec<(Range<usize>, String)> = keyed
            .iter()
            .filter_map(|(key, fence)| {
                let path = self.entries.get(key)?.result.get()?.as_ref()?;
                let url = url::Url::from_file_path(path).ok()?;
                Some((fence.range.clone(), format!("![mermaid diagram]({url})")))
            })
            .collect();
        if replacements.is_empty() {
            return None;
        }
        Some(rewrite_mermaid_fences(source, &replacements))
    }
}

/// Spawn the background render for one fence: SVG to the temp cache, then a
/// notify so the owning view repaints and picks the image up.
fn spawn_render<V: 'static>(key: u64, contents: String, cx: &mut Context<V>) -> MermaidEntry {
    let result = Arc::new(OnceLock::new());
    let task_result = result.clone();
    let task = cx.spawn(async move |this, cx| {
        // Debounce: typing inside a fence mints a new content key each
        // keystroke, and `process` prunes the superseded entry — dropping
        // this task and cancelling it right here at the timer await. Only a
        // fence that survives the window actually renders, so a keystroke
        // burst in Split mode costs one render, not one per key.
        cx.background_executor()
            .timer(std::time::Duration::from_millis(300))
            .await;
        let path = cx
            .background_spawn(async move { render_to_cache(key, &contents) })
            .await;
        // Single writer by construction (one task per entry), so `set` can
        // only fail if this exact task ran twice — ignore, don't unwrap.
        let _ = task_result.set(path);
        this.update(cx, |_, cx| cx.notify()).ok();
    });
    MermaidEntry {
        result,
        _task: task,
    }
}

/// Cache key: the fence contents, hashed with the std hasher. Deterministic
/// within a build, which is all a temp-dir cache needs.
fn fence_key(contents: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}

/// The content-addressed cache path for a fence. Temp-dir residency is fine:
/// files are regenerable in milliseconds and `process` re-kicks on a missing
/// file.
fn cache_path(key: u64) -> PathBuf {
    std::env::temp_dir()
        .join("oximux-mermaid")
        .join(format!("m{key:016x}.svg"))
}

/// Render one fence to its cache file, returning the path — or `None` on any
/// failure (invalid mermaid source, IO), which the caller records as a
/// terminal Failed state so the fence stays a plain code block.
fn render_to_cache(key: u64, contents: &str) -> Option<PathBuf> {
    let path = cache_path(key);
    if path.exists() {
        return Some(path);
    }
    let svg = match render_to_svg(contents) {
        Ok(svg) => svg,
        Err(err) => {
            tracing::warn!(?err, "mermaid: render failed; leaving fence as code");
            return None;
        }
    };
    // Write-then-rename so the final path only ever holds a complete SVG: a
    // partial `fs::write` straight to `path` would satisfy the `exists()`
    // fast-path above forever after, and two views rendering the same
    // content race the same file — rename makes the last full write win.
    // pid + counter so no two writers (other process, or two tabs rendering
    // the same content in this one) ever share a tmp file.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    let write = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(&tmp, svg))
        .and_then(|_| std::fs::rename(&tmp, &path));
    match write {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::warn!(?err, path = %path.display(), "mermaid: cache write failed");
            let _ = std::fs::remove_file(&tmp); // best-effort litter cleanup
            None
        }
    }
}

/// The one function that touches the merman API surface (0.x — churny), so a
/// version bump only ever lands here. resvg-safe output is non-negotiable;
/// see the module docs.
///
/// Always the default (light) theme: merman keeps the SVG's own
/// `background-color:white`, and mermaid's `dark` theme paints light `#ccc`
/// text expecting a dark canvas — on that white background it reads
/// washed-out (observed live). The default theme is self-consistent on its
/// white card and stays legible under both app themes; a true dark variant
/// (dark background + dark theme together) is a follow-up.
fn render_to_svg(contents: &str) -> anyhow::Result<String> {
    merman::render::HeadlessRenderer::new()
        .render_svg_resvg_safe_sync(contents)
        .map_err(|err| anyhow::anyhow!("{err}"))?
        .ok_or_else(|| anyhow::anyhow!("not recognized as a mermaid diagram"))
}

/// Find every ```mermaid fence. A full fence state machine (not a regex):
/// non-mermaid fences are tracked too, so a `mermaid` info string *inside*
/// another fenced block is never matched. GFM rules honored: up to 3 leading
/// spaces, >=3 backticks, info string without backticks, closing run at
/// least as long as the opener, unterminated fence is not a block.
pub(crate) fn scan_mermaid_fences(source: &str) -> Vec<MermaidFence> {
    struct OpenFence {
        ticks: usize,
        is_mermaid: bool,
        open_start: usize,
        content_start: usize,
    }

    let mut fences = Vec::new();
    let mut open: Option<OpenFence> = None;
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let text = line.trim_end_matches(['\n', '\r']);
        let stripped = text.strip_prefix("   ").or_else(|| text.strip_prefix("  ")).or_else(|| text.strip_prefix(' ')).unwrap_or(text);
        let ticks = stripped.bytes().take_while(|b| *b == b'`').count();

        match &open {
            Some(fence) if ticks >= fence.ticks && stripped[ticks..].trim().is_empty() => {
                if fence.is_mermaid {
                    let contents = source[fence.content_start..line_start]
                        .strip_suffix('\n')
                        .map(|s| s.strip_suffix('\r').unwrap_or(s))
                        .unwrap_or("")
                        .to_owned();
                    fences.push(MermaidFence {
                        range: fence.open_start..line_start + text.len(),
                        contents,
                    });
                }
                open = None;
            }
            Some(_) => {}
            None if ticks >= 3 => {
                let info = &stripped[ticks..];
                // A backtick in the info string means this line is not a
                // fence opener at all (GFM).
                if !info.contains('`') {
                    let lang = info.split_whitespace().next().unwrap_or("");
                    open = Some(OpenFence {
                        ticks,
                        is_mermaid: lang == "mermaid",
                        open_start: line_start,
                        content_start: line_start + line.len(),
                    });
                }
            }
            None => {}
        }
    }
    fences
}

/// Splice `replacements` (non-overlapping, in document order) into `source`.
/// Pure so it is unit-testable without a renderer or a cache.
pub(crate) fn rewrite_mermaid_fences(
    source: &str,
    replacements: &[(Range<usize>, String)],
) -> String {
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (range, replacement) in replacements {
        out.push_str(&source[cursor..range.start]);
        out.push_str(replacement);
        cursor = range.end;
    }
    out.push_str(&source[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(src: &str) -> Vec<MermaidFence> {
        scan_mermaid_fences(src)
    }

    #[test]
    fn finds_simple_mermaid_fence() {
        let src = "before\n```mermaid\nflowchart TD\n  A-->B\n```\nafter\n";
        let fences = scan(src);
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].contents, "flowchart TD\n  A-->B");
        assert_eq!(&src[fences[0].range.clone()], "```mermaid\nflowchart TD\n  A-->B\n```");
    }

    #[test]
    fn ignores_other_languages_and_plain_fences() {
        let src = "```rust\nfn main() {}\n```\n\n```\ntext\n```\n";
        assert!(scan(src).is_empty());
    }

    #[test]
    fn unterminated_fence_is_not_a_block() {
        assert!(scan("```mermaid\nflowchart TD\n  A-->B\n").is_empty());
    }

    #[test]
    fn mermaid_inside_outer_fence_is_not_matched() {
        // The outer 4-backtick block quotes a mermaid fence; nothing renders.
        let src = "````md\n```mermaid\nflowchart TD\n```\n````\n";
        assert!(scan(src).is_empty());
    }

    #[test]
    fn longer_closing_run_closes_and_indent_up_to_three_allowed() {
        let src = "  ```mermaid\nsequenceDiagram\n  A->>B: hi\n`````\n";
        let fences = scan(src);
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].contents, "sequenceDiagram\n  A->>B: hi");
    }

    #[test]
    fn info_string_with_extra_words_still_mermaid() {
        // Zed's scale param shape (```mermaid 150) parses as a mermaid fence.
        let fences = scan("```mermaid 150\ngraph TD;\n```\n");
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].contents, "graph TD;");
    }

    #[test]
    fn fence_at_eof_without_trailing_newline() {
        let fences = scan("```mermaid\ngraph TD;\n```");
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].contents, "graph TD;");
    }

    #[test]
    fn crlf_contents_are_trimmed() {
        let fences = scan("```mermaid\r\ngraph TD;\r\n```\r\n");
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].contents, "graph TD;");
    }

    #[test]
    fn rewrite_replaces_only_given_ranges() {
        let src = "a\n```mermaid\ngraph TD;\n```\nb\n```mermaid\nx\n```\nc\n";
        let fences = scan(src);
        assert_eq!(fences.len(), 2);
        // Only the first is "ready" — the second stays verbatim.
        let replacements = vec![(fences[0].range.clone(), "![mermaid diagram](file:///m1.svg)".to_owned())];
        let out = rewrite_mermaid_fences(src, &replacements);
        assert_eq!(out, "a\n![mermaid diagram](file:///m1.svg)\nb\n```mermaid\nx\n```\nc\n");
    }

    #[test]
    fn rewrite_with_no_replacements_is_identity() {
        let src = "# doc\n```mermaid\ngraph TD;\n```\n";
        assert_eq!(rewrite_mermaid_fences(src, &[]), src);
    }

    #[test]
    fn fence_key_is_content_addressed() {
        assert_ne!(fence_key("graph TD;"), fence_key("graph LR;"));
        assert_eq!(fence_key("graph TD;"), fence_key("graph TD;"));
    }

    /// End-to-end through the pinned merman: a real flowchart renders to SVG
    /// and the resvg-safe pipeline leaves no `<foreignObject>` behind — the
    /// one property gpui's rasterizer depends on (module docs).
    #[test]
    fn merman_renders_resvg_safe_svg() {
        let svg = render_to_svg("flowchart TD\n  A[Start] --> B[End]").expect("flowchart renders");
        assert!(svg.contains("<svg"), "not an svg: {}", &svg[..svg.len().min(120)]);
        assert!(!svg.contains("foreignObject"), "resvg-unsafe output");
        let seq = render_to_svg("sequenceDiagram\n  A->>B: hi\n  B-->>A: ok")
            .expect("sequence diagram renders");
        assert!(seq.contains("<svg") && !seq.contains("foreignObject"));
    }

    #[test]
    fn invalid_mermaid_reports_error() {
        assert!(render_to_svg("this is not a diagram").is_err());
    }
}
