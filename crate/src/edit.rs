//! After an edit, show a unified diff of what changed. No preview step.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use similar::TextDiff;
use xai_grok_tools::types::output::{
    ApplyPatchOutput, SearchReplaceEditDetail, SearchReplaceEditsApplied, SearchReplaceOutput,
    ToolOutput, line_diff,
};

use crate::plugin;

const CONTEXT: usize = 3;
const MAX_FILE: usize = 1024 * 1024;
const MAX_DIFF_CHARS: usize = 24_000;
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

struct Rendered {
    summary: String,
    diff: String,
    meta: Value,
}

/// MCP `tools/call` result: short text for the model, widget for ChatGPT.
pub fn mcp_result(output: &ToolOutput, prompt_text: &str, workspace: &Path) -> Value {
    let is_error = output.is_error();
    match render(output, workspace) {
        Some(r) if !is_error => json!({
            "content": [{ "type": "text", "text": r.summary }],
            "structuredContent": r.meta,
            "isError": false,
            "_meta": plugin::diff_result_meta()
        }),
        _ => json!({
            "content": [{ "type": "text", "text": prompt_text }],
            "isError": is_error
        }),
    }
}

/// CLI `hands call` text: same body ChatGPT sees.
pub fn text(output: &ToolOutput, prompt_text: &str, workspace: &Path) -> String {
    if output.is_error() {
        return prompt_text.to_string();
    }
    render(output, workspace)
        .map(|r| {
            if r.diff.is_empty() {
                r.summary
            } else {
                format!("{}\n\n{}", r.summary, r.diff)
            }
        })
        .unwrap_or_else(|| prompt_text.to_string())
}

fn render(output: &ToolOutput, workspace: &Path) -> Option<Rendered> {
    match output {
        ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(applied)) => {
            Some(render_search(applied, workspace))
        }
        ToolOutput::ApplyPatch(ApplyPatchOutput::Success { files, .. }) => {
            Some(render_patch(files, workspace))
        }
        _ => None,
    }
}

fn render_search(applied: &SearchReplaceEditsApplied, workspace: &Path) -> Rendered {
    let path = rel(&applied.absolute_path, workspace);
    let mut added = 0i64;
    let mut removed = 0i64;
    let mut body = String::new();

    if applied.edits.details.is_empty() {
        let old = ensure_nl(applied.old_string.clone());
        let new = ensure_nl(applied.new_string.clone());
        let (a, r) = line_diff(&old, &new);
        added = a;
        removed = r;
        body = unified(&path, &old, &new);
    } else {
        for detail in &applied.edits.details {
            let (old, new) = snippet(detail);
            let (a, r) = line_diff(&old, &new);
            added += a;
            removed += r;
            body.push_str(&unified(&path, &old, &new));
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
    }

    let kind = if applied.old_string.is_empty() {
        "created"
    } else {
        "edited"
    };

    let summary = summary_line(kind, &path, added, removed);
    let diff = truncate(&body);
    Rendered {
        summary,
        diff: diff.clone(),
        meta: json!({
            "kind": kind,
            "path": path,
            "added": added,
            "removed": removed,
            "diff": diff,
        }),
    }
}

fn render_patch(
    files: &[xai_grok_tools::types::output::ApplyPatchFileResult],
    workspace: &Path,
) -> Rendered {
    let mut added = 0i64;
    let mut removed = 0i64;
    let mut body = String::new();
    let mut paths = Vec::new();

    for file in files {
        let path = rel(&file.path, workspace);
        let old = file.old_text.as_deref().unwrap_or("");
        let (a, r) = line_diff(old, &file.new_text);
        added += a;
        removed += r;
        paths.push(json!({
            "path": path,
            "action": file.action,
            "added": a,
            "removed": r,
        }));
        let header_path = file
            .move_to
            .as_ref()
            .map(|p| rel(p, workspace))
            .unwrap_or_else(|| path.clone());
        let label = if file.action == "moved" {
            format!("{path} → {header_path}")
        } else {
            header_path
        };
        body.push_str(&unified(&label, old, &file.new_text));
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }

    let label = match files {
        [one] => rel(&one.path, workspace),
        _ => format!("{} files", files.len()),
    };
    let summary = summary_line("patched", &label, added, removed);
    let diff = truncate(&body);
    Rendered {
        summary,
        diff: diff.clone(),
        meta: json!({
            "kind": "patched",
            "path": label,
            "files": paths,
            "added": added,
            "removed": removed,
            "diff": diff,
        }),
    }
}

fn snippet(detail: &SearchReplaceEditDetail) -> (String, String) {
    let old = format!(
        "{}{}{}",
        detail.context_before, detail.old_string, detail.context_after
    );
    let new = format!(
        "{}{}{}",
        detail.context_before, detail.new_string, detail.context_after
    );
    // Matched substrings often omit the file's trailing newline; don't
    // show a fake "\\ No newline at end of file" in the ChatGPT card.
    (ensure_nl(old), ensure_nl(new))
}

fn ensure_nl(s: String) -> String {
    if s.is_empty() || s.ends_with('\n') {
        s
    } else {
        format!("{s}\n")
    }
}

fn unified(path: &str, old: &str, new: &str) -> String {
    if old == new {
        return format!("# {path}: no textual change\n");
    }
    if old.len() > MAX_FILE || new.len() > MAX_FILE {
        let (a, r) = line_diff(old, new);
        return format!("# {path}: too large to diff (+{a} −{r})\n");
    }
    let diff = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new);
    let (old_h, new_h) = headers(path, old, new);
    let text = diff
        .unified_diff()
        .context_radius(CONTEXT)
        .header(&old_h, &new_h)
        .to_string();
    if text.is_empty() {
        format!("# {path}: no textual change\n")
    } else {
        text
    }
}

fn headers(path: &str, old: &str, new: &str) -> (String, String) {
    if old.is_empty() {
        ("/dev/null".into(), format!("b/{path}"))
    } else if new.is_empty() {
        (format!("a/{path}"), "/dev/null".into())
    } else {
        (format!("a/{path}"), format!("b/{path}"))
    }
}

fn summary_line(kind: &str, path: &str, added: i64, removed: i64) -> String {
    match (added, removed) {
        (0, 0) => format!("{kind} {path}"),
        (a, 0) => format!("{kind} {path}  (+{a})"),
        (0, r) => format!("{kind} {path}  (−{r})"),
        (a, r) => format!("{kind} {path}  (+{a} −{r})"),
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_DIFF_CHARS {
        return s.to_string();
    }
    let mut end = MAX_DIFF_CHARS;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let cut = s[..end].rfind('\n').unwrap_or(end);
    format!("{}... (diff truncated)", &s[..cut])
}

fn rel(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use xai_grok_tools::types::output::SearchReplaceEditContextInformation;

    fn applied(old: &str, new: &str) -> SearchReplaceEditsApplied {
        SearchReplaceEditsApplied {
            old_string: old.to_string(),
            new_string: new.to_string(),
            tool_output_for_prompt: "ok".into(),
            tool_output_for_prompt_concise: None,
            absolute_path: PathBuf::from("/repo/src/foo.rs"),
            edits: SearchReplaceEditContextInformation {
                details: vec![SearchReplaceEditDetail {
                    old_string: old.to_string(),
                    old_line: 2,
                    new_string: new.to_string(),
                    new_line: 2,
                    context_before: "fn main() {\n".into(),
                    context_after: "\n}\n".into(),
                    line_prefix: String::new(),
                }],
            },
            patch: None,
            unicode_normalized: false,
        }
    }

    #[test]
    fn edit_result_includes_unified_diff() {
        let out = ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(applied(
            "    let x = 1;\n",
            "    let x = 2;\n",
        )));
        let text = text(&out, "updated", Path::new("/repo"));
        assert!(text.contains("edited src/foo.rs"), "{text}");
        assert!(text.contains("-    let x = 1;"), "{text}");
        assert!(text.contains("+    let x = 2;"), "{text}");
        let mcp = mcp_result(&out, "updated", Path::new("/repo"));
        assert_eq!(mcp["isError"], false);
        assert_eq!(mcp["structuredContent"]["added"], 1);
        assert_eq!(mcp["structuredContent"]["removed"], 1);
        assert_eq!(mcp["_meta"]["openai/outputTemplate"], crate::plugin::DIFF_URI);
        assert!(
            mcp["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("edited src/foo.rs"),
            "{}",
            mcp["content"][0]["text"]
        );
        assert!(mcp["structuredContent"]["diff"].as_str().unwrap().contains("-    let x = 1;"));
    }

    #[test]
    fn created_file_uses_dev_null() {
        let mut a = applied("", "hello\n");
        a.edits.details[0].context_before.clear();
        a.edits.details[0].context_after.clear();
        let out = ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(a));
        let text = text(&out, "created", Path::new("/repo"));
        assert!(text.contains("created src/foo.rs"), "{text}");
        assert!(text.contains("/dev/null"), "{text}");
        assert!(text.contains("+hello"), "{text}");
    }

    #[test]
    fn errors_keep_original_prompt() {
        let out = ToolOutput::SearchReplace(SearchReplaceOutput::NoMatchesFound(
            xai_grok_tools::types::output::NoMatchesFoundError {
                message: "no match".into(),
                file_path: PathBuf::from("/repo/src/foo.rs"),
                file_snapshot_at_edit: None,
            },
        ));
        let mcp = mcp_result(&out, "no match", Path::new("/repo"));
        assert_eq!(mcp["isError"], true);
        assert_eq!(mcp["content"][0]["text"], "no match");
        assert!(mcp.get("structuredContent").is_none());
    }

    #[test]
    fn unified_headers_for_delete() {
        let d = unified("gone.rs", "bye\n", "");
        assert!(d.contains("a/gone.rs"), "{d}");
        assert!(d.contains("/dev/null"), "{d}");
        assert!(d.contains("-bye"), "{d}");
    }
}
