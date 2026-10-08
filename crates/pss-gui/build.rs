fn main() {
    // Recompile when a locale file changes (rust-i18n embeds them at compile time)
    println!("cargo:rerun-if-changed=locales");
    println!("cargo:rerun-if-changed=build.rs");
}
