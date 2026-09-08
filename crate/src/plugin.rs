//! ChatGPT plugin chrome: titles, annotations, invocation text, skills.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SKILL_MD: &str = include_str!("../skills/hands-code/SKILL.md");
const SKILL_URI: &str = "skill://hands/hands-code/SKILL.md";

pub struct Face {
    pub title: &'static str,
    pub invoking: &'static str,
    pub invoked: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

/// Host confirmation (ChatGPT):
/// - `read_only` → auto-run
/// - write + not `destructive` → auto under **Important actions**
/// - `destructive` → confirm unless the app is **Never ask**
pub fn face(name: &str) -> Face {
    match name {
        "workspace_info" => Face {
            title: "Current workspace",
            invoking: "Checking workspace…",
            invoked: "Workspace ready",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "set_workspace" => Face {
            title: "Switch workspace",
            invoking: "Switching workspace…",
            invoked: "Workspace switched",
            read_only: false,
            destructive: false,
            idempotent: true,
        },
        "read_file" => Face {
            title: "Read file",
            invoking: "Reading file…",
            invoked: "Read",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "grep" => Face {
            title: "Search files",
            invoking: "Searching files…",
            invoked: "Search done",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "list_dir" => Face {
            title: "List folder",
            invoking: "Listing folder…",
            invoked: "Listed",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "glob" => Face {
            title: "Find files",
            invoking: "Finding files…",
            invoked: "Found files",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "get_task_output" => Face {
            title: "Command output",
            invoking: "Reading output…",
            invoked: "Got output",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        "search_replace" => Face {
            title: "Edit file",
            invoking: "Editing file…",
            invoked: "Edited",
            read_only: false,
            destructive: false,
            idempotent: false,
        },
        "todo_write" => Face {
            title: "Update todos",
            invoking: "Updating todos…",
            invoked: "Todos updated",
            read_only: false,
            destructive: false,
            idempotent: true,
        },
        "write" => Face {
            title: "Write file",
            invoking: "Writing file…",
            invoked: "Wrote",
            read_only: false,
            destructive: false,
            idempotent: true,
        },
        "apply_patch" => Face {
            title: "Apply patch",
            invoking: "Applying patch…",
            invoked: "Patched",
            read_only: false,
            destructive: false,
            idempotent: false,
        },
        "run_terminal_cmd" => Face {
            title: "Run command",
            invoking: "Running command…",
            invoked: "Command finished",
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        "run_command" => Face {
            title: "Run native command",
            invoking: "Running native command…",
            invoked: "Native command finished",
            read_only: false,
            destructive: true,
            idempotent: false,
        },
        "kill_task" => Face {
            title: "Stop command",
            invoking: "Stopping command…",
            invoked: "Stopped",
            read_only: false,
            destructive: true,
            idempotent: true,
        },
        "list_terminal_tasks" => Face {
            title: "List tasks",
            invoking: "Listing tasks...",
            invoked: "Listed tasks",
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        _ => Face {
            title: "Hands tool",
            invoking: "Working…",
            invoked: "Done",
            read_only: false,
            destructive: false,
            idempotent: false,
        },
    }
}

const WORKSPACE_ARG: &str = "Optional folder for this call only (absolute, ~/…, or name under ~/Dev). Needed when the host does not send openai/session. Hands strips this before the file tool runs.";

fn with_workspace_field(mut schema: Value) -> Value {
    if !schema.is_object() {
        schema = json!({ "type": "object", "properties": {} });
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return schema;
    }
    if schema.get("properties").is_none() {
        schema["properties"] = json!({});
    }
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            "workspace".into(),
            json!({
                "type": "string",
                "description": WORKSPACE_ARG
            }),
        );
    }
    schema
}

pub fn tool_descriptor(name: &str, description: &str, input_schema: Value) -> Value {
    let f = face(name);
    // No widget meta for edit tools: keep `invoking`/`invoked` status text but
    // do not advertise a diff template to ChatGPT. The model still receives the
    // full diff via `structuredContent`; the operator does not want an iframe
    // hydrated in the chat (heavy UI).
    let meta = json!({
        "openai/toolInvocation/invoking": f.invoking,
        "openai/toolInvocation/invoked": f.invoked,
    });
    let schema = if name == "set_workspace" {
        input_schema
    } else {
        with_workspace_field(input_schema)
    };
    json!({
        "name": name,
        "title": f.title,
        "description": description,
        "inputSchema": schema,
        "annotations": {
            "title": f.title,
            "readOnlyHint": f.read_only,
            "destructiveHint": f.destructive,
            "openWorldHint": false,
            "idempotentHint": f.idempotent,
        },
        "_meta": meta
    })
}

pub fn initialize_capabilities() -> Value {
    json!({
        "tools": { "listChanged": false },
        "resources": { "listChanged": false },
        "extensions": {
            "io.modelcontextprotocol/skills": {}
        }
    })
}

pub fn initialize_instructions(workspace: &str) -> String {
    format!(
        "Hands: local coding tools, no model. Default folder: {workspace}. \
         Each ChatGPT conversation has its own workspace (openai/session). \
         set_workspace in this chat does not change other chats. \
         If unsure, pass workspace on later tool calls. Use skill hands-code. \
         Reads auto-run. File edits are routine. Shell/kill may confirm unless \
         Apps → Hands → Never ask. Long commands: background + get_task_output."
    )
}

fn skill_digest() -> String {
    format!("sha256:{:x}", Sha256::digest(SKILL_MD.as_bytes()))
}

fn skill_entry() -> Value {
    json!({
        "uri": SKILL_URI,
        "frontmatter": {
            "name": "hands-code",
            "description": "Read, edit, and run code on the user's local machine via Hands MCP tools. Use when the user wants to work in a repo, fix a bug, run tests, or switch workspaces on this computer."
        },
        "resources": [{
            "uri": SKILL_URI,
            "digest": skill_digest()
        }]
    })
}

pub fn skills_list() -> Value {
    json!({ "skills": [skill_entry()] })
}

pub fn skills_get(params: &Value) -> Result<Value, (i64, String, Value)> {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
    if uri != SKILL_URI {
        return Err((-32602, format!("unknown skill uri: {uri}"), Value::Null));
    }
    Ok(json!({ "skill": skill_entry() }))
}

pub fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": SKILL_URI,
                "name": "hands-code",
                "mimeType": "text/markdown",
                "description": "Hands local coding workflow"
            }
        ]
    })
}

pub fn resources_read(params: &Value) -> Result<Value, (i64, String, Value)> {
    let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
    if uri == SKILL_URI {
        return Ok(json!({
            "contents": [{
                "uri": SKILL_URI,
                "mimeType": "text/markdown",
                "text": SKILL_MD
            }]
        }));
    }
    Err((-32602, format!("unknown resource uri: {uri}"), Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_tools_have_no_widget_meta() {
        let d = tool_descriptor("search_replace", "edit", json!({ "type": "object" }));
        assert!(d["_meta"].get("openai/outputTemplate").is_none());
        assert!(d["_meta"].get("ui").is_none());
        assert!(d["inputSchema"]["properties"].get("workspace").is_some());
        assert!(d["inputSchema"]["properties"]["workspace"]["description"]
            .as_str()
            .unwrap()
            .contains("openai/session"));
        let read = tool_descriptor("read_file", "read", json!({ "type": "object" }));
        assert!(read["_meta"].get("openai/outputTemplate").is_none());
        let set = tool_descriptor(
            "set_workspace",
            "pin",
            json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        );
        assert!(set["inputSchema"]["properties"].get("workspace").is_none());
    }

    #[test]
    fn resources_list_only_exposes_skill() {
        let list = resources_list();
        let uris: Vec<&str> = list["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert_eq!(uris, vec![SKILL_URI]);
        let got = resources_read(&json!({ "uri": SKILL_URI })).unwrap();
        assert!(got["contents"][0]["text"].as_str().unwrap().contains("Hands"));
        assert!(resources_read(&json!({ "uri": "ui://widget/diff-v1.html" })).is_err());
    }
}
