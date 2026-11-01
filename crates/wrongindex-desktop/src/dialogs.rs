//! Native Windows file dialogs (WI-402 folder picker, diagnostics save
//! location). They run in the Rust process on a dedicated single-threaded
//! COM apartment; the WebView only receives the chosen path, never general
//! filesystem access.

use std::path::PathBuf;

/// What to ask the user for.
pub enum Pick {
    /// An existing folder (add project).
    Folder,
    /// A new file to save, with a suggested name (diagnostics bundle).
    SaveJson { suggested: String },
}

/// Show the dialog modally over `owner` (a raw `HWND`, or 0). `Ok(None)`
/// when the user cancels.
#[cfg(windows)]
pub fn pick(owner: isize, what: Pick) -> Result<Option<PathBuf>, String> {
    // COM dialogs need a single-threaded apartment; Tauri's command threads
    // are not guaranteed to be one.
    std::thread::spawn(move || imp::pick(owner, what))
        .join()
        .map_err(|_| "file dialog thread panicked".to_string())?
}

#[cfg(not(windows))]
pub fn pick(_owner: isize, _what: Pick) -> Result<Option<PathBuf>, String> {
    Err("native dialogs are only implemented on Windows".into())
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
        IFileDialog, IFileOpenDialog, IFileSaveDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::{HRESULT, HSTRING, PCWSTR, w};

    use super::Pick;

    pub fn pick(owner: isize, what: Pick) -> Result<Option<PathBuf>, String> {
        // SAFETY: balanced with `CoUninitialize` below on this thread only.
        let init =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if init.is_err() {
            return Err(format!("COM initialization failed: {init:?}"));
        }
        let result = show(owner, what);
        // SAFETY: matches the successful `CoInitializeEx` above.
        unsafe { CoUninitialize() };
        result
    }

    fn show(owner: isize, what: Pick) -> Result<Option<PathBuf>, String> {
        let err = |e: windows::core::Error| e.message().to_string();
        // SAFETY: plain COM calls on interfaces created here; strings outlive
        // the calls that borrow them.
        unsafe {
            let dialog: IFileDialog = match &what {
                Pick::Folder => {
                    let d: IFileOpenDialog =
                        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                            .map_err(err)?;
                    let opts = d.GetOptions().map_err(err)?;
                    d.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                        .map_err(err)?;
                    d.SetTitle(w!("Choose a project folder")).map_err(err)?;
                    windows::core::Interface::cast(&d).map_err(err)?
                }
                Pick::SaveJson { suggested } => {
                    let d: IFileSaveDialog =
                        CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)
                            .map_err(err)?;
                    let opts = d.GetOptions().map_err(err)?;
                    d.SetOptions(opts | FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM)
                        .map_err(err)?;
                    d.SetTitle(w!("Save diagnostics bundle")).map_err(err)?;
                    let types = [COMDLG_FILTERSPEC {
                        pszName: w!("JSON bundle"),
                        pszSpec: w!("*.json"),
                    }];
                    d.SetFileTypes(&types).map_err(err)?;
                    d.SetDefaultExtension(w!("json")).map_err(err)?;
                    let name = HSTRING::from(suggested.as_str());
                    d.SetFileName(PCWSTR(name.as_ptr())).map_err(err)?;
                    windows::core::Interface::cast(&d).map_err(err)?
                }
            };
            let hwnd = (owner != 0).then_some(HWND(owner as *mut core::ffi::c_void));
            if let Err(e) = dialog.Show(hwnd) {
                if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                    return Ok(None);
                }
                return Err(err(e));
            }
            let item = dialog.GetResult().map_err(err)?;
            let raw = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(err)?;
            let path = raw.to_string().map_err(|e| e.to_string());
            CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
            Ok(Some(PathBuf::from(path?)))
        }
    }
}
