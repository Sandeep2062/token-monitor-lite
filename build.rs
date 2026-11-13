#[cfg(feature = "desktop")]
fn main() {
    println!("cargo:rerun-if-changed=ui/app.slint");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles),
    )
    .expect("Ledger Rail UI must compile");
}

#[cfg(not(feature = "desktop"))]
fn main() {}
