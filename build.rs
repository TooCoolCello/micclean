// Puts assets/micclean.ico into the exes, so Explorer and the Start menu show the mic icon.
// Without the file the build still works, just with the generic icon (a test checks the file).
fn main() {
    let ico = "assets/micclean.ico";
    println!("cargo:rerun-if-changed={ico}");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") && std::path::Path::new(ico).exists() {
        winresource::WindowsResource::new()
            .set_icon(ico)
            .set("FileDescription", "MicClean")
            .compile()
            .expect("embedding the icon failed");
    }
}
