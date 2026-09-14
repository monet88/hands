use std::env;
use std::path::PathBuf;

use hands_bridge::host::{
    LocalInitOptions, NotifyOptions, PrepareOptions, SetupOptions, TargetAddOptions,
    TargetListOptions, TargetRemoveOptions, execute_local_init, execute_notify, execute_prepare,
    execute_setup, execute_target_add, execute_target_list, execute_target_remove,
    resolve_state_dir, run_native_host,
};
use hands_bridge::journal::{ExplicitNotificationStatus, Journal};
use hands_bridge::protocol::TRUST_NOTICE;
use serde_json::json;

/// Non-empty, trimmed environment variable used by the one-command worker flow.
fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn print_help() {
    eprintln!(
        r#"Hands Return Bridge Companion CLI
Usage:
  hands-bridge native-host [--state-dir <dir>]
  hands-bridge local-init --extension-id <id> [--browser <chrome|edge>] [--state-dir <dir>] [--skip-registry]
  hands-bridge setup --target <path> --profile <profile_id> --extension-id <id> --policy-revision <rev> --tool-policy <policy> --approval-policy <policy> [options]
  hands-bridge status [--state-dir <dir>]
  hands-bridge revoke --pairing-id <id> [--state-dir <dir>]
  hands-bridge target add --target <path> [--target-id <id>] [--pairing-id <id>] [--state-dir <dir>]
  hands-bridge target remove --target-id <id> [--pairing-id <id>] [--state-dir <dir>]
  hands-bridge target list [--pairing-id <id>] [--state-dir <dir>]
  hands-bridge done [--conversation <conversation_id>] [--state-dir <dir>]
  hands-bridge failed --message <text> [--conversation <conversation_id>] [--state-dir <dir>]
  hands-bridge notify done [--task <task_id>] [--execution-id <execution_id>] [--state-dir <dir>]
  hands-bridge notify failed --message <text> [--task <task_id>] [--execution-id <execution_id>] [--state-dir <dir>]
  hands-bridge prepare --conversation <conversation_id> [--pairing-id <id>] [--state-dir <dir>] [--json]

Setup Options:
  --browser <chrome|edge>       Target browser (default: chrome)
  --profile <profile_id>        Browser profile identifier (required)
  --target <path>               Canonical workspace/worktree target path (required)
  --extension-id <id>           Expected extension ID to pair and pin allowed origin (required)
  --policy-revision <rev>       Explicit launch policy revision (required, e.g. v1)
  --tool-policy <policy>        Tool policy (required, e.g. standard)
  --approval-policy <policy>    Approval policy (required, e.g. prompt)
  --target-id <id>              Target ID identifier (default: dir name)
  --state-dir <dir>             Override state directory (requires --skip-registry for setup and local-init)
  --skip-registry               Skip Windows Registry NativeMessagingHosts registration

Target Commands:
  target add                    Register a new git workspace target on active pairing
  target remove                 Remove a registered target from active pairing
  target list                   List registered targets on active pairing

Worker Commands:
  done                          One-command completion: reuse the execution this worker was
                                launched with, or claim a new one for --conversation, then
                                record the receipt. No identity arguments are needed.
  failed                        Same one-command flow for a failed run; --message is required.
  prepare                       Claim task/execution identity for a registered ChatGPT conversation
                                and print the environment for a normal Orca OMP worker
                                (no target/workspace is involved; routing follows the conversation)
  notify done                   Record explicit terminal success for a prepared/launched task
  notify failed                 Record explicit terminal failure for a prepared/launched task
"#
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Chrome/Edge Native Messaging calls the binary with the extension origin as the first arg:
    // e.g., "hands-bridge.exe chrome-extension://<id>/"
    let caller_origin = if args.len() >= 2 && args[1].starts_with("chrome-extension://") {
        Some(args[1].as_str())
    } else {
        None
    };

    if args.len() == 1
        || (args.len() >= 2
            && (args[1].starts_with("chrome-extension://")
                || args[1] == "native-host"
                || args[1] == "--native-host"))
    {
        let mut state_dir = None;
        let mut i = 1;
        while i < args.len() {
            if args[i] == "--state-dir" && i + 1 < args.len() {
                state_dir = Some(PathBuf::from(&args[i + 1]));
                i += 1;
            }
            i += 1;
        }

        if let Err(e) = run_native_host(state_dir.as_deref(), caller_origin) {
            eprintln!("Native host error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    match args[1].as_str() {
        "local-init" => {
            let mut browser = "chrome".to_string();
            let mut extension_id = None;
            let mut state_dir = None;
            let mut skip_registry = false;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--browser" if i + 1 < args.len() => {
                        browser = args[i + 1].clone();
                        i += 1;
                    }
                    "--extension-id" if i + 1 < args.len() => {
                        extension_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    "--skip-registry" => skip_registry = true,
                    "--help" | "-h" => {
                        print_help();
                        return;
                    }
                    other => {
                        eprintln!("Unknown option for local-init: {}", other);
                        std::process::exit(1);
                    }
                }
                i += 1;
            }

            let extension_id = match extension_id {
                Some(id) if !id.trim().is_empty() => id,
                _ => {
                    eprintln!("Error: --extension-id <id> is required");
                    std::process::exit(1);
                }
            };
            match execute_local_init(&LocalInitOptions {
                browser,
                extension_id,
                state_dir,
                skip_registry,
            }) {
                Ok(res) => {
                    println!("Hands Return Bridge local mode initialized");
                    println!("Browser: {}", res.browser);
                    println!("Extension ID: {}", res.extension_id);
                    println!("Manifest: {}", res.manifest_path.display());
                }
                Err(e) => {
                    eprintln!("Local init failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "setup" => {
            let mut browser = "chrome".to_string();
            let mut profile_id = None;
            let mut target_path = None;
            let mut target_id = None;
            let mut policy_revision = None;
            let mut tool_policy = None;
            let mut approval_policy = None;
            let mut extension_id = None;
            let mut state_dir = None;
            let mut skip_registry = false;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--browser" if i + 1 < args.len() => {
                        browser = args[i + 1].clone();
                        i += 1;
                    }
                    "--profile" if i + 1 < args.len() => {
                        profile_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--target" if i + 1 < args.len() => {
                        target_path = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--target-id" if i + 1 < args.len() => {
                        target_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--policy-revision" if i + 1 < args.len() => {
                        policy_revision = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--tool-policy" if i + 1 < args.len() => {
                        tool_policy = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--approval-policy" if i + 1 < args.len() => {
                        approval_policy = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--extension-id" if i + 1 < args.len() => {
                        extension_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    "--skip-registry" => {
                        skip_registry = true;
                    }
                    "--help" | "-h" => {
                        print_help();
                        return;
                    }
                    other => {
                        eprintln!("Unknown option: {}", other);
                        print_help();
                        std::process::exit(1);
                    }
                }
                i += 1;
            }

            let target = match target_path {
                Some(t) if !t.trim().is_empty() => t,
                _ => {
                    eprintln!("Error: --target <path> is required");
                    std::process::exit(1);
                }
            };

            let profile_id = match profile_id {
                Some(p) if !p.trim().is_empty() => p,
                _ => {
                    eprintln!("Error: --profile <profile_id> is required (obtain from Return Bridge extension settings)");
                    std::process::exit(1);
                }
            };

            let extension_id = match extension_id {
                Some(e) if !e.trim().is_empty() => e,
                _ => {
                    eprintln!("Error: --extension-id <id> is required (obtain from Return Bridge extension settings)");
                    std::process::exit(1);
                }
            };

            let policy_revision = match policy_revision {
                Some(r) if !r.trim().is_empty() => r,
                _ => {
                    eprintln!("Error: --policy-revision <rev> is required (e.g. v1)");
                    std::process::exit(1);
                }
            };

            let tool_policy = match tool_policy {
                Some(t) if !t.trim().is_empty() => t,
                _ => {
                    eprintln!("Error: --tool-policy <policy> is required (e.g. standard)");
                    std::process::exit(1);
                }
            };

            let approval_policy = match approval_policy {
                Some(a) if !a.trim().is_empty() => a,
                _ => {
                    eprintln!("Error: --approval-policy <policy> is required (e.g. prompt)");
                    std::process::exit(1);
                }
            };
            let opts = SetupOptions {
                browser,
                profile_id,
                target_path: target,
                target_id,
                policy_revision,
                tool_policy,
                approval_policy,
                extension_id,
                state_dir,
                skip_registry,
            };

            match execute_setup(&opts) {
                Ok(res) => {
                    println!("============================================================");
                    println!("Hands Return Bridge Setup");
                    println!("============================================================");
                    println!("{}", TRUST_NOTICE);
                    println!("============================================================");
                    println!("Pairing ID:       {}", res.pairing_id);
                    println!("Browser:          {} (profile: {})", res.browser, res.profile_id);
                    println!("Target:           {} -> {}", res.target_id, res.canonical_target_path);
                    println!("Policy Revision:  {}", res.policy_revision);
                    println!("Manifest:         {}", res.manifest_path.display());
                    println!("------------------------------------------------------------");
                    println!("Bootstrap Token:  {}", res.bootstrap_token);
                    println!("------------------------------------------------------------");
                    println!("Enter this Bootstrap Token in the Return Bridge extension");
                    println!("options page to complete pairing.");
                    println!("============================================================");
                }
                Err(e) => {
                    eprintln!("Setup failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "status" => {
            let mut state_dir = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--state-dir" && i + 1 < args.len() {
                    state_dir = Some(PathBuf::from(&args[i + 1]));
                    i += 1;
                }
                i += 1;
            }

            let dir = match resolve_state_dir(state_dir.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Failed to resolve state dir: {}", e);
                    std::process::exit(1);
                }
            };
            let db_path = dir.join("journal.sqlite");
            if !db_path.exists() {
                println!("No Return Bridge journal found at {}", db_path.display());
                return;
            }

            println!("State Directory: {}", dir.display());
            println!("Journal SQLite:  {}", db_path.display());
            let _journal = match Journal::open(&db_path) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to open journal: {}", e);
                    std::process::exit(1);
                }
            };

            println!("Status: Active journal present.");
        }
        "revoke" => {
            let mut pairing_id = None;
            let mut state_dir = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--pairing-id" => {
                        if i + 1 >= args.len() {
                            eprintln!("Error: --pairing-id requires a value");
                            std::process::exit(1);
                        }
                        pairing_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" => {
                        if i + 1 >= args.len() {
                            eprintln!("Error: --state-dir requires a value");
                            std::process::exit(1);
                        }
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    "--help" | "-h" => {
                        print_help();
                        return;
                    }
                    other => {
                        eprintln!("Unknown option for revoke: {}", other);
                        print_help();
                        std::process::exit(1);
                    }
                }
                i += 1;
            }

            let pid = match pairing_id {
                Some(id) if !id.trim().is_empty() => id.trim().to_string(),
                None => {
                    eprintln!("Error: --pairing-id <id> is required");
                    std::process::exit(1);
                }
                Some(_) => {
                    eprintln!("Error: --pairing-id <id> must not be empty or whitespace");
                    std::process::exit(1);
                }
            };

            let dir = match resolve_state_dir(state_dir.as_deref()) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Failed to resolve state dir: {}", e);
                    std::process::exit(1);
                }
            };
            let db_path = dir.join("journal.sqlite");
            let journal = match Journal::open(&db_path) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("Failed to open journal: {}", e);
                    std::process::exit(1);
                }
            };

            match journal.revoke_pairing_admin(&pid) {
                Ok(()) => {
                    println!("Pairing {} has been revoked.", pid);
                }
                Err(e) => {
                    eprintln!("Failed to revoke pairing {}: {}", pid, e);
                    std::process::exit(1);
                }
            }
        }
        "target" => {
            if args.len() < 3 {
                eprintln!("Error: target requires a subcommand: add, remove, or list");
                print_help();
                std::process::exit(1);
            }
            match args[2].as_str() {
                "add" => {
                    let mut target_path = None;
                    let mut target_id = None;
                    let mut pairing_id = None;
                    let mut state_dir = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--target" if i + 1 < args.len() => {
                                target_path = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--target-id" if i + 1 < args.len() => {
                                target_id = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--pairing-id" if i + 1 < args.len() => {
                                pairing_id = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--state-dir" if i + 1 < args.len() => {
                                state_dir = Some(PathBuf::from(&args[i + 1]));
                                i += 1;
                            }
                            "--help" | "-h" => {
                                print_help();
                                return;
                            }
                            other => {
                                eprintln!("Unknown option for target add: {}", other);
                                print_help();
                                std::process::exit(1);
                            }
                        }
                        i += 1;
                    }
                    let path = match target_path {
                        Some(p) if !p.trim().is_empty() => p,
                        _ => {
                            eprintln!("Error: --target <path> is required");
                            std::process::exit(1);
                        }
                    };
                    let opts = TargetAddOptions {
                        pairing_id,
                        target_path: path,
                        target_id,
                        state_dir,
                    };
                    match execute_target_add(&opts) {
                        Ok(rec) => {
                            println!("Target added successfully:");
                            println!("  Target ID:       {}", rec.target_id);
                            println!("  Canonical Path:  {}", rec.canonical_path);
                            println!("  Name:            {}", rec.name);
                        }
                        Err(e) => {
                            eprintln!("Failed to add target: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                "remove" | "rm" => {
                    let mut target_id = None;
                    let mut pairing_id = None;
                    let mut state_dir = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--target-id" if i + 1 < args.len() => {
                                target_id = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--pairing-id" if i + 1 < args.len() => {
                                pairing_id = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--state-dir" if i + 1 < args.len() => {
                                state_dir = Some(PathBuf::from(&args[i + 1]));
                                i += 1;
                            }
                            "--help" | "-h" => {
                                print_help();
                                return;
                            }
                            other => {
                                eprintln!("Unknown option for target remove: {}", other);
                                print_help();
                                std::process::exit(1);
                            }
                        }
                        i += 1;
                    }
                    let tid = match target_id {
                        Some(id) if !id.trim().is_empty() => id,
                        _ => {
                            eprintln!("Error: --target-id <id> is required");
                            std::process::exit(1);
                        }
                    };
                    let opts = TargetRemoveOptions {
                        pairing_id,
                        target_id: tid.clone(),
                        state_dir,
                    };
                    match execute_target_remove(&opts) {
                        Ok(()) => {
                            println!("Target '{}' removed successfully.", tid);
                        }
                        Err(e) => {
                            eprintln!("Failed to remove target: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                "list" | "ls" => {
                    let mut pairing_id = None;
                    let mut state_dir = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--pairing-id" if i + 1 < args.len() => {
                                pairing_id = Some(args[i + 1].clone());
                                i += 1;
                            }
                            "--state-dir" if i + 1 < args.len() => {
                                state_dir = Some(PathBuf::from(&args[i + 1]));
                                i += 1;
                            }
                            "--help" | "-h" => {
                                print_help();
                                return;
                            }
                            other => {
                                eprintln!("Unknown option for target list: {}", other);
                                print_help();
                                std::process::exit(1);
                            }
                        }
                        i += 1;
                    }
                    let opts = TargetListOptions {
                        pairing_id,
                        state_dir,
                    };
                    match execute_target_list(&opts) {
                        Ok(targets) => {
                            if targets.is_empty() {
                                println!("No targets registered on active pairing.");
                            } else {
                                println!("Registered targets ({}):", targets.len());
                                for t in targets {
                                    println!("  - ID:   {}", t.target_id);
                                    println!("    Path: {}", t.canonical_path);
                                    println!("    Name: {}", t.name);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to list targets: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                other => {
                    eprintln!("Unknown target subcommand: {}", other);
                    print_help();
                    std::process::exit(1);
                }
            }
        }
        "done" | "failed" => {
            let sub = args[1].as_str();
            let mut conversation_id: Option<String> = None;
            let mut message = None;
            let mut state_dir = None;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--conversation" if i + 1 < args.len() => {
                        conversation_id = Some(args[i + 1].trim().to_string());
                        i += 1;
                    }
                    "--message" if i + 1 < args.len() => {
                        message = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    other => {
                        eprintln!("error: unknown option '{}'", other);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }

            let status = match sub {
                "done" => {
                    if message.is_some() {
                        eprintln!("error: --message is not supported for 'done'");
                        std::process::exit(2);
                    }
                    ExplicitNotificationStatus::Done
                }
                _ => match message {
                    Some(m) if !m.trim().is_empty() => {
                        ExplicitNotificationStatus::Failed { message: m }
                    }
                    _ => {
                        eprintln!("error: --message is required for 'failed'");
                        std::process::exit(2);
                    }
                },
            };

            // A worker launched by the extension already owns an execution; anything else
            // mints one here so the caller never has to pass identity around.
            let (task_id, execution_id, notify_state_dir, conversation_id) =
                match (env_var("HANDS_TASK_ID"), env_var("HANDS_RETURN_BRIDGE_EXECUTION_ID")) {
                    (Some(task_id), Some(execution_id)) => {
                        let worker_conversation = env_var("HANDS_RETURN_BRIDGE_CONVERSATION_ID");
                        if let (Some(requested), Some(bound)) =
                            (conversation_id.as_deref(), worker_conversation.as_deref())
                        {
                            if requested != bound {
                                eprintln!(
                                    "error: --conversation {} does not match the conversation bound to this worker execution ({})",
                                    requested, bound
                                );
                                std::process::exit(1);
                            }
                        }
                        (
                            task_id,
                            execution_id,
                            state_dir
                                .clone()
                                .or_else(|| env_var("HANDS_RETURN_BRIDGE_STATE_DIR").map(PathBuf::from)),
                            conversation_id.or(worker_conversation).unwrap_or_default(),
                        )
                    }
                    _ => {
                        let conversation_id = match conversation_id
                            .or_else(|| env_var("HANDS_RETURN_BRIDGE_CONVERSATION_ID"))
                        {
                            Some(value) if !value.is_empty() => value,
                            _ => {
                                let message_flag = if sub == "failed" { " --message <text>" } else { "" };
                                eprintln!(
                                    "usage: hands-bridge {} [--conversation <conversation_id>] [--state-dir <dir>]{}",
                                    sub, message_flag
                                );
                                std::process::exit(2);
                            }
                        };
                        let options = PrepareOptions {
                            pairing_id: None,
                            conversation_id,
                            state_dir: state_dir.clone(),
                        };
                        match execute_prepare(options) {
                            Ok(worker) => (
                                worker.task_id,
                                worker.execution_id,
                                Some(PathBuf::from(worker.state_dir)),
                                worker.origin_conversation_id,
                            ),
                            Err(e) => {
                                eprintln!("error: {}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                };

            let options = NotifyOptions {
                task_id: Some(task_id),
                execution_id: Some(execution_id),
                status,
                state_dir: notify_state_dir,
            };

            match execute_notify(options) {
                Ok(res) => {
                    let verb = if res.is_idempotent { "already recorded" } else { "recorded" };
                    if conversation_id.is_empty() {
                        println!(
                            "Notification {} (receipt: {}, status: {}).",
                            verb, res.receipt_id, res.state
                        );
                    } else {
                        println!(
                            "Notification {} (receipt: {}, conversation: {}, status: {}).",
                            verb, res.receipt_id, conversation_id, res.state
                        );
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "notify" => {
            if args.len() < 3 {
                eprintln!("Usage: hands-bridge notify done [--task <task_id>] | failed --message <text> [--task <task_id>]");
                std::process::exit(2);
            }
            let sub = args[2].as_str();
            let mut task_id = None;
            let mut execution_id = None;
            let mut state_dir = None;
            let mut message = None;

            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--task" if i + 1 < args.len() => {
                        task_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--execution-id" if i + 1 < args.len() => {
                        execution_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--message" if i + 1 < args.len() => {
                        message = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    other => {
                        eprintln!("error: unknown option '{}'", other);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }

            let status = match sub {
                "done" => {
                    if message.is_some() {
                        eprintln!("error: --message is not supported for 'notify done'");
                        std::process::exit(2);
                    }
                    ExplicitNotificationStatus::Done
                }
                "failed" => {
                    let msg = match message {
                        Some(m) if !m.trim().is_empty() => m,
                        _ => {
                            eprintln!("error: --message is required for 'notify failed'");
                            std::process::exit(2);
                        }
                    };
                    ExplicitNotificationStatus::Failed { message: msg }
                }
                other => {
                    eprintln!("error: unknown notify subcommand '{}'", other);
                    std::process::exit(2);
                }
            };

            let options = NotifyOptions {
                task_id,
                execution_id,
                status,
                state_dir,
            };

            match execute_notify(options) {
                Ok(res) => {
                    if res.is_idempotent {
                        println!(
                            "Notification already recorded (receipt: {}, task: {}, execution: {}, status: {}).",
                            res.receipt_id, res.task_id, res.execution_id, res.state
                        );
                    } else {
                        println!(
                            "Notification recorded (receipt: {}, task: {}, execution: {}, status: {}).",
                            res.receipt_id, res.task_id, res.execution_id, res.state
                        );
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "prepare" => {
            let mut conversation_id: Option<String> = None;
            let mut pairing_id = None;
            let mut state_dir = None;
            let mut as_json = false;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--conversation" if i + 1 < args.len() => {
                        conversation_id = Some(args[i + 1].trim().to_string());
                        i += 1;
                    }
                    "--pairing-id" if i + 1 < args.len() => {
                        pairing_id = Some(args[i + 1].trim().to_string());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    "--json" => as_json = true,
                    other => {
                        eprintln!("error: unknown option '{}'", other);
                        std::process::exit(2);
                    }
                }
                i += 1;
            }

            let conversation_id = match conversation_id {
                Some(c) if !c.is_empty() => c,
                _ => {
                    eprintln!("usage: hands-bridge prepare --conversation <conversation_id> [--pairing-id <id>] [--state-dir <dir>] [--json]");
                    std::process::exit(2);
                }
            };

            let options = PrepareOptions {
                pairing_id,
                conversation_id,
                state_dir,
            };

            match execute_prepare(options) {
                Ok(worker) => {
                    if as_json {
                        println!(
                            "{}",
                            json!({
                                "task_id": worker.task_id,
                                "execution_id": worker.execution_id,
                                "origin_conversation_id": worker.origin_conversation_id,
                                "policy_revision": worker.policy_revision,
                                "state": worker.state,
                                "state_dir": worker.state_dir,
                                "env": worker.env,
                            })
                        );
                    } else {
                        println!(
                            "Prepared worker claim (task: {}, execution: {}, conversation: {}, state: {}).",
                            worker.task_id, worker.execution_id, worker.origin_conversation_id, worker.state
                        );
                        println!("Set this environment on the normal Orca OMP worker before it runs:");
                        for (key, value) in &worker.env {
                            println!("  $env:{}='{}'", key, value);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        "--help" | "-h" | "help" => {
            print_help();
        }
        other => {
            eprintln!("Unknown command: {}", other);
            print_help();
            std::process::exit(1);
        }
    }
}
