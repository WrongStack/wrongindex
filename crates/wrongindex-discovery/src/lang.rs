//! Language detection, driven by the generated inventory
//! (`contracts/inventory/languages.json`), mirroring `detectLang` in
//! WrongStack's languages.ts.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

const INVENTORY: &str = include_str!("../../../contracts/inventory/languages.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    extensions: HashMap<String, String>,
    special_filenames: HashMap<String, String>,
}

fn inventory() -> &'static Inventory {
    static INV: OnceLock<Inventory> = OnceLock::new();
    INV.get_or_init(|| serde_json::from_str(INVENTORY).expect("valid language inventory"))
}

fn basename(path: &str) -> &str {
    let cut = path.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    &path[cut..]
}

/// `detectLang(file)`.
pub fn detect_lang(path: &str) -> Option<&'static str> {
    let lower = basename(path).to_lowercase();
    if lower.ends_with(".d.ts") || lower.ends_with(".d.mts") || lower.ends_with(".d.cts") {
        return Some("ts");
    }
    let inv = inventory();
    if let Some(l) = inv.special_filenames.get(&lower) {
        return Some(l.as_str());
    }
    let dot = lower.rfind('.')?;
    if dot == 0 {
        return None;
    }
    inv.extensions.get(&lower[dot..]).map(String::as_str)
}

/// Node `path.extname(basename)` lowercased: `''` when the last dot is the
/// first character or absent.
fn extname_lower(path: &str) -> String {
    let base = basename(path);
    match base.rfind('.') {
        Some(i) if i > 0 => base[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// The discovery admission test: indexable extension or a detectable language.
pub fn is_indexable(path: &str) -> bool {
    inventory().extensions.contains_key(&extname_lower(path)) || detect_lang(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_like_wrongstack() {
        assert_eq!(detect_lang("src/a.ts"), Some("ts"));
        assert_eq!(detect_lang("src/types.d.ts"), Some("ts"));
        assert_eq!(detect_lang("C:\\x\\Makefile"), Some("other"));
        assert_eq!(detect_lang("docker-compose.yml"), Some("yaml"));
        assert_eq!(detect_lang("Gemfile"), Some("ruby"));
        assert_eq!(detect_lang(".ts"), None);
        assert_eq!(detect_lang("README"), None);
        assert_eq!(detect_lang("x/.hidden.py"), Some("py"));
        assert!(is_indexable("A.JSON"));
        assert!(!is_indexable("image.png"));
    }
}
