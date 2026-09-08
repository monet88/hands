//! After an edit, show a unified diff of what changed. No preview step.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use similar::TextDiff;
use xai_grok_tools::types::output::{
    ApplyPatchOutput, SearchReplaceEditDetail, SearchReplaceEditsApplied, SearchReplaceOutput,
    ToolOutput, line_diff,
};

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
            // No widget `_meta`: the model reads the full diff from
            // `structuredContent`, but we don't hydrate a diff iframe in
            // ChatGPT (it made the UI heavy and the operator does not read it).
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
        let updated_file = std::fs::read_to_string(&applied.absolute_path)
            .ok()
            .map(|s| s.replace("\r\n", "\n"));
        let file_lines: Option<Vec<&str>> = updated_file
            .as_deref()
            .map(|s| s.split_inclusive('\n').collect());

        let mut line_counts = std::collections::HashMap::new();
        for detail in &applied.edits.details {
            *line_counts.entry(detail.new_line).or_insert(0) += 1;
        }

        for detail in &applied.edits.details {
            let suffix = if line_counts.get(&detail.new_line).copied().unwrap_or(0) > 1 {
                None
            } else {
                file_lines.as_deref().and_then(|lines| {
                    derive_suffix(
                        lines,
                        detail.new_line,
                        &detail.line_prefix,
                        &detail.new_string,
                    )
                })
            };
            let (old, new) = snippet(detail, suffix);
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

fn derive_suffix<'a>(
    lines: &[&'a str],
    new_line: usize,
    line_prefix: &str,
    new_string: &str,
) -> Option<&'a str> {
    if new_line == 0 {
        return None;
    }
    let start_idx = new_line - 1;
    if start_idx >= lines.len() {
        return None;
    }

    let line_prefix_norm = line_prefix.replace("\r\n", "\n");
    let new_string_norm = new_string.replace("\r\n", "\n");

    let new_lines: Vec<&str> = if new_string_norm.is_empty() {
        vec![""]
    } else {
        new_string_norm.split_inclusive('\n').collect()
    };

    let end_idx = start_idx + new_lines.len() - 1;
    if end_idx >= lines.len() {
        return None;
    }

    if !lines[start_idx].starts_with(&line_prefix_norm) {
        return None;
    }
    let after_prefix = &lines[start_idx][line_prefix_norm.len()..];

    if new_lines.len() == 1 {
        if !after_prefix.starts_with(new_lines[0]) {
            return None;
        }
        return Some(&after_prefix[new_lines[0].len()..]);
    }

    if after_prefix != new_lines[0] {
        return None;
    }

    for (k, expected_line) in new_lines.iter().enumerate().take(new_lines.len() - 1).skip(1) {
        if lines[start_idx + k] != *expected_line {
            return None;
        }
    }

    let last_new = *new_lines.last().unwrap();
    if !lines[end_idx].starts_with(last_new) {
        return None;
    }
    Some(&lines[end_idx][last_new.len()..])
}

fn snippet(detail: &SearchReplaceEditDetail, suffix: Option<&str>) -> (String, String) {
    let prefix = &detail.line_prefix;
    if let Some(sfx) = suffix {
        let (old_sfx, new_sfx) = if sfx.is_empty()
            && detail.new_string.ends_with('\n')
            && !detail.old_string.is_empty()
            && !detail.old_string.ends_with('\n')
        {
            ("\n", "")
        } else {
            (sfx, sfx)
        };
        let old = format!(
            "{}{}{}{}{}",
            detail.context_before, prefix, detail.old_string, old_sfx, detail.context_after
        );
        let new = format!(
            "{}{}{}{}{}",
            detail.context_before, prefix, detail.new_string, new_sfx, detail.context_after
        );
        return (
            ensure_nl(old.replace("\r\n", "\n")),
            ensure_nl(new.replace("\r\n", "\n")),
        );
    }

    let (old_suffix, new_suffix) = if !detail.context_after.is_empty()
        && !detail.context_after.starts_with('\n')
        && !detail.context_after.starts_with("\r\n")
    {
        let old_needs_nl = !detail.old_string.is_empty() && !detail.old_string.ends_with('\n') && !detail.old_string.ends_with("\r\n");
        let new_needs_nl = !detail.new_string.ends_with('\n') && !detail.new_string.ends_with("\r\n");
        let old_sep = if old_needs_nl { "\n" } else { "" };
        let new_sep = if new_needs_nl { "\n" } else { "" };
        (
            format!("{}{}", old_sep, detail.context_after),
            format!("{}{}", new_sep, detail.context_after),
        )
    } else {
        (detail.context_after.clone(), detail.context_after.clone())
    };

    let old = format!(
        "{}{}{}{}",
        detail.context_before, prefix, detail.old_string, old_suffix
    );
    let new = format!(
        "{}{}{}{}",
        detail.context_before, prefix, detail.new_string, new_suffix
    );
    (ensure_nl(old.replace("\r\n", "\n")), ensure_nl(new.replace("\r\n", "\n")))
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

    fn test_detail_helper(prefix: &str, old: &str, new: &str) -> SearchReplaceEditDetail {
        SearchReplaceEditDetail {
            old_string: old.to_string(),
            old_line: 2,
            new_string: new.to_string(),
            new_line: 2,
            context_before: "fn main() {\n".into(),
            context_after: "}\n".into(),
            line_prefix: prefix.to_string(),
        }
    }

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
        assert!(mcp.get("_meta").is_none(), "no widget meta for edits");
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

    #[test]
    fn snippet_includes_line_prefix_and_normalizes_crlf() {
        let detail = test_detail_helper("    let x = ", "1;\r\n", "2;\r\n");
        let (old, new) = snippet(&detail, None);
        assert_eq!(old, "fn main() {\n    let x = 1;\n}\n");
        assert_eq!(new, "fn main() {\n    let x = 2;\n}\n");

        let d = unified("src/foo.rs", &old, &new);
        assert!(d.contains("-    let x = 1;\n"), "{d}");
        assert!(d.contains("+    let x = 2;\n"), "{d}");
    }

    #[test]
    fn snippet_preserves_line_boundary_when_context_after_lacks_leading_newline() {
        let detail = SearchReplaceEditDetail {
            old_string: "foo".to_string(),
            old_line: 2,
            new_string: "bar".to_string(),
            new_line: 2,
            context_before: "header\n".into(),
            context_after: "footer\n".into(),
            line_prefix: "prefix_".to_string(),
        };
        let (old, new) = snippet(&detail, None);
        assert_eq!(old, "header\nprefix_foo\nfooter\n");
        assert_eq!(new, "header\nprefix_bar\nfooter\n");

        let d = unified("src/foo.rs", &old, &new);
        assert!(d.contains("-prefix_foo\n"), "{d}");
        assert!(d.contains("+prefix_bar\n"), "{d}");
        assert!(d.contains(" footer\n"), "{d}");
    }

    #[test]
    fn snippet_with_authoritative_suffix_recovers_same_line_context() {
        let detail = SearchReplaceEditDetail {
            old_string: "hello".to_string(),
            old_line: 1,
            new_string: "hello world".to_string(),
            new_line: 1,
            context_before: "".into(),
            context_after: "".into(),
            line_prefix: "say ".to_string(),
        };
        let file_content = "say hello world now\n";
        let lines: Vec<&str> = file_content.split_inclusive('\n').collect();
        let suffix = derive_suffix(&lines, detail.new_line, &detail.line_prefix, &detail.new_string);
        assert_eq!(suffix, Some(" now\n"));

        let (old, new) = snippet(&detail, suffix);
        assert_eq!(old, "say hello now\n");
        assert_eq!(new, "say hello world now\n");

        let d = unified("src/inline.rs", &old, &new);
        assert!(d.contains("-say hello now\n"), "{d}");
        assert!(d.contains("+say hello world now\n"), "{d}");
    }

    #[test]
    fn derive_suffix_fails_conservatively_on_mismatched_content() {
        let detail = SearchReplaceEditDetail {
            old_string: "hello".to_string(),
            old_line: 1,
            new_string: "hello world".to_string(),
            new_line: 1,
            context_before: "".into(),
            context_after: "".into(),
            line_prefix: "say ".to_string(),
        };
        let file_content = "different text entirely\n";
        let lines: Vec<&str> = file_content.split_inclusive('\n').collect();
        let suffix = derive_suffix(&lines, detail.new_line, &detail.line_prefix, &detail.new_string);
        assert_eq!(suffix, None);
    }
}
