//! `bindRefsByImports('all')` (`ref-binding-pass.ts`): import-aware binding
//! of refs to the symbol their file actually imports, run once over the whole
//! index after module resolution.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::relations::{detect_lang, file_family, language_family};

pub const REF_BINDING_VERSION: &str = "2";
const MAX_REEXPORT_HOPS: usize = 4;
const NON_BINDABLE: &[&str] = &["data", "web", "other"];

fn is_bindable_file(f: &str) -> bool {
    file_family(f).is_some_and(|fam| !NON_BINDABLE.contains(&fam))
}

fn is_js_file(f: &str) -> bool {
    file_family(f) == Some("js")
}

struct RefRow {
    id: i64,
    file: String,
    to_name: String,
    to_id: Option<i64>,
    call_type: String,
    module: Option<String>,
    to_file: Option<String>,
}

/// `(toName, module, toFile)`.
type Import = (String, Option<String>, Option<String>);

#[derive(Default)]
struct Facts {
    declared: HashMap<String, i64>,
    imports: Vec<Import>,
}

struct Forward {
    by_name: HashMap<String, Vec<String>>,
    wildcard: Vec<String>,
}

enum Binding {
    File(String),
    External,
}

/// Result of a binding run (updates written).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BindingResult {
    pub updated: usize,
}

/// `toId`/`toFile` decision for one non-import ref: `None` means "no
/// evidence"; `Some((None, ""))` an external binding.
type Decision = Option<(Option<i64>, Option<String>)>;

pub fn bind_refs_all(tx: &Connection) -> rusqlite::Result<BindingResult> {
    let refs: Vec<RefRow> = tx
        .prepare(
            "SELECT r.id, s.file, r.to_name, r.to_id, r.call_type, r.module, r.to_file
       FROM refs r JOIN symbols s ON s.id = r.from_id ORDER BY r.id",
        )?
        .query_map([], |r| {
            Ok(RefRow {
                id: r.get(0)?,
                file: r.get(1)?,
                to_name: r.get(2)?,
                to_id: r.get(3)?,
                call_type: r.get(4)?,
                module: r.get(5)?,
                to_file: r.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| is_bindable_file(&r.file))
        .collect();
    if refs.is_empty() {
        return Ok(BindingResult::default());
    }
    let mut facts: HashMap<String, Facts> = HashMap::new();
    let mut file_of_symbol: HashMap<i64, String> = HashMap::new();
    {
        let mut st = tx.prepare(
            "SELECT file, name,
    COALESCE(
      MIN(CASE WHEN kind NOT IN ('method', 'property', 'field', 'ctor', 'namespace') THEN id END),
      MIN(id)
    )
    FROM symbols
   WHERE kind != 'mod'
     AND (scope = '' OR lang NOT IN ('ts', 'tsx', 'js', 'jsx', 'vue', 'svelte')) GROUP BY file, name",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let file: String = r.get(0)?;
            let name: String = r.get(1)?;
            let Some(id) = r.get::<_, Option<i64>>(2)? else {
                continue;
            };
            facts
                .entry(file.clone())
                .or_default()
                .declared
                .insert(name, id);
            file_of_symbol.insert(id, file);
        }
    }
    {
        let mut st = tx.prepare(
            "SELECT s.file, r.to_name, r.module, r.to_file
         FROM refs r JOIN symbols s ON s.id = r.from_id
        WHERE +r.call_type = 'import' ORDER BY r.id",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let file: String = r.get(0)?;
            facts
                .entry(file)
                .or_default()
                .imports
                .push((r.get(1)?, r.get(2)?, r.get(3)?));
        }
    }

    let mut forwards: HashMap<String, Forward> = HashMap::new();

    // `findExport` with memo.
    let mut memo: HashMap<(String, String), Option<i64>> = HashMap::new();
    let mut exported_id = |file: &str, name: &str| -> Option<i64> {
        let key = (file.to_string(), name.to_string());
        if let Some(m) = memo.get(&key) {
            return *m;
        }
        let found = find_export(&facts, &mut forwards, file, name);
        memo.insert(key, found);
        found
    };

    let mut bindings_of: HashMap<String, HashMap<String, Binding>> = HashMap::new();
    let mut updates: Vec<(Option<i64>, Option<String>, i64)> = Vec::new();
    let mut need_fallback: Vec<&RefRow> = Vec::new();
    for r in &refs {
        let js = is_js_file(&r.file);
        if r.call_type == "import" {
            if r.module.is_none() || r.module.as_deref() == Some(r.to_name.as_str()) {
                continue;
            }
            let has_file = r.to_file.as_deref().is_some_and(|t| !t.is_empty());
            if js {
                let to_id = if has_file {
                    exported_id(r.to_file.as_deref().unwrap_or_default(), &r.to_name)
                } else {
                    None
                };
                if to_id != r.to_id {
                    updates.push((to_id, r.to_file.clone(), r.id));
                }
            } else if has_file
                && let Some(id) = exported_id(r.to_file.as_deref().unwrap_or_default(), &r.to_name)
                && Some(id) != r.to_id
            {
                updates.push((Some(id), r.to_file.clone(), r.id));
            }
            continue;
        }
        let local = facts
            .get(&r.file)
            .and_then(|f| f.declared.get(&r.to_name))
            .copied();
        let mut decision: Decision = None;
        if let Some(local) = local {
            decision = Some((Some(local), Some(r.file.clone())));
        } else {
            if !bindings_of.contains_key(&r.file) {
                let m = build_bindings(&r.file, facts.get(&r.file));
                bindings_of.insert(r.file.clone(), m);
            }
            match bindings_of.get(&r.file).and_then(|m| m.get(&r.to_name)) {
                Some(Binding::External) => decision = Some((None, Some(String::new()))),
                Some(Binding::File(bf)) => {
                    let bf = bf.clone();
                    if let Some(id) = exported_id(&bf, &r.to_name) {
                        let tf = file_of_symbol.get(&id).cloned().unwrap_or(bf);
                        decision = Some((Some(id), Some(tf)));
                    }
                }
                None => {}
            }
            if decision.is_none() && !js {
                let imports: Vec<String> = facts
                    .get(&r.file)
                    .map(|f| {
                        f.imports
                            .iter()
                            .filter_map(|(_, _, t)| t.clone().filter(|t| !t.is_empty()))
                            .collect()
                    })
                    .unwrap_or_default();
                for imp in imports {
                    if let Some(id) = exported_id(&imp, &r.to_name) {
                        let tf = file_of_symbol.get(&id).cloned().unwrap_or(imp);
                        decision = Some((Some(id), Some(tf)));
                        break;
                    }
                }
            }
        }
        let Some((to_id, to_file)) = decision else {
            if r.to_file.is_some() {
                need_fallback.push(r);
            }
            continue;
        };
        if to_id != r.to_id || to_file != r.to_file {
            updates.push((to_id, to_file, r.id));
        }
    }

    // Refs that lost their binding go back to the family-wide name guess.
    if !need_fallback.is_empty() {
        let mut families: Vec<(&'static str, Vec<&RefRow>)> = Vec::new();
        for r in need_fallback {
            let fam = detect_lang(&r.file).map_or("other", language_family);
            match families.iter_mut().find(|(f, _)| *f == fam) {
                Some((_, list)) => list.push(r),
                None => families.push((fam, vec![r])),
            }
        }
        let mut st = tx.prepare(
            "SELECT MIN(sym.id) FROM symbols sym
               JOIN lang_family lf ON lf.lang = sym.lang
              WHERE lf.family = ?1 AND sym.name = ?2",
        )?;
        for (fam, list) in families {
            let mut min_by_name: HashMap<String, Option<i64>> = HashMap::new();
            for r in list {
                let id = match min_by_name.get(&r.to_name) {
                    Some(v) => *v,
                    None => {
                        let v: Option<i64> =
                            st.query_row(rusqlite::params![fam, r.to_name], |x| x.get(0))?;
                        min_by_name.insert(r.to_name.clone(), v);
                        v
                    }
                };
                updates.push((id, None, r.id));
            }
        }
    }

    let mut up = tx.prepare("UPDATE refs SET to_id = ?, to_file = ? WHERE id = ?")?;
    for (to_id, to_file, id) in &updates {
        up.execute(rusqlite::params![to_id, to_file, id])?;
    }
    Ok(BindingResult {
        updated: updates.len(),
    })
}

fn build_bindings(file: &str, facts: Option<&Facts>) -> HashMap<String, Binding> {
    let mut m: HashMap<String, Binding> = HashMap::new();
    let js = is_js_file(file);
    let lang = detect_lang(file);
    let Some(f) = facts else {
        return m;
    };
    for (to_name, module, to_file) in &f.imports {
        let Some(module) = module else { continue };
        if to_name == module {
            continue;
        }
        if let Some(tf) = to_file.as_ref().filter(|t| !t.is_empty()) {
            m.insert(to_name.clone(), Binding::File(tf.clone()));
        } else if !m.contains_key(to_name) {
            let is_py_named = lang == Some("py")
                && !module.starts_with('.')
                && to_name.as_str() != module.split('.').next_back().unwrap_or("");
            if js || is_py_named {
                m.insert(to_name.clone(), Binding::External);
            }
        }
    }
    m
}

fn forward<'a>(forwards: &'a mut HashMap<String, Forward>, file: &str, f: &Facts) -> &'a Forward {
    forwards.entry(file.to_string()).or_insert_with(|| {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut wildcard = Vec::new();
        for (to_name, module, to_file) in &f.imports {
            let Some(tf) = to_file.as_ref().filter(|t| !t.is_empty()) else {
                continue;
            };
            if module.as_deref() == Some(to_name.as_str()) {
                wildcard.push(tf.clone());
            } else {
                by_name.entry(to_name.clone()).or_default().push(tf.clone());
            }
        }
        Forward { by_name, wildcard }
    })
}

/// `findExport(file, name)`: breadth-first over re-exports, at most
/// `MAX_REEXPORT_HOPS` hops.
fn find_export(
    facts: &HashMap<String, Facts>,
    forwards: &mut HashMap<String, Forward>,
    file: &str,
    name: &str,
) -> Option<i64> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = vec![file.to_string()];
    let mut hop = 0;
    while hop <= MAX_REEXPORT_HOPS && !frontier.is_empty() {
        let mut next: Vec<String> = Vec::new();
        for cur in &frontier {
            if !seen.insert(cur.clone()) {
                continue;
            }
            let Some(f) = facts.get(cur) else { continue };
            if let Some(id) = f.declared.get(name) {
                return Some(*id);
            }
            let fw = forward(forwards, cur, f);
            if let Some(named) = fw.by_name.get(name) {
                next.extend(named.iter().cloned());
            }
            next.extend(fw.wildcard.iter().cloned());
        }
        frontier = next;
        hop += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answers from the validated Python reference (`finalize_ref.py`
    /// `bind_refs_all`): JS named / external / wildcard imports and one
    /// re-export hop, Python named-external vs module imports, the non-JS
    /// "any imported file" fallback, Go, local declarations, scoped JS
    /// members and a non-bindable (data) file.
    #[test]
    fn binding_matches_the_reference() {
        let c = Connection::open_in_memory().unwrap();
        for s in crate::native::schema_statements() {
            c.execute_batch(&s).unwrap();
        }
        let (a, b, cc, d) = (r"C:\p\a.ts", r"C:\p\b.ts", r"C:\p\c.ts", r"C:\p\d.ts");
        let (m, n, o) = (r"C:\p\m.py", r"C:\p\n.py", r"C:\p\o.py");
        let (g, h, j) = (r"C:\p\g\x.go", r"C:\p\h\y.go", r"C:\p\conf.json");
        let syms = [
            (1, a, "<module>", "ts", "mod", ""),
            (2, a, "Local", "ts", "function", ""),
            (3, b, "<module>", "ts", "mod", ""),
            (4, cc, "Y", "ts", "function", ""),
            (5, d, "X", "ts", "class", ""),
            (6, d, "X", "ts", "method", "Other"),
            (7, m, "<module>", "py", "mod", ""),
            (8, m, "main", "py", "function", ""),
            (9, n, "helper", "py", "function", ""),
            (10, n, "<module>", "py", "mod", ""),
            (11, o, "other", "py", "function", ""),
            (12, o, "other", "py", "method", "K"),
            (13, g, "Run", "go", "function", ""),
            (14, h, "Util", "go", "function", ""),
            (15, j, "key", "json", "property", ""),
            (16, a, "Y", "ts", "property", "Cls"),
        ];
        for (id, f, name, lang, kind, scope) in syms {
            c.execute(
                "INSERT INTO symbols(id, lang, kind, name, file, line, col, signature, doc_comment, scope, end_line, is_exported, signature_hash) VALUES (?1, ?2, ?3, ?4, ?5, 1, 0, '', '', ?6, 1, 0, '')",
                rusqlite::params![id, lang, kind, name, f, scope],
            )
            .unwrap();
        }
        type R<'a> = (
            i64,
            &'a str,
            Option<i64>,
            &'a str,
            Option<&'a str>,
            Option<&'a str>,
        );
        let refs: [R; 21] = [
            (1, "X", None, "import", Some("./b"), Some(b)),
            (1, "Ext", Some(99), "import", Some("pkg"), None),
            (1, "./c", None, "import", Some("./c"), Some(cc)),
            (1, "Gone", None, "import", Some("./gone"), None),
            (2, "X", None, "call", None, None),
            (2, "Ext", Some(7), "call", None, None),
            (2, "Local", None, "call", None, None),
            (2, "Y", None, "call", None, None),
            (2, "Nope", Some(4), "call", None, None),
            (3, "X", None, "import", Some("./d"), Some(d)),
            (7, "thing", None, "import", Some("pkg"), None),
            (7, "path", None, "import", Some("os.path"), None),
            (7, "helper", None, "import", Some(".n"), Some(n)),
            (8, "thing", Some(5), "call", None, None),
            (8, "helper", None, "call", None, None),
            (8, "other", None, "call", None, None),
            (8, "path", None, "call", None, None),
            (10, "o", None, "import", Some(".o"), Some(o)),
            (13, "y", None, "import", Some("example.com/h"), Some(h)),
            (13, "Util", None, "call", None, None),
            (15, "key", None, "call", None, None),
        ];
        for (from, to_name, to_id, ct, module, to_file) in refs {
            c.execute(
                "INSERT INTO refs(from_id, to_name, to_id, call_type, line, lang, module, to_file) VALUES (?1, ?2, ?3, ?4, 1, '', ?5, ?6)",
                rusqlite::params![from, to_name, to_id, ct, module, to_file],
            )
            .unwrap();
        }
        assert_eq!(bind_refs_all(&c).unwrap().updated, 10);
        let got: Vec<(i64, Option<i64>, Option<String>)> = c
            .prepare("SELECT id, to_id, to_file FROM refs ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let s = |x: &str| Some(x.to_string());
        assert_eq!(
            got,
            vec![
                (1, Some(5), s(b)),
                (2, None, None),
                (3, None, s(cc)),
                (4, None, None),
                (5, Some(5), s(d)),
                (6, None, s("")),
                (7, Some(2), s(a)),
                (8, None, None),
                (9, Some(4), None),
                (10, Some(5), s(d)),
                (11, None, None),
                (12, None, None),
                (13, Some(9), s(n)),
                (14, None, s("")),
                (15, Some(9), s(n)),
                (16, None, None),
                (17, None, None),
                (18, None, s(o)),
                (19, None, s(h)),
                (20, Some(14), s(h)),
                (21, None, None),
            ]
        );
    }
}
