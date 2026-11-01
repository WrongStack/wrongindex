//! `cargo run -p wrongindex-engine --example native_index -- <root> <index-dir>`
fn main() {
    let a: Vec<String> = std::env::args().collect();
    match wrongindex_engine::native::index_fresh(a[1].as_ref(), a[2].as_ref()) {
        Ok(r) => println!("{}", serde_json::to_string(&r).unwrap()),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
