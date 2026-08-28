//! First-run checklist. TTY prompts; no browser. Agents use env + HANDS_NO_UI.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use crate::host;
use crate::secrets;
use crate::service;

pub fn run(dir: &Path) -> Result<(), String> {
    host::migrate_from_legacy();
    let cwd = host::pin_workspace(dir)?;

    let client_ok = service::tunnel_client_bin().is_ok();
    let mut key_ok = secrets::get().is_some();
    let mut id_ok = service::tunnel_id_opt().is_some();
    let tty = io::stdin().is_terminal();

    let client_msg = if client_ok {
        "ok"
    } else if cfg!(windows) {
        "missing — run install.ps1 or place tunnel-client.exe in PATH"
    } else if cfg!(target_os = "macos") {
        "missing — brew install openai/tools/tunnel-client"
    } else {
        "missing — install tunnel-client on PATH"
    };

    eprintln!("Hands setup");
    check("workspace", true, &cwd.display().to_string());
    check("tunnel-client", client_ok, client_msg);
    check("runtime key", key_ok, if key_ok { "saved" } else { "missing" });
    check("tunnel id", id_ok, if id_ok { "saved" } else { "missing" });

    if !client_ok {
        return Err("install tunnel-client first".into());
    }

    if !key_ok && tty {
        eprint!("paste runtime key (hidden): ");
        let _ = io::stderr().flush();
        if let Some(k) = read_secret()? {
            secrets::set(&k)?;
            key_ok = true;
            let target_desc = if cfg!(windows) {
                "Windows Credential Manager"
            } else if cfg!(target_os = "macos") {
                "Keychain"
            } else {
                "credential store"
            };
            eprintln!("saved to {target_desc}");
        }
    }
    if !id_ok && tty {
        eprint!("paste tunnel id (tunnel_…): ");
        let _ = io::stderr().flush();
        if let Some(id) = read_line()? {
            service::set_tunnel_id(&id)?;
            id_ok = true;
        }
    }

    if !key_ok || !id_ok {
        if tty {
            return Err("need both runtime key and tunnel id. or: hands config --open".into());
        }
        return Err(
            "non-interactive: set CONTROL_PLANE_API_KEY and CONTROL_PLANE_TUNNEL_ID".into(),
        );
    }

    service::enable()?;
    if let Some(id) = service::tunnel_id_opt() {
        copy_clip(&id);
        eprintln!();
        eprintln!("tunnel id (copied): {id}");
        eprintln!("ChatGPT → chatgpt.com/plugins → Developer mode → Tunnel → paste → Scan tools");
    }
    Ok(())
}

fn check(name: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "·" };
    eprintln!("  [{mark}] {name:<14} {detail}");
}

fn read_line() -> Result<Option<String>, String> {
    let mut s = String::new();
    io::stdin()
        .lock()
        .read_line(&mut s)
        .map_err(|e| e.to_string())?;
    let t = s.trim().to_string();
    Ok(if t.is_empty() { None } else { Some(t) })
}

#[cfg(windows)]
fn read_secret() -> Result<Option<String>, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = 0xFFFFFFF6;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0u32;
    let have_mode = unsafe { GetConsoleMode(handle, &mut mode) } != 0;
    if have_mode {
        unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
    }
    let mut s = String::new();
    let r = io::stdin().lock().read_line(&mut s);
    if have_mode {
        unsafe { SetConsoleMode(handle, mode) };
    }
    eprintln!();
    r.map_err(|e| e.to_string())?;
    let t = s.trim().to_string();
    Ok(if t.is_empty() { None } else { Some(t) })
}

#[cfg(not(windows))]
fn read_secret() -> Result<Option<String>, String> {
    let _ = Command::new("stty").arg("-echo").status();
    let mut s = String::new();
    let r = io::stdin().lock().read_line(&mut s);
    let _ = Command::new("stty").arg("echo").status();
    eprintln!();
    r.map_err(|e| e.to_string())?;
    let t = s.trim().to_string();
    Ok(if t.is_empty() { None } else { Some(t) })
}

fn copy_clip(text: &str) {
    let try_copy = |bin: &str, args: &[&str]| {
        let mut c = Command::new(bin);
        c.args(args);
        c.stdin(std::process::Stdio::piped());
        if let Ok(mut child) = c.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return true;
        }
        false
    };
    #[cfg(windows)]
    {
        let _ = try_copy("clip.exe", &[]) || try_copy("clip", &[]);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = try_copy("pbcopy", &[]);
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = try_copy("wl-copy", &[]) || try_copy("xclip", &["-selection", "clipboard"]);
    }
}
