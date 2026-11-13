//! Product identity shared by the three standalone Windows programs.
//! This adds VERSIONINFO only; GPUI retains its own application manifest.
pub fn compile(filename: &str) {
    println!("cargo:rerun-if-changed=../windows-resource.rs");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        tauri_winres::WindowsResource::new()
            .set_language(0x0409)
            .set("ProductName", "WrongIndex")
            .set("CompanyName", "WrongStack")
            .set("OriginalFilename", filename)
            .set("InternalName", filename.trim_end_matches(".exe"))
            .set("LegalCopyright", "Copyright WrongStack contributors")
            .compile()
            .expect("compile Windows product/version resources");
    }
    #[cfg(not(windows))]
    let _ = filename;
}
