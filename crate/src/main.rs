//! Global Grok harness CLI: no model, no Computer Hub.
//!
//! Pin any repo, then ChatGPT talks to that workspace through the tunnel:
//!
//! ```text
//! cd /any/project
//! grok-harness use
//! ```

mod host;
mod mcp;
mod service;

use std::net::SocketAddr;
use std::path::PathBuf;

const USAGE: &str = "\
grok-harness — local coding tools for ChatGPT Web (no model)

  cd /any/repo && grok-harness use     pin this folder as the workspace
  grok-harness status                  show pin + tunnel
  grok-harness enable                  auto-start tunnel at login (KeepAlive)
  grok-harness disable                 remove auto-start
  grok-harness start                   start tunnel now
  grok-harness stop                    stop tunnel now
  grok-harness                         MCP stdio (used by tunnel-client)

Debug:
  grok-harness list
  grok-harness call <tool> <json>
  grok-harness --http [--port N]
";

enum Cmd {
    Use { dir: PathBuf },
    Status,
    Enable,
    Disable,
    Start,
    Stop,
    McpStdio,
    McpHttp { addr: SocketAddr },
    List,
    Call { tool: String, args_json: String },
}

fn parse_args() -> Result<(PathBuf, Cmd), String> {
    let mut fallback = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut http = false;
    let mut port: u16 = 8787;
    let mut rest = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cwd" => {
                let value = args.next().ok_or("--cwd requires a directory")?;
                fallback = PathBuf::from(value);
            }
            "--http" => http = true,
            "--port" => {
                let value = args.next().ok_or("--port requires a number")?;
                port = value.parse().map_err(|_| "invalid --port")?;
            }
            "-V" | "--version" => {
                println!("grok-harness {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => return Err(USAGE.trim_end().to_string()),
            _ => {
                rest.push(arg);
                rest.extend(args);
                break;
            }
        }
    }
    let cmd = if http {
        Cmd::McpHttp {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    } else {
        match rest.as_slice() {
            [] => Cmd::McpStdio,
            [op] if op == "mcp" => Cmd::McpStdio,
            [op] if op == "use" => Cmd::Use {
                dir: fallback.clone(),
            },
            [op, dir] if op == "use" => Cmd::Use {
                dir: PathBuf::from(dir),
            },
            [op] if op == "status" => Cmd::Status,
            [op] if op == "enable" => Cmd::Enable,
            [op] if op == "disable" => Cmd::Disable,
            [op] if op == "start" => Cmd::Start,
            [op] if op == "stop" => Cmd::Stop,
            [op] if op == "list" => Cmd::List,
            [op, tool, json] if op == "call" => Cmd::Call {
                tool: tool.clone(),
                args_json: json.clone(),
            },
            _ => return Err(USAGE.trim_end().to_string()),
        }
    };
    Ok((fallback, cmd))
}

#[tokio::main]
async fn main() {
    let (fallback, cmd) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(if e.starts_with("grok-harness") { 0 } else { 2 });
        }
    };
    if let Err(e) = run(fallback, cmd).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(fallback: PathBuf, cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Use { dir } => {
            let cwd = host::pin_workspace(&dir)?;
            println!("{}", cwd.display());
            match service::ensure() {
                Ok(true) => {
                    eprintln!("pinned. ChatGPT uses this folder on the next tool call (call workspace_info).");
                }
                Ok(false) => {
                    eprintln!(
                        "pinned, but tunnel is down. One-time:\n  export CONTROL_PLANE_API_KEY=...\n  grok-harness enable"
                    );
                }
                Err(e) => eprintln!("pinned, tunnel start failed: {e}"),
            }
            Ok(())
        }
        Cmd::Status => {
            let cwd = host::resolve_workspace(&fallback);
            let pin = host::read_pinned_workspace();
            println!("workspace  {}", cwd.display());
            println!(
                "pin        {}",
                pin.map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(none — using cwd/env)".into())
            );
            println!("tunnel     {}", service::status_line());
            Ok(())
        }
        Cmd::Enable => service::enable(),
        Cmd::Disable => service::disable(),
        Cmd::Start => service::start(),
        Cmd::Stop => service::stop(),
        Cmd::McpStdio => mcp::McpHost::new(fallback).serve_stdio().await,
        Cmd::McpHttp { addr } => mcp::McpHost::new(fallback).serve_http(addr).await,
        Cmd::List | Cmd::Call { .. } => {
            let cwd = host::resolve_workspace(&fallback);
            let bridge = host::build_bridge(cwd).await?;
            match cmd {
                Cmd::List => {
                    let defs = bridge.tool_definitions().await;
                    let tools: Vec<serde_json::Value> = defs
                        .into_iter()
                        .map(|d| {
                            serde_json::json!({
                                "name": d.function.name,
                                "description": d.function.description,
                                "parameters": d.function.parameters,
                            })
                        })
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({ "tools": tools }))
                            .map_err(|e| e.to_string())?
                    );
                    Ok(())
                }
                Cmd::Call { tool, args_json } => {
                    let params: serde_json::Value = serde_json::from_str(&args_json)
                        .map_err(|e| format!("invalid json args: {e}"))?;
                    let result = bridge
                        .call(&tool, params, "grok-harness-1")
                        .await
                        .map_err(|e| e.to_string())?;
                    println!("{}", result.prompt_text);
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
    }
}


