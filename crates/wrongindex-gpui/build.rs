#[path = "../windows-resource.rs"]
mod windows_resource;

fn main() {
    windows_resource::compile_with_icon("wrongindex-gpui.exe", Some("icons/icon.ico"));
}
