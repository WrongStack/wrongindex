//! Project structure and the relation pass of a full run, ported from
//! WrongStack's `module-roots.ts` (marker probes, package labels) and
//! `relation-pass.ts` (`resolveProjectRelations` with `full = true`).
//!
//! Marker files (`package.json`, `Cargo.toml`, `go.mod`, `pyproject.toml`,
//! `setup.py`, `pom.xml`, `build.gradle[.kts]`, `*.csproj`) are only ever
//! read as text; nothing is executed.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::paths::{self, js_lower, js_trim, to_portable, u16len};
use crate::resolver::ModuleResolver;

pub const MODULE_RESOLUTION_VERSION: &str = "1";

/// `detectLang` (`languages.ts`).
pub fn detect_lang(file: &str) -> Option<&'static str> {
    wrongindex_discovery::lang::detect_lang(file)
}

/// `languageFamily(lang)`: `LANG_FAMILY[lang] ?? 'other'`.
pub fn language_family(lang: &str) -> &'static str {
    crate::native::LANG_FAMILY
        .iter()
        .find(|(l, _)| !l.is_empty() && *l == lang)
        .map_or("other", |(_, f)| f)
}

/// Family of a file by its detected language, `None` when undetectable.
pub fn file_family(file: &str) -> Option<&'static str> {
    detect_lang(file).map(language_family)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleRoot {
    /// Portable (forward-slash) directory.
    pub dir: String,
    pub kind: &'static str,
    pub name: String,
    pub import_path: Option<String>,
    pub source_roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectStructure {
    pub project_root: String,
    pub roots: Vec<ModuleRoot>,
}

// ─── Marker parsing ─────────────────────────────────────────────────────────

/// `fs.readFile(file, 'utf8')`, `None` on any error (missing, a directory…).
fn read_text(file: &str) -> Option<String> {
    let bytes = std::fs::read(file).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// `JSON.parse(source).name` when it is a non-empty string.
///
/// `serde_json` stands in for `JSON.parse`; they differ only on inputs
/// `JSON.parse` accepts and `serde_json` rejects (lone surrogate escapes,
/// nesting deeper than 128), which then read as "no declared name".
pub fn parse_package_json_name(source: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(source).ok()?;
    match v.get("name") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// First `module <path>` directive of a go.mod, ignoring `//` comments.
pub fn parse_go_module_path(source: &str) -> Option<String> {
    for raw in paths::split_lines_crlf(source) {
        let line = js_trim(paths::strip_line_comment(raw, "//"));
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        let ws = rest.len() - rest.trim_start_matches(paths::is_js_space).len();
        if ws == 0 {
            continue;
        }
        let rest = &rest[ws..];
        let end = rest.find(paths::is_js_space).unwrap_or(rest.len());
        let cap = &rest[..end];
        if cap.is_empty() {
            continue;
        }
        // `.replace(/^["']|["']$/g, '')`
        let quote = |c: char| c == '"' || c == '\'';
        let mut s = cap;
        let had_lead = s.starts_with(quote);
        if had_lead {
            s = &s[1..];
        }
        if !s.is_empty() && s.ends_with(quote) {
            s = &s[..s.len() - 1];
        }
        return Some(s.to_string());
    }
    None
}

/// `name = "…"` inside one of `tables` (a line scanner, not a TOML parser).
pub fn parse_toml_table_name(source: &str, tables: &[&str]) -> Option<String> {
    let mut current = String::new();
    for raw in paths::split_lines_crlf(source) {
        let line = js_trim(paths::strip_line_comment(raw, "#"));
        if line.starts_with("[[") {
            current = "\u{0}".into();
            continue;
        }
        if line.len() >= 3 && line.starts_with('[') && line.ends_with(']') {
            let inner = &line[1..line.len() - 1];
            if !inner.contains(']') {
                current = js_trim(inner).to_string();
                continue;
            }
        }
        if !tables.contains(&current.as_str()) {
            continue;
        }
        // `/^name\s*=\s*["']([^"']+)["']/`
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let rest = rest.trim_start_matches(paths::is_js_space);
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start_matches(paths::is_js_space);
        let quote = |c: char| c == '"' || c == '\'';
        let Some(rest) = rest.strip_prefix(quote) else {
            continue;
        };
        let end = rest.find(quote);
        if let Some(end) = end
            && end > 0
        {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// `<artifactId>` of the project itself, `<parent>` blocks removed first.
pub fn parse_pom_artifact_id(source: &str) -> Option<String> {
    // `source.replace(/<parent>[\s\S]*?<\/parent>/g, '')`
    let mut without = String::with_capacity(source.len());
    let mut at = 0;
    while let Some(i) = source[at..].find("<parent>").map(|i| i + at) {
        let body = i + "<parent>".len();
        match source[body..].find("</parent>") {
            Some(j) => {
                without.push_str(&source[at..i]);
                at = body + j + "</parent>".len();
            }
            None => break,
        }
    }
    without.push_str(&source[at..]);
    // `/<artifactId>\s*([^<\s]+)\s*<\/artifactId>/`
    let s = without.as_str();
    let mut from = 0;
    while let Some(i) = s[from..].find("<artifactId>").map(|i| i + from) {
        from = i + 1;
        let rest = &s[i + "<artifactId>".len()..];
        let rest = rest.trim_start_matches(paths::is_js_space);
        let end = rest
            .find(|c: char| c == '<' || paths::is_js_space(c))
            .unwrap_or(rest.len());
        if end == 0 {
            continue;
        }
        let after = rest[end..].trim_start_matches(paths::is_js_space);
        if after.starts_with("</artifactId>") {
            return Some(rest[..end].to_string());
        }
    }
    None
}

// ─── Root detection ─────────────────────────────────────────────────────────

const LANGS_BY_KIND: &[(&str, &[&str])] = &[
    ("npm", &["ts", "tsx", "js", "jsx", "vue", "svelte"]),
    ("cargo", &["rs"]),
    ("go", &["go"]),
    ("python", &["py"]),
    ("maven", &["java", "kotlin", "scala"]),
    ("gradle", &["java", "kotlin", "scala"]),
    ("dotnet", &["csharp"]),
];

fn kind_langs(kind: &str) -> &'static [&'static str] {
    LANGS_BY_KIND
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(&[], |(_, l)| l)
}

type Built = (String, Option<String>, Vec<String>);

fn gradle(dir: &str) -> Option<Built> {
    Some((
        format!("gradle:{}", paths::basename(dir)),
        None,
        vec![
            paths::join(&[dir, "src/main/java"]),
            paths::join(&[dir, "src/main/kotlin"]),
            paths::join(&[dir, "src/main/scala"]),
        ],
    ))
}

/// `MARKER_PROBES` in order: (kind, marker file, build).
type Probe = (&'static str, &'static str, fn(&str, &str) -> Option<Built>);
const MARKER_PROBES: &[Probe] = &[
    ("npm", "package.json", |dir, src| {
        let declared = parse_package_json_name(src);
        Some((
            declared.clone().unwrap_or_else(|| paths::basename(dir)),
            declared,
            vec![dir.to_string()],
        ))
    }),
    ("cargo", "Cargo.toml", |dir, src| {
        let name = parse_toml_table_name(src, &["package"])?;
        Some((
            format!("crate:{name}"),
            Some(name.replace('-', "_")),
            vec![paths::join(&[dir, "src"])],
        ))
    }),
    ("go", "go.mod", |dir, src| {
        let mp = parse_go_module_path(src).filter(|m| !m.is_empty())?;
        Some((format!("go:{mp}"), Some(mp), vec![dir.to_string()]))
    }),
    ("python", "pyproject.toml", |dir, src| {
        let name = parse_toml_table_name(src, &["project", "tool.poetry"])
            .unwrap_or_else(|| paths::basename(dir));
        Some((
            format!("py:{name}"),
            None,
            vec![paths::join(&[dir, "src"]), dir.to_string()],
        ))
    }),
    ("python", "setup.py", |dir, _| {
        Some((
            format!("py:{}", paths::basename(dir)),
            None,
            vec![paths::join(&[dir, "src"]), dir.to_string()],
        ))
    }),
    ("maven", "pom.xml", |dir, src| {
        let id = parse_pom_artifact_id(src).unwrap_or_else(|| paths::basename(dir));
        Some((
            format!("mvn:{id}"),
            None,
            vec![
                paths::join(&[dir, "src/main/java"]),
                paths::join(&[dir, "src/main/kotlin"]),
                paths::join(&[dir, "src/main/scala"]),
                paths::join(&[dir, "src/test/java"]),
            ],
        ))
    }),
    ("gradle", "build.gradle", |dir, _| gradle(dir)),
    ("gradle", "build.gradle.kts", |dir, _| gradle(dir)),
];

/// `*.csproj` in `dir` (first directory entry, in listing order).
fn probe_dotnet_root(dir: &str) -> Option<ModuleRoot> {
    let entries = std::fs::read_dir(dir).ok()?;
    let project = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| js_lower(n).ends_with(".csproj"))?;
    let name = &project[..project.len() - ".csproj".len()];
    Some(ModuleRoot {
        dir: dir.to_string(),
        kind: "dotnet",
        name: format!("csproj:{name}"),
        import_path: None,
        source_roots: vec![dir.to_string()],
    })
}

/// Ancestor directories of `dir` up to and including `stop`.
fn ancestors_of(dir: &str, stop: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = dir.to_string();
    loop {
        out.push(cur.clone());
        if cur == stop || u16len(&cur) <= u16len(stop) {
            break;
        }
        let parent = paths::dirname(&cur);
        if parent == cur {
            break;
        }
        cur = parent;
    }
    out
}

/// `detectModuleRoots(projectRoot, files)`.
pub fn detect_module_roots(project_root: &str, files: &[String]) -> ProjectStructure {
    let root = to_portable(project_root).trim_end_matches('/').to_string();
    let mut langs_by_dir: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    for f in files {
        let portable = to_portable(f);
        let Some(lang) = detect_lang(&portable) else {
            continue;
        };
        langs_by_dir
            .entry(paths::dirname(&portable))
            .or_default()
            .insert(lang);
    }
    let mut candidates: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    for (dir, langs) in &langs_by_dir {
        for a in ancestors_of(dir, &root) {
            candidates
                .entry(a)
                .or_default()
                .extend(langs.iter().copied());
        }
    }
    let mut roots = Vec::new();
    for (dir, langs) in &candidates {
        for (kind, marker, build) in MARKER_PROBES {
            if !kind_langs(kind).iter().any(|l| langs.contains(l)) {
                continue;
            }
            let Some(src) = read_text(&paths::join(&[dir, marker])) else {
                continue;
            };
            if let Some((name, import_path, source_roots)) = build(dir, &src) {
                roots.push(ModuleRoot {
                    dir: dir.clone(),
                    kind,
                    name,
                    import_path,
                    source_roots,
                });
            }
        }
        if kind_langs("dotnet").iter().any(|l| langs.contains(l))
            && let Some(r) = probe_dotnet_root(dir)
        {
            roots.push(r);
        }
    }
    // Deepest first, then `localeCompare`; stable, so one directory's roots
    // keep probe order.
    roots.sort_by(|a, b| {
        u16len(&b.dir)
            .cmp(&u16len(&a.dir))
            .then_with(|| paths::locale_compare(&a.dir, &b.dir))
    });
    ProjectStructure {
        project_root: root,
        roots,
    }
}

/// `JSON.stringify` of a string.
fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `JSON.stringify(structure.roots)` (keys in object order, `undefined`
/// dropped).
pub fn structure_json(roots: &[ModuleRoot]) -> String {
    let mut out = String::from("[");
    for (i, r) in roots.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"dir\":");
        json_string(&r.dir, &mut out);
        out.push_str(",\"kind\":");
        json_string(r.kind, &mut out);
        out.push_str(",\"name\":");
        json_string(&r.name, &mut out);
        if let Some(ip) = &r.import_path {
            out.push_str(",\"importPath\":");
            json_string(ip, &mut out);
        }
        out.push_str(",\"sourceRoots\":[");
        for (j, s) in r.source_roots.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            json_string(s, &mut out);
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

pub fn hex_sha256(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// `structureKey(structure)`.
pub fn structure_key(s: &ProjectStructure) -> String {
    hex_sha256(structure_json(&s.roots).as_bytes())
}

/// `findOwningRoot(structure, file, kinds?)`.
pub fn find_owning_root<'a>(
    s: &'a ProjectStructure,
    file: &str,
    kinds: Option<&[&str]>,
) -> Option<&'a ModuleRoot> {
    let portable = to_portable(file);
    s.roots.iter().find(|r| {
        kinds.is_none_or(|k| k.contains(&r.kind))
            && (portable == r.dir
                || (portable.starts_with(&r.dir) && portable[r.dir.len()..].starts_with('/')))
    })
}

/// `derivePackageFromLayout(file)`.
pub fn derive_package_from_layout(file: &str) -> Option<String> {
    let portable = to_portable(file);
    for (marker, prefix) in [("/packages/", "@wrongstack/"), ("/apps/", "app:")] {
        if let Some(i) = portable.find(marker) {
            let seg = portable[i + marker.len()..].split('/').next().unwrap_or("");
            if !seg.is_empty() {
                return Some(format!("{prefix}{seg}"));
            }
        }
    }
    None
}

fn python_package_label(
    s: &ProjectStructure,
    file: &str,
    init_dirs: &BTreeSet<String>,
) -> Option<String> {
    let dir = paths::dirname(&to_portable(file));
    if !init_dirs.contains(&dir) {
        return None;
    }
    let mut segs: Vec<String> = Vec::new();
    let mut cur = dir;
    while init_dirs.contains(&cur) && u16len(&cur) > u16len(&s.project_root) {
        segs.insert(0, paths::basename(&cur));
        cur = paths::dirname(&cur);
    }
    (!segs.is_empty()).then(|| format!("py:{}", segs.join(".")))
}

/// `assignPackageLabels(structure, files)` for every file, in `files` order.
pub fn assign_package_labels(s: &ProjectStructure, files: &[String]) -> Vec<(String, String)> {
    let init_dirs: BTreeSet<String> = files
        .iter()
        .map(|f| to_portable(f))
        .filter(|p| paths::basename(p) == "__init__.py")
        .map(|p| paths::dirname(&p))
        .collect();
    let mut out: Vec<(String, String)> = Vec::with_capacity(files.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut set = |f: &String, label: String| match index.get(f) {
        Some(&i) => out[i].1 = label,
        None => {
            index.insert(f.clone(), out.len());
            out.push((f.clone(), label));
        }
    };
    for f in files {
        let portable = to_portable(f);
        let lang = detect_lang(&portable);
        if lang == Some("go") {
            let dir = paths::dirname(&portable);
            let label = match find_owning_root(s, &portable, Some(&["go"])) {
                Some(owner) if owner.import_path.as_deref().is_some_and(|p| !p.is_empty()) => {
                    let ip = owner.import_path.as_deref().unwrap_or_default();
                    let rel = paths::relative(&owner.dir, &dir);
                    if rel.is_empty() {
                        ip.to_string()
                    } else {
                        format!("{ip}/{rel}")
                    }
                }
                _ => {
                    let rel = paths::relative(&s.project_root, &dir);
                    format!("go:{}", if rel.is_empty() { "." } else { &rel })
                }
            };
            set(f, label);
            continue;
        }
        if lang == Some("py")
            && let Some(dotted) = python_package_label(s, &portable, &init_dirs)
        {
            set(f, dotted);
            continue;
        }
        let label = find_owning_root(s, &portable, None)
            .map(|r| r.name.clone())
            .filter(|n| !n.is_empty())
            .or_else(|| derive_package_from_layout(&portable))
            .unwrap_or_else(|| "(root)".into());
        set(f, label);
    }
    out
}

// ─── relation-pass.ts (full run) ────────────────────────────────────────────

/// What the relation pass decided, for tests and reports.
#[derive(Debug, Clone, Default)]
pub struct RelationReport {
    pub structure: Option<ProjectStructure>,
    pub resolutions: Vec<(String, String, String, Option<String>)>,
}

fn set_meta(tx: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    crate::native::set_meta(tx, key, value)
}

/// `resolveProjectRelations(store, projectRoot, { full: true, projectScan: true })`
/// on a store with no previous relation state.
pub fn relation_pass(tx: &Connection, project_root: &str) -> rusqlite::Result<RelationReport> {
    let mut report = RelationReport::default();
    let files: Vec<String> = tx
        .prepare("SELECT file, lang, mtime_ms, symbol_count, last_indexed, content_hash, git_blob FROM files")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if files.is_empty() {
        return Ok(report);
    }
    let structure = detect_module_roots(project_root, &files);
    let key = structure_key(&structure);
    let stored_key: Option<String> = crate::native::get_meta(tx, "relation_structure")?;
    let labels = assign_package_labels(&structure, &files);
    let stored: HashMap<String, String> = tx
        .prepare("SELECT file, package FROM files WHERE package != ''")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    {
        let mut up = tx.prepare("UPDATE files SET package = ?1 WHERE file = ?2")?;
        for (file, label) in &labels {
            if stored.get(file) != Some(label) {
                up.execute([label, file])?;
            }
        }
    }
    if stored_key.as_deref() != Some(key.as_str()) {
        set_meta(tx, "relation_structure", &key)?;
    }
    set_meta(tx, "relation_epoch", &uuid::Uuid::new_v4().to_string())?;
    let pending: Vec<(String, String, String)> = tx
        .prepare(
            "SELECT DISTINCT s.file AS fromFile, r.lang AS lang, r.module AS module
                  FROM refs r
                  JOIN symbols s ON s.id = r.from_id
                 WHERE r.call_type = 'import' AND r.module IS NOT NULL",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    if !pending.is_empty() {
        let namespaces: Vec<(String, String)> = tx
            .prepare("SELECT name, file FROM symbols WHERE kind = 'namespace' ORDER BY file, id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let resolver = ModuleResolver::new(&structure, &files, &namespaces);
        // `applyImportResolutions`: one row per (importer, lang, specifier),
        // last wins; the per-entry UPDATE has the same effect as the bulk
        // temp-table form WrongStack switches to above 2000 entries.
        let mut uniq: Vec<(String, String, String, Option<String>)> = Vec::new();
        let mut at: HashMap<(String, String, String), usize> = HashMap::new();
        for (from, lang, module) in pending {
            let to = resolver
                .resolve(&from, &lang, &module)
                .filter(|t| *t != from);
            let k = (from.clone(), lang.clone(), module.clone());
            match at.get(&k) {
                Some(&i) => uniq[i].3 = to,
                None => {
                    at.insert(k, uniq.len());
                    uniq.push((from, lang, module, to));
                }
            }
        }
        let mut up = tx.prepare(
            "UPDATE refs SET to_file = ?
          WHERE from_id IN (SELECT id FROM symbols WHERE file = ?)
            AND +module = ? AND +lang = ? AND +call_type = 'import'",
        )?;
        for (from, lang, module, to) in &uniq {
            up.execute(rusqlite::params![to, from, module, lang])?;
        }
        report.resolutions = uniq;
    }
    set_meta(tx, "module_resolution_version", MODULE_RESOLUTION_VERSION)?;
    report.structure = Some(structure);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_scanners_match_the_typescript_regexes() {
        let toml = "[workspace]\nname = \"ws\"\n[[bin]]\nname = 'b'\n[package] # c\nname = \"my-crate\" # x\n";
        assert_eq!(
            parse_toml_table_name(toml, &["package"]).as_deref(),
            Some("my-crate")
        );
        assert_eq!(
            parse_toml_table_name("[workspace]\nmembers=[]\n", &["package"]),
            None
        );
        assert_eq!(
            parse_toml_table_name("[tool.poetry]\nname='p'\n", &["project", "tool.poetry"])
                .as_deref(),
            Some("p")
        );
        assert_eq!(
            parse_go_module_path("// c\nmodule \"example.com/m\" // x\n").as_deref(),
            Some("example.com/m")
        );
        assert_eq!(parse_go_module_path("go 1.22\n"), None);
        assert_eq!(
            parse_pom_artifact_id(
                "<parent><artifactId>p</artifactId></parent><artifactId> app </artifactId>"
            )
            .as_deref(),
            Some("app")
        );
        assert_eq!(
            parse_package_json_name("{\"name\": \"@a/b\"}").as_deref(),
            Some("@a/b")
        );
        assert_eq!(parse_package_json_name("{\"name\": \"\"}"), None);
        assert_eq!(parse_package_json_name("{"), None);
    }

    #[test]
    fn structure_json_drops_undefined_import_paths() {
        let roots = vec![
            ModuleRoot {
                dir: "C:/r/a".into(),
                kind: "cargo",
                name: "crate:a".into(),
                import_path: Some("a".into()),
                source_roots: vec!["C:/r/a/src".into()],
            },
            ModuleRoot {
                dir: "C:/r".into(),
                kind: "python",
                name: "py:\"q\"".into(),
                import_path: None,
                source_roots: vec!["C:/r/src".into(), "C:/r".into()],
            },
        ];
        assert_eq!(
            structure_json(&roots),
            r#"[{"dir":"C:/r/a","kind":"cargo","name":"crate:a","importPath":"a","sourceRoots":["C:/r/a/src"]},{"dir":"C:/r","kind":"python","name":"py:\"q\"","sourceRoots":["C:/r/src","C:/r"]}]"#
        );
        assert_eq!(
            hex_sha256(b"[]"),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }

    #[test]
    fn labels_follow_owner_layout_and_go_rules() {
        let s = ProjectStructure {
            project_root: "C:/r".into(),
            roots: vec![
                ModuleRoot {
                    dir: "C:/r/svc".into(),
                    kind: "go",
                    name: "go:example.com/svc".into(),
                    import_path: Some("example.com/svc".into()),
                    source_roots: vec!["C:/r/svc".into()],
                },
                ModuleRoot {
                    dir: "C:/r/crates/a".into(),
                    kind: "cargo",
                    name: "crate:a".into(),
                    import_path: Some("a".into()),
                    source_roots: vec!["C:/r/crates/a/src".into()],
                },
            ],
        };
        let files: Vec<String> = [
            r"C:\r\svc\x\y.go",
            r"C:\r\svc\m.go",
            r"C:\r\tools\t.go",
            r"C:\r\crates\a\src\lib.rs",
            r"C:\r\crates\ab\src\lib.rs",
            r"C:\r\packages\core\x.yaml",
            r"C:\r\pkg\__init__.py",
            r"C:\r\pkg\sub\__init__.py",
            r"C:\r\pkg\sub\m.py",
            r"C:\r\a.json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let labels: Vec<String> = assign_package_labels(&s, &files)
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert_eq!(
            labels,
            vec![
                "example.com/svc/x",
                "example.com/svc",
                "go:tools",
                "crate:a",
                "(root)",
                "@wrongstack/core",
                "py:pkg",
                "py:pkg.sub",
                "py:pkg.sub",
                "(root)",
            ]
        );
    }
}
