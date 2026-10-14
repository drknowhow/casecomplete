fn main() {
    // Embeds the icon and version info into casecomplete.exe (taskbar, Explorer, Add/Remove).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../installer/casecomplete.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        if icon.exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(icon.to_str().unwrap_or_default());
            res.set("ProductName", "CaseComplete");
            res.set("FileDescription", "CaseComplete local typing assistant");
            if let Err(e) = res.compile() {
                println!("cargo:warning=icon resource not embedded: {e}");
            }
        }
    }
}
