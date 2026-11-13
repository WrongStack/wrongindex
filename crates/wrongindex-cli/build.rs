#[path = "../windows-resource.rs"]
mod windows_resource;

fn main() {
    windows_resource::compile("wrongindex.exe");
}
