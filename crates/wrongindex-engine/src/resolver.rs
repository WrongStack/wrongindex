//! `ModuleResolver` (`module-resolver.ts`): import specifier → indexed file,
//! per language family, over the indexed file set and detected module roots.

use std::collections::{HashMap, HashSet};

use crate::paths::{self, js_lower, js_trim, to_portable};
use crate::relations::{
    ModuleRoot, ProjectStructure, detect_lang, find_owning_root, language_family,
};

fn extensions(family: &str) -> &'static [&'static str] {
    match family {
        "js" => &[
            ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte",
        ],
        "py" => &[".py", ".pyi"],
        "rs" => &[".rs"],
        "jvm" => &[".java", ".kt", ".scala"],
        "c" => &[".h", ".hpp", ".hh", ".hxx", ".c", ".cpp", ".cc", ".cxx"],
        "ruby" => &[".rb"],
        "go" => &[".go"],
        _ => &[],
    }
}

fn directory_entries(family: &str) -> &'static [&'static str] {
    match family {
        "js" => &["index"],
        "py" => &["__init__"],
        "rs" => &["mod"],
        "ruby" => &["index"],
        _ => &[],
    }
}

/// `normalizeNamespace`: `::`, `\` and `/` to `.`, outer dots trimmed,
/// lowercased.
pub fn normalize_namespace(v: &str) -> String {
    let s = v.replace("::", ".").replace(['\\', '/'], ".");
    js_lower(s.trim_matches('.'))
}

/// Directory holding a Rust module's child modules.
fn rust_module_dir(file: &str) -> String {
    let base = paths::basename(file);
    if base == "mod.rs" || base == "lib.rs" || base == "main.rs" {
        return paths::dirname(file);
    }
    let n = file.len();
    if n >= 3 && file.is_char_boundary(n - 3) && file[n - 3..].eq_ignore_ascii_case(".rs") {
        file[..n - 3].to_string()
    } else {
        file.to_string()
    }
}

pub struct ModuleResolver<'a> {
    structure: &'a ProjectStructure,
    by_path: HashMap<String, String>,
    by_dir: HashMap<String, Vec<String>>,
    by_ns: HashMap<String, String>,
}

impl<'a> ModuleResolver<'a> {
    pub fn new(
        structure: &'a ProjectStructure,
        files: &[String],
        namespaces: &[(String, String)],
    ) -> Self {
        let mut by_path: HashMap<String, String> = HashMap::new();
        let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
        let mut dirs_by_key: HashMap<String, String> = HashMap::new();
        let mut collided_paths: HashSet<String> = HashSet::new();
        let mut collided_dirs: HashSet<String> = HashSet::new();
        for f in files {
            let portable = to_portable(f);
            let pk = js_lower(&portable);
            if !collided_paths.contains(&pk) {
                match by_path.get(&pk) {
                    Some(prior) if prior != f => {
                        by_path.remove(&pk);
                        collided_paths.insert(pk);
                    }
                    _ => {
                        by_path.insert(pk, f.clone());
                    }
                }
            }
            let d = paths::dirname(&portable);
            let dk = js_lower(&d);
            if collided_dirs.contains(&dk) {
                continue;
            }
            match dirs_by_key.get(&dk) {
                None => {
                    dirs_by_key.insert(dk.clone(), d);
                    by_dir.insert(dk, vec![f.clone()]);
                }
                Some(known) if *known == d => {
                    by_dir.entry(dk).or_default().push(f.clone());
                }
                Some(_) => {
                    dirs_by_key.remove(&dk);
                    by_dir.remove(&dk);
                    collided_dirs.insert(dk);
                }
            }
        }
        let mut by_ns: HashMap<String, String> = HashMap::new();
        for (name, file) in namespaces {
            let Some(lang) = detect_lang(file) else {
                continue;
            };
            let norm = normalize_namespace(name);
            if norm.is_empty() {
                continue;
            }
            by_ns
                .entry(format!("{}:{norm}", language_family(lang)))
                .or_insert_with(|| file.clone());
        }
        Self {
            structure,
            by_path,
            by_dir,
            by_ns,
        }
    }

    /// `resolve(fromFile, lang, specifier)`.
    pub fn resolve(&self, from_file: &str, lang: &str, specifier: &str) -> Option<String> {
        let spec = js_trim(specifier).replace('\\', "/");
        if spec.is_empty() {
            return None;
        }
        let from = to_portable(from_file);
        match language_family(lang) {
            "js" => self.resolve_js(&from, &spec),
            "go" => self.resolve_go(&spec),
            "py" => self.resolve_python(&from, &spec),
            "rs" => self.resolve_rust(&from, &spec),
            "jvm" => self.resolve_jvm(&spec),
            "c" => self.resolve_relative_or_bases(&from, &spec, "c", "include"),
            "ruby" => self.resolve_relative_or_bases(&from, &spec, "ruby", "lib"),
            "dotnet" | "php" | "elixir" | "haskell" => self.resolve_namespace(lang, &spec),
            _ => None,
        }
    }

    fn resolve_namespace(&self, lang: &str, spec: &str) -> Option<String> {
        let fam = language_family(lang);
        let normalized = normalize_namespace(spec);
        if let Some(exact) = self.by_ns.get(&format!("{fam}:{normalized}")) {
            return Some(exact.clone());
        }
        let segs: Vec<&str> = normalized.split('.').filter(|s| !s.is_empty()).collect();
        if segs.len() < 2 {
            return None;
        }
        self.by_ns
            .get(&format!("{fam}:{}", segs[..segs.len() - 1].join(".")))
            .cloned()
    }

    fn lookup(&self, candidate: &str) -> Option<String> {
        self.by_path
            .get(&js_lower(&paths::normalize(candidate)))
            .cloned()
    }

    fn lookup_with_extensions(&self, base: &str, family: &str) -> Option<String> {
        if let Some(d) = self.lookup(base) {
            return Some(d);
        }
        let exts = extensions(family);
        let suffix = paths::extname(base);
        let stem = if !suffix.is_empty() && exts.contains(&suffix.as_str()) {
            &base[..base.len() - suffix.len()]
        } else {
            base
        };
        for ext in exts {
            if let Some(h) = self.lookup(&format!("{stem}{ext}")) {
                return Some(h);
            }
        }
        for entry in directory_entries(family) {
            for ext in exts {
                if let Some(h) = self.lookup(&paths::join(&[base, &format!("{entry}{ext}")])) {
                    return Some(h);
                }
            }
        }
        None
    }

    fn representative_in(&self, dir: &str, family: &str) -> Option<String> {
        let members = self.by_dir.get(&js_lower(&paths::normalize(dir)))?;
        let exts = extensions(family);
        let mut eligible: Vec<&String> = members
            .iter()
            .filter(|f| exts.contains(&js_lower(&paths::extname(&to_portable(f))).as_str()))
            .collect();
        eligible.sort_by(|a, b| paths::locale_compare(&to_portable(a), &to_portable(b)));
        if eligible.is_empty() {
            return None;
        }
        let base = js_lower(&paths::basename(&paths::normalize(dir)));
        let named = eligible.iter().find(|f| {
            let b = paths::basename(&to_portable(f));
            js_lower(b.split('.').next().unwrap_or("")) == base
        });
        Some((*named.unwrap_or(&eligible[0])).clone())
    }

    fn find_root(&self, kind: &str, spec: &str) -> Option<&ModuleRoot> {
        self.structure.roots.iter().find(|r| {
            r.kind == kind
                && r.import_path.as_deref().is_some_and(|ip| {
                    spec == ip || (spec.starts_with(ip) && spec[ip.len()..].starts_with('/'))
                })
        })
    }

    fn subpath<'s>(spec: &'s str, ip: &str) -> &'s str {
        let s = &spec[ip.len()..];
        s.strip_prefix('/').unwrap_or(s)
    }

    fn resolve_js(&self, from: &str, spec: &str) -> Option<String> {
        if spec.starts_with('.') {
            return self.lookup_with_extensions(&paths::join(&[&paths::dirname(from), spec]), "js");
        }
        let owner = self.find_root("npm", spec)?;
        let ip = owner.import_path.as_deref().filter(|p| !p.is_empty())?;
        let sub = Self::subpath(spec, ip);
        if sub.is_empty() {
            return self
                .lookup_with_extensions(&paths::join(&[&owner.dir, "src/index"]), "js")
                .or_else(|| {
                    self.lookup_with_extensions(&paths::join(&[&owner.dir, "index"]), "js")
                });
        }
        self.lookup_with_extensions(&paths::join(&[&owner.dir, sub]), "js")
            .or_else(|| self.lookup_with_extensions(&paths::join(&[&owner.dir, "src", sub]), "js"))
    }

    fn resolve_go(&self, spec: &str) -> Option<String> {
        let owner = self.find_root("go", spec)?;
        let ip = owner.import_path.as_deref().filter(|p| !p.is_empty())?;
        let sub = Self::subpath(spec, ip);
        self.representative_in(&paths::join(&[&owner.dir, sub]), "go")
    }

    fn resolve_python(&self, from: &str, spec: &str) -> Option<String> {
        let dots = spec.len() - spec.trim_start_matches('.').len();
        if dots > 0 {
            let mut base = paths::dirname(from);
            for _ in 1..dots {
                base = paths::dirname(&base);
            }
            let mut parts: Vec<&str> = vec![&base];
            parts.extend(spec[dots..].split('.').filter(|s| !s.is_empty()));
            return self.lookup_with_extensions(&paths::join(&parts), "py");
        }
        let segs: Vec<&str> = spec.split('.').filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return None;
        }
        let mut bases: Vec<&str> = self
            .structure
            .roots
            .iter()
            .filter(|r| r.kind == "python")
            .flat_map(|r| r.source_roots.iter().map(String::as_str))
            .collect();
        bases.push(&self.structure.project_root);
        for base in bases {
            let mut parts = vec![base];
            parts.extend(&segs);
            if let Some(h) = self.lookup_with_extensions(&paths::join(&parts), "py") {
                return Some(h);
            }
            if segs.len() > 1 {
                let mut parts = vec![base];
                parts.extend(&segs[..segs.len() - 1]);
                if let Some(h) = self.lookup_with_extensions(&paths::join(&parts), "py") {
                    return Some(h);
                }
            }
        }
        None
    }

    fn resolve_rust(&self, from: &str, spec: &str) -> Option<String> {
        let segs: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
        let head = *segs.first()?;
        if head == "self" || head == "super" {
            let mut base = rust_module_dir(from);
            let mut consumed = 0;
            for s in &segs {
                if *s == "super" {
                    base = paths::dirname(&base);
                } else if *s != "self" {
                    break;
                }
                consumed += 1;
            }
            return self
                .lookup_rust_path(&base, &segs[consumed..])
                .or_else(|| self.lookup_rust_module_file(&base));
        }
        let krate = if head == "crate" {
            find_owning_root(self.structure, from, Some(&["cargo"]))
        } else {
            let want = head.replace('-', "_");
            self.structure
                .roots
                .iter()
                .find(|r| r.kind == "cargo" && r.import_path.as_deref() == Some(want.as_str()))
        };
        let Some(krate) = krate else {
            return self.lookup_rust_path(&rust_module_dir(from), &segs);
        };
        let rest = &segs[1..];
        for base in &krate.source_roots {
            if let Some(h) = self
                .lookup_rust_path(base, rest)
                .or_else(|| self.lookup_rust_crate_root(base))
            {
                return Some(h);
            }
        }
        None
    }

    fn lookup_rust_path(&self, base: &str, segs: &[&str]) -> Option<String> {
        for end in (1..=segs.len()).rev() {
            let mut parts = vec![base];
            parts.extend(&segs[..end]);
            if let Some(h) = self.lookup_with_extensions(&paths::join(&parts), "rs") {
                return Some(h);
            }
        }
        None
    }

    fn lookup_rust_module_file(&self, dir: &str) -> Option<String> {
        self.lookup_with_extensions(dir, "rs")
            .or_else(|| self.lookup_rust_crate_root(dir))
    }

    fn lookup_rust_crate_root(&self, src_root: &str) -> Option<String> {
        self.lookup(&paths::join(&[src_root, "lib.rs"]))
            .or_else(|| self.lookup(&paths::join(&[src_root, "main.rs"])))
    }

    fn resolve_jvm(&self, spec: &str) -> Option<String> {
        let segs: Vec<&str> = spec.split('.').filter(|s| !s.is_empty()).collect();
        let last = *segs.last()?;
        let mut bases: Vec<&str> = self
            .structure
            .roots
            .iter()
            .filter(|r| r.kind == "maven" || r.kind == "gradle")
            .flat_map(|r| r.source_roots.iter().map(String::as_str))
            .collect();
        bases.push(&self.structure.project_root);
        let wildcard = last == "*";
        let parts: &[&str] = if wildcard {
            &segs[..segs.len() - 1]
        } else {
            &segs
        };
        for base in bases {
            if wildcard {
                let mut p = vec![base];
                p.extend(parts);
                let target = paths::join(&p);
                if let Some(h) = self
                    .representative_in(&target, "jvm")
                    .or_else(|| self.lookup_with_extensions(&target, "jvm"))
                {
                    return Some(h);
                }
                continue;
            }
            let lowest = 2.max(parts.len() as isize - 2);
            let mut end = parts.len() as isize;
            while end >= lowest {
                let mut p = vec![base];
                p.extend(&parts[..end as usize]);
                if let Some(h) = self.lookup_with_extensions(&paths::join(&p), "jvm") {
                    return Some(h);
                }
                end -= 1;
            }
        }
        None
    }

    /// `resolveInclude` (c) and `resolveRuby`: relative to the importer, then
    /// `<root>/<sub>`, then the project root.
    fn resolve_relative_or_bases(
        &self,
        from: &str,
        spec: &str,
        family: &str,
        sub: &str,
    ) -> Option<String> {
        if let Some(h) =
            self.lookup_with_extensions(&paths::join(&[&paths::dirname(from), spec]), family)
        {
            return Some(h);
        }
        let root = &self.structure.project_root;
        for base in [paths::join(&[root, sub]), root.clone()] {
            if let Some(h) = self.lookup_with_extensions(&paths::join(&[&base, spec]), family) {
                return Some(h);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &str, kind: &'static str, ip: Option<&str>, src: &[&str]) -> ModuleRoot {
        ModuleRoot {
            dir: dir.into(),
            kind,
            name: format!("{kind}:{dir}"),
            import_path: ip.map(Into::into),
            source_roots: src.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rust_paths_resolve_through_crates_and_modules() {
        let s = ProjectStructure {
            project_root: "C:/r".into(),
            roots: vec![
                root(
                    "C:/r/crates/core",
                    "cargo",
                    Some("my_core"),
                    &["C:/r/crates/core/src"],
                ),
                root(
                    "C:/r/crates/app",
                    "cargo",
                    Some("app"),
                    &["C:/r/crates/app/src"],
                ),
            ],
        };
        let f = files(&[
            r"C:\r\crates\core\src\lib.rs",
            r"C:\r\crates\core\src\model\mod.rs",
            r"C:\r\crates\core\src\model\user.rs",
            r"C:\r\crates\app\src\main.rs",
            r"C:\r\crates\app\src\cli.rs",
            r"C:\r\crates\app\src\cli\args.rs",
        ]);
        let r = ModuleResolver::new(&s, &f, &[]);
        let main = r"C:\r\crates\app\src\main.rs";
        let cli = r"C:\r\crates\app\src\cli.rs";
        let args = r"C:\r\crates\app\src\cli\args.rs";
        assert_eq!(r.resolve(main, "rs", "cli").as_deref(), Some(cli));
        assert_eq!(
            r.resolve(main, "rs", "crate::cli::Opts").as_deref(),
            Some(cli)
        );
        assert_eq!(r.resolve(cli, "rs", "self::args").as_deref(), Some(args));
        assert_eq!(
            r.resolve(args, "rs", "super::super::cli").as_deref(),
            Some(cli)
        );
        assert_eq!(
            r.resolve(main, "rs", "my-core::model::user::User")
                .as_deref(),
            Some(r"C:\r\crates\core\src\model\user.rs")
        );
        assert_eq!(
            r.resolve(main, "rs", "my_core::model").as_deref(),
            Some(r"C:\r\crates\core\src\model\mod.rs")
        );
        assert_eq!(
            r.resolve(main, "rs", "my_core::Thing").as_deref(),
            Some(r"C:\r\crates\core\src\lib.rs")
        );
        assert_eq!(r.resolve(main, "rs", "std::fmt"), None);
    }

    #[test]
    fn other_families_resolve() {
        let s = ProjectStructure {
            project_root: "C:/r".into(),
            roots: vec![
                root("C:/r/web", "npm", Some("@a/web"), &["C:/r/web"]),
                root("C:/r/svc", "go", Some("example.com/svc"), &["C:/r/svc"]),
                root("C:/r/py", "python", None, &["C:/r/py/src", "C:/r/py"]),
            ],
        };
        let f = files(&[
            r"C:\r\web\src\index.ts",
            r"C:\r\web\src\util.ts",
            r"C:\r\svc\api\b.go",
            r"C:\r\svc\api\api.go",
            r"C:\r\py\src\pkg\__init__.py",
            r"C:\r\py\src\pkg\mod.py",
            r"C:\r\src\Main.java",
            r"C:\r\include\x.h",
            r"C:\r\cs\A.cs",
        ]);
        let ns = vec![("Acme::Core".to_string(), r"C:\r\cs\A.cs".to_string())];
        let r = ModuleResolver::new(&s, &f, &ns);
        let idx = r"C:\r\web\src\index.ts";
        assert_eq!(
            r.resolve(idx, "ts", "./util.js").as_deref(),
            Some(r"C:\r\web\src\util.ts")
        );
        assert_eq!(
            r.resolve(r"C:\r\x.ts", "ts", "@a/web").as_deref(),
            Some(idx)
        );
        assert_eq!(
            r.resolve(r"C:\r\x.ts", "ts", "@a/web/util").as_deref(),
            Some(r"C:\r\web\src\util.ts")
        );
        assert_eq!(
            r.resolve(r"C:\r\m.go", "go", "example.com/svc/api")
                .as_deref(),
            Some(r"C:\r\svc\api\api.go")
        );
        assert_eq!(
            r.resolve(r"C:\r\py\src\pkg\mod.py", "py", ".").as_deref(),
            Some(r"C:\r\py\src\pkg\__init__.py")
        );
        assert_eq!(
            r.resolve(r"C:\r\a.py", "py", "pkg.mod.thing").as_deref(),
            Some(r"C:\r\py\src\pkg\mod.py")
        );
        assert_eq!(
            r.resolve(r"C:\r\a.java", "java", "src.Main").as_deref(),
            Some(r"C:\r\src\Main.java")
        );
        assert_eq!(
            r.resolve(r"C:\r\a.c", "c", "x.h").as_deref(),
            Some(r"C:\r\include\x.h")
        );
        assert_eq!(
            r.resolve(r"C:\r\b.cs", "csharp", "acme.core.Widget")
                .as_deref(),
            Some(r"C:\r\cs\A.cs")
        );
    }
}
