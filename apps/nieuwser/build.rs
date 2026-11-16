fn main() {
    println!("cargo:rerun-if-changed=../../assets/app-icon/nieuwser.ico");
    let config = slint_build::CompilerConfiguration::new().with_edition((1, 19));
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/app-icon/nieuwser.ico")
            .set("ProductName", "Nieuwser")
            .set(
                "FileDescription",
                "Offline-first desktop client for FreshRSS",
            )
            .set("OriginalFilename", "nieuwser.exe")
            .set("InternalName", "nieuwser")
            .set("ProductVersion", env!("CARGO_PKG_VERSION"))
            .set("FileVersion", env!("CARGO_PKG_VERSION"));
        if let Err(e) = res.compile() {
            println!("cargo:warning=nieuwser.exe built without icon/version resources: {e}");
        }
    }
}
