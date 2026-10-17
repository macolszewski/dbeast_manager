//! Windows executables carry their icon and version information as resources.

fn main() {
    println!("cargo:rerun-if-changed=assets/branding/DBeast.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/branding/DBeast.ico")
            .set("ProductName", "DBeast")
            .set("FileDescription", "DBeast database client")
            .set("LegalCopyright", "Copyright 2026 Maciej Olszewski. Apache License 2.0.");
        if let Err(error) = resource.compile() {
            println!("cargo:warning=Windows icon not embedded: {error}");
        }
    }
}
