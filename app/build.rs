fn main() {
    slint_build::compile("ui/main.slint").expect("failed to compile slint UI");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../assets/waclient.ico")
            .set("ProductName", "WaClient")
            .set("FileDescription", "WaClient - unofficial WhatsApp desktop client")
            .set("InternalName", "WaClient")
            .set("OriginalFilename", "WaClient.exe")
            .set("ProductVersion", &version)
            .set("FileVersion", &version);
        println!("cargo:rerun-if-changed=../assets/waclient.ico");
        res.compile().expect("failed to embed Windows resources");
    }
}
