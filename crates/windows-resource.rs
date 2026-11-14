//! Product identity shared by the three standalone Windows programs.
//! This adds VERSIONINFO (and, for the desktop, its icon as resource 1,
//! which GPUI loads for the window); GPUI retains its own application
//! manifest.
#[allow(dead_code)]
pub fn compile(filename: &str) {
    compile_with_icon(filename, None);
}

pub fn compile_with_icon(filename: &str, icon: Option<&str>) {
    println!("cargo:rerun-if-changed=../windows-resource.rs");
    if let Some(icon) = icon {
        println!("cargo:rerun-if-changed={icon}");
    }
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = tauri_winres::WindowsResource::new();
        res.set_language(0x0409)
            .set("ProductName", "WrongIndex")
            .set("CompanyName", "WrongStack")
            .set("OriginalFilename", filename)
            .set("InternalName", filename.trim_end_matches(".exe"))
            .set("LegalCopyright", "Copyright WrongStack contributors");
        if let Some(icon) = icon {
            res.set_icon(icon);
        }
        res.compile()
            .expect("compile Windows product/version resources");
    }
    #[cfg(not(windows))]
    let _ = (filename, icon);
}
