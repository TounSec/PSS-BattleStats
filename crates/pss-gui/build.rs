fn main() {
    // Embed the .ico as the executable's own icon (Explorer, shortcuts, taskbar pinning)
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/pss_battlestats_icon.ico");
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed the Windows icon: {err}");
        }
    }

    // Recompile when a locale file changes (rust-i18n embeds them at compile time)
    println!("cargo:rerun-if-changed=locales");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../assets/pss_battlestats_icon.ico");
}
