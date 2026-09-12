use std::env;
use std::path::PathBuf;

use hands_return_bridge::host::{
    LocalInitOptions, SetupOptions, TargetAddOptions, TargetListOptions, TargetRemoveOptions,
    execute_local_init, execute_setup, execute_target_add, execute_target_list,
    execute_target_remove, resolve_state_dir, run_native_host,
};
use hands_return_bridge::journal::Journal;
use hands_return_bridge::protocol::TRUST_NOTICE;

fn print_help() {
    eprintln!(
        r#"Hands Return Bridge Companion CLI
Usage:
  hands-return-bridge native-host [--state-dir <dir>]
  hands-return-bridge local-init --extension-id <id> [--browser <chrome|edge>] [--state-dir <dir>] [--skip-registry]
  hands-return-bridge setup --target <path> --profile <profile_id> --extension-id <id> --policy-revision <rev> --tool-policy <policy> --approval-policy <policy> [options]
  hands-return-bridge status [--state-dir <dir>]
  hands-return-bridge revoke --pairing-id <id> [--state-dir <dir>]
  hands-return-bridge target add --target <path> [--target-id <id>] [--pairing-id <id>] [--state-dir <dir>]
  hands-return-bridge target remove --target-id <id> [--pairing-id <id>] [--state-dir <dir>]
  hands-return-bridge target list [--pairing-id <id>] [--state-dir <dir>]

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
"#
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Chrome/Edge Native Messaging calls the binary with the extension origin as the first arg:
    // e.g., "hands-return-bridge.exe chrome-extension://<id>/"
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
