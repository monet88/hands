//! ChatGPT plugin chrome: titles, annotations, invocation text, skills, widgets.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SKILL_MD: &str = include_str!("../skills/hands-code/SKILL.md");
const SKILL_URI: &str = "skill://hands/hands-code/SKILL.md";
pub const DIFF_URI: &str = "ui://widget/diff-v1.html";
const DIFF_HTML: &str = include_str!("widgets/diff.html");
const DIFF_MIME_CHATGPT: &str = "text/html+skybridge";
const DIFF_MIME_APPS: &str = "text/html;profile=mcp-app";

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "search_replace" | "write" | "apply_patch")
}

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
    let mut meta = json!({
        "openai/toolInvocation/invoking": f.invoking,
        "openai/toolInvocation/invoked": f.invoked,
    });
    if is_edit_tool(name) {
        meta["ui"] = json!({ "resourceUri": DIFF_URI });
        meta["openai/outputTemplate"] = json!(DIFF_URI);
        meta["openai/resultCanProduceWidget"] = json!(true);
        meta["openai/widgetAccessible"] = json!(false);
        meta["openai/widgetDescription"] =
            json!("Inline unified diff of the file just written on this machine.");
    }
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

fn widget_resource_meta() -> Value {
    json!({
        "ui": {
            "prefersBorder": true,
            "csp": {
                "connectDomains": [],
                "resourceDomains": [],
            }
        },
        "openai/widgetPrefersBorder": true,
        "openai/widgetDescription": "Unified diff of the edit that just landed.",
        "openai/widgetCSP": {
            "connect_domains": [],
            "resource_domains": [],
        }
    })
}

/// `_meta` on a successful edit `tools/call` so ChatGPT hydrates the iframe.
pub fn diff_result_meta() -> Value {
    json!({
        "ui": { "resourceUri": DIFF_URI },
        "openai/outputTemplate": DIFF_URI,
        "openai.com/widget": {
            "type": "resource",
            "resource": {
                "uri": DIFF_URI,
                "mimeType": DIFF_MIME_CHATGPT,
                "text": DIFF_HTML,
                "_meta": widget_resource_meta()
            }
        }
    })
}

pub fn initialize_capabilities() -> Value {
    json!({
        "tools": { "listChanged": false },
        "resources": { "listChanged": false },
        "extensions": {
            "io.modelcontextprotocol/skills": {},
            "io.modelcontextprotocol/ui": {}
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

fn diff_resource(mime: &str) -> Value {
    json!({
        "uri": DIFF_URI,
        "name": "edit-diff",
        "title": "Edit diff",
        "mimeType": mime,
        "description": "Inline unified diff after search_replace, write, or apply_patch",
        "_meta": widget_resource_meta()
    })
}

pub fn resources_list() -> Value {
    json!({
        "resources": [
            {
                "uri": SKILL_URI,
                "name": "hands-code",
                "mimeType": "text/markdown",
                "description": "Hands local coding workflow"
            },
            diff_resource(DIFF_MIME_CHATGPT)
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
    if uri == DIFF_URI {
        return Ok(json!({
            "contents": [
                {
                    "uri": DIFF_URI,
                    "mimeType": DIFF_MIME_CHATGPT,
                    "text": DIFF_HTML,
                    "_meta": widget_resource_meta()
                },
                {
                    "uri": DIFF_URI,
                    "mimeType": DIFF_MIME_APPS,
                    "text": DIFF_HTML,
                    "_meta": widget_resource_meta()
                }
            ]
        }));
    }
    Err((-32602, format!("unknown resource uri: {uri}"), Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_tools_advertise_widget() {
        let d = tool_descriptor("search_replace", "edit", json!({ "type": "object" }));
        assert_eq!(d["_meta"]["openai/outputTemplate"], DIFF_URI);
        assert_eq!(d["_meta"]["ui"]["resourceUri"], DIFF_URI);
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
    fn diff_resource_is_readable() {
        let list = resources_list();
        let uris: Vec<&str> = list["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert!(uris.contains(&DIFF_URI));
        let got = resources_read(&json!({ "uri": DIFF_URI })).unwrap();
        let html = got["contents"][0]["text"].as_str().unwrap();
        assert!(html.contains("ui/notifications/tool-result"));
        assert_eq!(got["contents"][0]["mimeType"], DIFF_MIME_CHATGPT);
    }
}
