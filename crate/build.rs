fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.hands-source-rev");
    println!("cargo:rerun-if-env-changed=DEV_GIT_REV");

    let rev = std::env::var("DEV_GIT_REV").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let prov_path = std::path::Path::new(&manifest_dir).join(".hands-source-rev");
        if let Ok(content) = std::fs::read_to_string(&prov_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }

        std::process::Command::new("git")
            .args(["-C", &manifest_dir, "rev-parse", "--short", "HEAD"])
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
