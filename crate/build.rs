fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEV_GIT_REV");

    let rev = std::env::var("DEV_GIT_REV").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    String::from_utf8(out.stdout).ok().map(|s| s.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "dev".to_string())
    });

    println!("cargo:rustc-env=DEV_GIT_REV={rev}");
}
