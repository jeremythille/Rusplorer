fn main() {
    embed_resource::compile("logo/app.rc", embed_resource::NONE);

    // Embed the short git commit date so the window title can show it.
    // Falls back to "unknown" if git is unavailable or the repo has no commits.
    let date = std::process::Command::new("git")
        .args(["log", "-1", "--format=%ci"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
        .map(|b| {
            let s = String::from_utf8_lossy(&b).trim().to_string();
            // "2026-03-03 14:32:11 +0100" → "2026-03-03 14:32"
            s.get(..16).unwrap_or(&s).to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_COMMIT_DATE={}", date);

    // Re-run this script when HEAD or any ref changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");
}

