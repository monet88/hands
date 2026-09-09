use std::env;
use std::path::PathBuf;

use hands_return_bridge::host::{
    DEFAULT_EXTENSION_ID, SetupOptions, execute_setup, resolve_state_dir, run_native_host,
};
use hands_return_bridge::journal::Journal;
use hands_return_bridge::protocol::TRUST_NOTICE;

fn print_help() {
    eprintln!(
        r#"Hands Return Bridge Companion CLI

Usage:
  hands-return-bridge native-host [--state-dir <dir>]
  hands-return-bridge setup --target <path> [options]
  hands-return-bridge status [--state-dir <dir>]
  hands-return-bridge revoke --pairing-id <id> [--state-dir <dir>]

Setup Options:
  --browser <chrome|edge>       Target browser (default: chrome)
  --profile <profile_id>        Browser profile identifier (default: Default)
  --target <path>               Canonical workspace/worktree target path (required)
  --target-id <id>              Target ID identifier (default: dir name)
  --policy-revision <rev>       Explicit OMP launch policy revision (default: v1)
  --tool-policy <policy>        Tool policy (default: standard)
  --approval-policy <policy>    Approval policy (default: prompt)
  --extension-id <id>           Expected extension ID (default: {DEFAULT_EXTENSION_ID})
  --state-dir <dir>             Override per-user companion state directory
  --skip-registry               Skip Windows Registry NativeMessagingHosts registration
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
        "setup" => {
            let mut browser = "chrome".to_string();
            let mut profile_id = "Default".to_string();
            let mut target_path = None;
            let mut target_id = None;
            let mut policy_revision = "v1".to_string();
            let mut tool_policy = "standard".to_string();
            let mut approval_policy = "prompt".to_string();
            let mut extension_id = DEFAULT_EXTENSION_ID.to_string();
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
                        profile_id = args[i + 1].clone();
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
                        policy_revision = args[i + 1].clone();
                        i += 1;
                    }
                    "--tool-policy" if i + 1 < args.len() => {
                        tool_policy = args[i + 1].clone();
                        i += 1;
                    }
                    "--approval-policy" if i + 1 < args.len() => {
                        approval_policy = args[i + 1].clone();
                        i += 1;
                    }
                    "--extension-id" if i + 1 < args.len() => {
                        extension_id = args[i + 1].clone();
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
                    "--pairing-id" if i + 1 < args.len() => {
                        pairing_id = Some(args[i + 1].clone());
                        i += 1;
                    }
                    "--state-dir" if i + 1 < args.len() => {
                        state_dir = Some(PathBuf::from(&args[i + 1]));
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
            }

            let pid = match pairing_id {
                Some(id) => id,
                None => {
                    eprintln!("Error: --pairing-id <id> is required");
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
