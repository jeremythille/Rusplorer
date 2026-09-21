fn main() {
    embed_resource::compile("logo/app.rc", embed_resource::NONE);

    // Bake the local date/time of this build into the binary.
    // No rerun-if-changed directives → Cargo re-runs this on every build;
    // it only triggers a recompile of the crate when the minute actually changes.
    let timestamp = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-Date -Format 'yyyy-MM-dd HH:mm'"])
        .output()
        .ok()
        .and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", timestamp);
}

