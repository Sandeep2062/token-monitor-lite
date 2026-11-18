#[cfg(feature = "desktop")]
fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    println!("cargo:rerun-if-changed=ui/i18n");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
            // Bundled gettext catalogs. `ui/i18n/<locale>/LC_MESSAGES/<crate>.po`
            // is generated from the upstream catalogue by
            // `scripts/generate-settings-i18n.py`; `select_bundled_translation`
            // picks one at runtime.
            .with_bundled_translations("ui/i18n"),
    )
    .expect("Ledger Rail UI must compile");
}

#[cfg(not(feature = "desktop"))]
fn main() {}
