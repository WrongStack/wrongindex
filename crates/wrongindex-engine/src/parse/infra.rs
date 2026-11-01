//! Port of WrongStack's `infra-parser.ts` (`detectInfraArtifactKind`,
//! `parseInfraArtifact`) for the artifact kinds a file routed to the regex
//! extractor (`generic-parser.ts`) can have: Dockerfile, Makefile/justfile,
//! CMakeLists.txt, dotenv, Terraform, SQL, GraphQL, Protobuf and Markdown
//! links, plus the YAML kinds `yaml-parser.ts` adds refs from (Docker
//! Compose, GitHub workflows, OpenAPI, Kubernetes). `shell` is reachable from
//! neither route (its files are `shell`, not ported) and is reported as not
//! ported.
//!
//! Every regex is the TypeScript source text run by [`crate::jsre`].

use std::sync::OnceLock;

use super::{ParsedRef, ParsedSymbol};
use crate::js::{self, U16, utf16};
use crate::jsre::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dockerfile,
    DockerCompose,
    GithubWorkflow,
    Makefile,
    Cmake,
    Dotenv,
    Openapi,
    Terraform,
    Kubernetes,
    Sql,
    Graphql,
    Protobuf,
    MarkdownWiki,
    Shell,
}

/// `detectInfraArtifactKind(file, content)`.
pub fn detect_kind(file: &str, content: &U16) -> Option<Kind> {
    let norm = file.replace('\\', "/");
    let lower = js_lower(&norm);
    let base = &lower[lower.rfind('/').map_or(0, |i| i + 1)..];
    if base == "dockerfile" || base.starts_with("dockerfile.") {
        return Some(Kind::Dockerfile);
    }
    if matches!(
        base,
        "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml"
    ) {
        return Some(Kind::DockerCompose);
    }
    let yml = base.ends_with(".yml") || base.ends_with(".yaml");
    if lower.contains(".github/workflows/") && yml {
        return Some(Kind::GithubWorkflow);
    }
    if matches!(base, "makefile" | "gnumakefile" | "justfile") {
        return Some(Kind::Makefile);
    }
    if base == "cmakelists.txt" {
        return Some(Kind::Cmake);
    }
    if base == ".env" || base.starts_with(".env.") {
        return Some(Kind::Dotenv);
    }
    if matches!(
        base,
        "openapi.yml"
            | "openapi.yaml"
            | "openapi.json"
            | "swagger.yml"
            | "swagger.yaml"
            | "swagger.json"
    ) {
        return Some(Kind::Openapi);
    }
    if base.ends_with(".tf") || base.ends_with(".tfvars") {
        return Some(Kind::Terraform);
    }
    if yml
        && (lower.contains("/k8s/")
            || lower.starts_with("k8s/")
            || lower.contains("/kubernetes/")
            || lower.starts_with("kubernetes/")
            || lower.contains("/manifests/")
            || lower.starts_with("manifests/")
            || base.ends_with(".k8s.yml")
            || base.ends_with(".k8s.yaml")
            || (re(&KUBE_API, r"^apiVersion\s*:", "m").test(content)
                && re(&KUBE_KIND, r"^kind\s*:", "m").test(content)))
    {
        return Some(Kind::Kubernetes);
    }
    if base.ends_with(".sql") {
        return Some(Kind::Sql);
    }
    if base.ends_with(".graphql") || base.ends_with(".gql") {
        return Some(Kind::Graphql);
    }
    if base.ends_with(".proto") {
        return Some(Kind::Protobuf);
    }
    if base.ends_with(".md") || base.ends_with(".mdx") {
        return Some(Kind::MarkdownWiki);
    }
    if base.ends_with(".sh") || base.ends_with(".bash") || base.ends_with(".zsh") {
        return Some(Kind::Shell);
    }
    None
}

/// `String.prototype.toLowerCase` for the path tests above (only ASCII
/// comparisons are made on the result, and non-ASCII lowercasing never
/// produces ASCII except for U+0130 → `i̇`, which keeps a non-ASCII mark).
fn js_lower(s: &str) -> String {
    s.to_lowercase()
}

static KUBE_API: OnceLock<Regex> = OnceLock::new();
static KUBE_KIND: OnceLock<Regex> = OnceLock::new();

fn re(cell: &'static OnceLock<Regex>, src: &str, flags: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(src, flags))
}

macro_rules! regex {
    ($src:expr, $flags:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        re(&CELL, $src, $flags)
    }};
}

/// `parseInfraArtifact` output.
#[derive(Debug, Default)]
pub struct Infra {
    pub kind: Option<Kind>,
    pub symbols: Vec<ParsedSymbol>,
    pub refs: Vec<ParsedRef>,
    /// Set for a kind this port does not parse.
    pub unsupported: Option<String>,
}

/// `makeSym`: column 0, signature trimmed and cut to 500 units, text from
/// the untrimmed signature.
fn make_sym(
    kind: &'static str,
    name: &U16,
    line: usize,
    signature: &U16,
    scope: &U16,
) -> ParsedSymbol {
    let sig = js::trim(signature);
    let sig = &sig[..sig.len().min(500)];
    let mut text = name.to_vec();
    text.push(0x20);
    text.extend_from_slice(signature);
    let text = js::trim(&text);
    ParsedSymbol {
        kind,
        name: js::string(name),
        line,
        col: 0,
        signature: js::string(sig),
        doc_comment: String::new(),
        scope: js::string(scope),
        text: js::string(&text[..text.len().min(1000)]),
        is_exported: None,
        end_line: None,
    }
}

fn mk_ref(
    lang: &'static str,
    to_name: &U16,
    line: usize,
    call_type: &'static str,
    module: bool,
) -> ParsedRef {
    ParsedRef {
        to_name: js::string(to_name),
        call_type,
        line,
        lang,
        module: module.then(|| js::string(to_name)),
    }
}

fn starts_with(s: &U16, lit: &str) -> bool {
    let l = utf16(lit);
    s.len() >= l.len() && s[..l.len()] == l[..]
}

fn concat(parts: &[&U16]) -> Vec<u16> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

/// `line.length - line.trimStart().length`.
fn indent_of(line: &U16) -> isize {
    (line.len() - js::trim_start(line).len()) as isize
}

/// `s.replace(/^['"]|['"]$/g, '')`: at most one quote off each end.
fn strip_one_quote_each_end(s: &U16) -> &U16 {
    let q = |c: u16| c == b'"' as u16 || c == b'\'' as u16;
    let s = match s.first() {
        Some(&c) if q(c) => &s[1..],
        _ => s,
    };
    match s.last() {
        Some(&c) if q(c) => &s[..s.len() - 1],
        _ => s,
    }
}

fn remove_quotes(s: &U16) -> Vec<u16> {
    s.iter().copied().filter(|&c| c != b'"' as u16).collect()
}

/// `parseInfraArtifact({ file, content, lang })`.
pub fn parse(file: &str, content: &U16, lang: &'static str) -> Infra {
    let Some(kind) = detect_kind(file, content) else {
        return Infra::default();
    };
    let mut out = Infra {
        kind: Some(kind),
        ..Infra::default()
    };
    if content.is_empty() {
        out.kind = None;
        return out;
    }
    let lines = js::split_lines(content);
    let empty: &U16 = &[];
    let syms = &mut out.symbols;
    let refs = &mut out.refs;
    match kind {
        Kind::Dockerfile => {
            let mut stage: Vec<u16> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "#") {
                    continue;
                }
                let n = i + 1;
                let from = regex!(
                    r"^FROM\s+(?:--platform=\S+\s+)?(\S+)(?:\s+AS\s+([A-Za-z0-9_.-]+))?",
                    "i"
                );
                if let Some(m) = from.exec(raw) {
                    let base = m.text(raw, 1).unwrap_or(empty);
                    let name = m.text(raw, 2).unwrap_or(base);
                    stage = name.to_vec();
                    syms.push(make_sym("namespace", name, n, raw, empty));
                    if !base.is_empty() {
                        refs.push(mk_ref(lang, base, n, "import", true));
                    }
                    continue;
                }
                let copy = regex!(r"^COPY\s+.*--from=([A-Za-z0-9_.:/-]+)", "i");
                if let Some(t) = copy.exec(raw).and_then(|m| m.text(raw, 1))
                    && !t.is_empty()
                {
                    refs.push(mk_ref(lang, t, n, "call", false));
                }
                let env = regex!(r"^(?:ARG|ENV)\s+([A-Za-z_][A-Za-z0-9_]*)", "i");
                if let Some(t) = env.exec(raw).and_then(|m| m.text(raw, 1))
                    && !t.is_empty()
                {
                    syms.push(make_sym("const", t, n, raw, &stage));
                    continue;
                }
                let expose = regex!(r"^EXPOSE\s+(\d+(?:\/(?:tcp|udp))?)", "i");
                if let Some(t) = expose.exec(raw).and_then(|m| m.text(raw, 1))
                    && !t.is_empty()
                {
                    let name = concat(&[&utf16("EXPOSE:"), t]);
                    syms.push(make_sym("property", &name, n, raw, &stage));
                }
            }
        }
        Kind::Makefile => {
            for (i, line) in lines.iter().enumerate() {
                if line.is_empty() || line[0] == 0x09 || starts_with(js::trim_start(line), "#") {
                    continue;
                }
                let n = i + 1;
                let target = regex!(r"^([A-Za-z_][\w.-]*)\s*:(?!=)\s*(.*)$", "");
                let Some(m) = target.exec(line) else {
                    continue;
                };
                let Some(name) = m.text(line, 1).filter(|t| !t.is_empty()) else {
                    continue;
                };
                if name == utf16(".PHONY").as_slice() {
                    continue;
                }
                syms.push(make_sym("function", name, n, js::trim(line), empty));
                let prereqs = m.text(line, 2).unwrap_or(empty);
                let ok = regex!(r"^[A-Za-z_][\w.-]*$", "");
                for p in prereqs
                    .split(|&c| js::is_space(c))
                    .filter(|p| !p.is_empty())
                {
                    if ok.test(p) {
                        refs.push(mk_ref(lang, p, n, "call", false));
                    }
                }
            }
        }
        Kind::Cmake => {
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "#") {
                    continue;
                }
                let add = regex!(
                    r"^(?:add_executable|add_library)\s*\(\s*([A-Za-z0-9_.-]+)",
                    "i"
                );
                if let Some(t) = add.exec(raw).and_then(|m| m.text(raw, 1))
                    && !t.is_empty()
                {
                    syms.push(make_sym("function", t, i + 1, raw, empty));
                }
            }
        }
        Kind::Dotenv => {
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "#") {
                    continue;
                }
                let n = i + 1;
                let env = regex!(r"^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$", "");
                let Some(m) = env.exec(raw) else {
                    continue;
                };
                let Some(name) = m.text(raw, 1).filter(|t| !t.is_empty()) else {
                    continue;
                };
                let sig = concat(&[name, &utf16("=***")]);
                syms.push(make_sym("const", name, n, &sig, empty));
                let val = m.text(raw, 2).unwrap_or(empty);
                let interp = regex!(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", "g");
                for x in interp.match_all(val) {
                    if let Some(t) = x.text(val, 1).filter(|t| !t.is_empty()) {
                        refs.push(mk_ref(lang, t, n, "call", false));
                    }
                }
            }
        }
        Kind::Terraform => {
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "#") || starts_with(raw, "//") {
                    continue;
                }
                let n = i + 1;
                let res = regex!(r#"^(resource|data)\s+"([^"]+)"\s+"([^"]+)""#, "");
                if let Some(m) = res.exec(raw) {
                    let (bt, rt, rn) = (
                        m.text(raw, 1).unwrap(),
                        m.text(raw, 2).unwrap(),
                        m.text(raw, 3).unwrap(),
                    );
                    let is_data = bt == utf16("data").as_slice();
                    let dot = utf16(".");
                    let q = utf16("\"");
                    let sp = utf16(" ");
                    let full = if is_data {
                        concat(&[&utf16("data."), rt, &dot, rn])
                    } else {
                        concat(&[rt, &dot, rn])
                    };
                    let sig = concat(&[bt, &sp, &q, rt, &q, &sp, &q, rn, &q]);
                    let kind = if is_data { "type" } else { "class" };
                    syms.push(make_sym(kind, &full, n, &sig, bt));
                    continue;
                }
                let mvo = regex!(r#"^(module|variable|output)\s+"([^"]+)""#, "");
                if let Some(m) = mvo.exec(raw) {
                    let (bt, bn) = (m.text(raw, 1).unwrap(), m.text(raw, 2).unwrap());
                    let b = js::string(bt);
                    let prefix = if b == "variable" {
                        utf16("var")
                    } else {
                        bt.to_vec()
                    };
                    let kind = match b.as_str() {
                        "module" => "namespace",
                        "variable" => "const",
                        _ => "property",
                    };
                    let name = concat(&[&prefix, &utf16("."), bn]);
                    let q = utf16("\"");
                    let sig = concat(&[bt, &utf16(" "), &q, bn, &q]);
                    syms.push(make_sym(kind, &name, n, &sig, bt));
                    continue;
                }
                let source = regex!(r#"^source\s*=\s*"([^"]+)""#, "");
                if let Some(t) = source.exec(raw).and_then(|m| m.text(raw, 1)) {
                    refs.push(mk_ref(lang, t, n, "import", true));
                }
                let vr = regex!(r"\b(var|module)\.([A-Za-z0-9_-]+)", "g");
                for m in vr.match_all(raw) {
                    let name = concat(&[
                        m.text(raw, 1).unwrap(),
                        &utf16("."),
                        m.text(raw, 2).unwrap(),
                    ]);
                    refs.push(mk_ref(lang, &name, n, "call", false));
                }
            }
        }
        Kind::Sql => {
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "--") {
                    continue;
                }
                let n = i + 1;
                let create = regex!(
                    r#"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:TABLE|VIEW|INDEX|FUNCTION|PROCEDURE|TRIGGER)\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z_"][\w."]*)"#,
                    "i"
                );
                if let Some(t) = create.exec(raw).and_then(|m| m.text(raw, 1))
                    && !t.is_empty()
                {
                    syms.push(make_sym("type", &remove_quotes(t), n, raw, empty));
                }
                let refre = regex!(r#"\bREFERENCES\s+([A-Za-z_"][\w."]*)"#, "gi");
                for m in refre.match_all(raw) {
                    let target = remove_quotes(m.text(raw, 1).unwrap_or(empty));
                    if !target.is_empty() {
                        refs.push(mk_ref(lang, &target, n, "type_ref", false));
                    }
                }
            }
        }
        Kind::Graphql => {
            let mut current = false;
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "#") {
                    continue;
                }
                let n = i + 1;
                let ty = regex!(
                    r"^(?:extend\s+)?(type|interface|input|enum|union|scalar)\s+([A-Za-z_]\w*)(?:\s+implements\s+([^{]+))?",
                    "i"
                );
                if let Some(m) = ty.exec(raw)
                    && let Some(name) = m.text(raw, 2).filter(|t| !t.is_empty())
                {
                    current = true;
                    syms.push(make_sym("type", name, n, raw, empty));
                    if let Some(list) = m.text(raw, 3).filter(|t| !t.is_empty()) {
                        for part in list.split(|&c| c == b'&' as u16 || c == b',' as u16) {
                            let t = js::trim(part);
                            if !t.is_empty() {
                                refs.push(mk_ref(lang, t, n, "implement", false));
                            }
                        }
                    }
                    continue;
                }
                if starts_with(raw, "}") {
                    current = false;
                    continue;
                }
                if current {
                    let field = regex!(
                        r"^([A-Za-z_]\w*)\s*(?:\([^)]*\))?\s*:\s*[[!\s]*([A-Za-z_]\w*)",
                        ""
                    );
                    if let Some(t) = field.exec(raw).and_then(|m| m.text(raw, 2))
                        && !t.is_empty()
                        && !["String", "Int", "Float", "Boolean", "ID"]
                            .iter()
                            .any(|s| utf16(s) == t)
                    {
                        refs.push(mk_ref(lang, t, n, "type_ref", false));
                    }
                }
            }
        }
        Kind::Protobuf => {
            let mut scope: Vec<u16> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                let raw = js::trim(line);
                if raw.is_empty() || starts_with(raw, "//") {
                    continue;
                }
                let n = i + 1;
                let decl = regex!(r"^(service|message|enum)\s+([A-Za-z_]\w*)", "");
                if let Some(name) = decl.exec(raw).and_then(|m| m.text(raw, 2))
                    && !name.is_empty()
                {
                    scope = name.to_vec();
                    syms.push(make_sym("type", name, n, raw, empty));
                    continue;
                }
                let rpc = regex!(
                    r"^rpc\s+([A-Za-z_]\w*)\s*\(\s*(?:stream\s+)?([A-Za-z_][\w.]*)\s*\)\s*returns\s*\(\s*(?:stream\s+)?([A-Za-z_][\w.]*)\s*\)",
                    ""
                );
                if let Some(m) = rpc.exec(raw)
                    && let Some(name) = m.text(raw, 1).filter(|t| !t.is_empty())
                {
                    syms.push(make_sym("function", name, n, raw, &scope));
                    for g in [2, 3] {
                        let Some(t) = m.text(raw, g).filter(|t| !t.is_empty()) else {
                            continue;
                        };
                        let scalar = [
                            "double", "float", "int32", "int64", "uint32", "uint64", "sint32",
                            "sint64", "fixed32", "fixed64", "sfixed32", "sfixed64", "bool",
                            "string", "bytes",
                        ]
                        .iter()
                        .any(|s| utf16(s) == t);
                        if !scalar {
                            let last = t.split(|&c| c == b'.' as u16).next_back().unwrap_or(t);
                            refs.push(mk_ref(lang, last, n, "type_ref", false));
                        }
                    }
                }
            }
        }
        Kind::MarkdownWiki => {
            let mut in_fence = false;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = js::trim(line);
                if regex!(r"^(`{3,}|~{3,})", "").test(trimmed) {
                    in_fence = !in_fence;
                    continue;
                }
                if in_fence {
                    continue;
                }
                let n = i + 1;
                let wiki = regex!(r"\[\[([^\]|#]+)(?:#[^\]|]+)?(?:\|[^\]]+)?\]\]", "g");
                for m in wiki.match_all(line) {
                    let t = js::trim(m.text(line, 1).unwrap_or(empty));
                    if !t.is_empty() {
                        refs.push(mk_ref(lang, t, n, "import", true));
                    }
                }
                let link = regex!(r"(?<!!)\[[^\]]+\]\(([^)\s]+)\)", "g");
                for m in link.match_all(line) {
                    let href = js::trim(m.text(line, 1).unwrap_or(empty));
                    if href.is_empty() || regex!(r"^(?:https?:|mailto:|#)", "i").test(href) {
                        continue;
                    }
                    let clean = href.split(|&c| c == b'#' as u16).next().unwrap_or(href);
                    if !clean.is_empty() {
                        refs.push(mk_ref(lang, clean, n, "import", true));
                    }
                }
            }
        }
        Kind::DockerCompose => {
            let mut in_services = false;
            let mut current: Vec<u16> = Vec::new();
            let mut in_depends_on = false;
            let mut service_indent: isize = -1;
            let mut prop_indent: isize = -1;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = js::trim(line);
                if trimmed.is_empty() || starts_with(trimmed, "#") {
                    continue;
                }
                let indent = indent_of(line);
                let n = i + 1;
                if indent == 0 {
                    in_services = regex!(r"^services\s*:", "").test(trimmed);
                    current.clear();
                    in_depends_on = false;
                    service_indent = -1;
                    prop_indent = -1;
                    continue;
                }
                if !in_services {
                    continue;
                }
                if service_indent < 0 {
                    service_indent = indent;
                }
                if !current.is_empty() && indent > service_indent && prop_indent < 0 {
                    prop_indent = indent;
                }
                if indent == service_indent {
                    let svc = regex!(r"^([A-Za-z0-9_.-]+)\s*:", "");
                    if let Some(name) = svc.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !name.is_empty()
                    {
                        current = name.to_vec();
                        in_depends_on = false;
                        prop_indent = -1;
                        let sig = concat(&[&utf16("service "), name]);
                        syms.push(make_sym("namespace", name, n, &sig, &utf16("services")));
                    }
                    continue;
                }
                if !current.is_empty() && indent == prop_indent {
                    in_depends_on = regex!(r"^depends_on\s*:", "").test(trimmed);
                    let image = regex!(r#"^image\s*:\s*['"]?([^\s'"]+)"#, "");
                    if let Some(t) = image.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !t.is_empty()
                    {
                        refs.push(mk_ref(lang, t, n, "import", true));
                    }
                    continue;
                }
                if !current.is_empty() && in_depends_on && indent >= prop_indent {
                    let list = regex!(r"^-\s*([A-Za-z0-9_.-]+)", "");
                    let map = regex!(r"^([A-Za-z0-9_.-]+)\s*:", "");
                    let target = list
                        .exec(trimmed)
                        .and_then(|m| m.text(trimmed, 1))
                        .or_else(|| map.exec(trimmed).and_then(|m| m.text(trimmed, 1)));
                    if let Some(t) = target
                        && !t.is_empty()
                        && t != utf16("condition").as_slice()
                    {
                        refs.push(mk_ref(lang, t, n, "call", false));
                    }
                }
            }
        }
        Kind::GithubWorkflow => {
            let mut in_jobs = false;
            let mut current: Vec<u16> = Vec::new();
            let mut job_indent: isize = -1;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = js::trim(line);
                if trimmed.is_empty() || starts_with(trimmed, "#") {
                    continue;
                }
                let indent = indent_of(line);
                let n = i + 1;
                if indent == 0 {
                    in_jobs = regex!(r"^jobs\s*:", "").test(trimmed);
                    current.clear();
                    job_indent = -1;
                    continue;
                }
                if !in_jobs {
                    continue;
                }
                if job_indent < 0 {
                    job_indent = indent;
                }
                if indent == job_indent {
                    let job = regex!(r"^([A-Za-z0-9_.-]+)\s*:", "");
                    if let Some(name) = job.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !name.is_empty()
                    {
                        current = name.to_vec();
                        let sig = concat(&[&utf16("job "), name]);
                        syms.push(make_sym("function", name, n, &sig, &utf16("jobs")));
                    }
                    continue;
                }
                if current.is_empty() || indent <= job_indent {
                    continue;
                }
                let inline = regex!(r"^needs\s*:\s*\[([^\]]+)\]", "");
                if let Some(list) = inline
                    .exec(trimmed)
                    .and_then(|m| m.text(trimmed, 1))
                    .filter(|t| !t.is_empty())
                {
                    for raw in list.split(|&c| c == b',' as u16) {
                        let need = strip_one_quote_each_end(js::trim(raw));
                        if !need.is_empty() {
                            refs.push(mk_ref(lang, need, n, "call", false));
                        }
                    }
                } else {
                    let single = regex!(r"^needs\s*:\s*([A-Za-z0-9_.-]+)\s*$", "");
                    if let Some(t) = single.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !t.is_empty()
                    {
                        refs.push(mk_ref(lang, t, n, "call", false));
                    }
                }
                let uses = regex!(r#"^(?:-\s*)?uses\s*:\s*['"]?([^\s'"]+)"#, "");
                if let Some(t) = uses.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !t.is_empty()
                {
                    refs.push(mk_ref(lang, t, n, "import", true));
                }
            }
        }
        Kind::Openapi => {
            let mut in_schemas = false;
            let mut schemas_indent: isize = 0;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = js::trim(line);
                if trimmed.is_empty() || starts_with(trimmed, "#") {
                    continue;
                }
                let n = i + 1;
                let indent = indent_of(line);
                if regex!(r"^schemas\s*:", "").test(trimmed) {
                    in_schemas = true;
                    schemas_indent = indent;
                    continue;
                }
                if in_schemas && indent <= schemas_indent {
                    in_schemas = false;
                }
                if in_schemas && indent == schemas_indent + 2 {
                    let schema = regex!(r"^([A-Za-z_][A-Za-z0-9_.-]*)\s*:", "");
                    if let Some(name) = schema.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !name.is_empty()
                    {
                        let sig = concat(&[&utf16("schema "), name]);
                        syms.push(make_sym(
                            "type",
                            name,
                            n,
                            &sig,
                            &utf16("components.schemas"),
                        ));
                        continue;
                    }
                }
                let path = regex!(r#"^['"]?(\/[A-Za-z0-9_/{}-]+)['"]?\s*:"#, "");
                if let Some(name) = path.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !name.is_empty()
                {
                    let sig = concat(&[&utf16("endpoint "), name]);
                    syms.push(make_sym("function", name, n, &sig, &utf16("paths")));
                    continue;
                }
                let r = regex!(
                    r#"\$ref\s*:\s*['"]?#\/components\/schemas\/([A-Za-z0-9_.-]+)['"]?"#,
                    ""
                );
                if let Some(t) = r.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !t.is_empty()
                {
                    refs.push(mk_ref(lang, t, n, "type_ref", false));
                }
            }
        }
        Kind::Kubernetes => {
            let mut current_kind: Vec<u16> = Vec::new();
            let mut in_metadata = false;
            let mut metadata_line = 1usize;
            let mut metadata_indent: isize = -1;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = js::trim(line);
                if trimmed.is_empty() || starts_with(trimmed, "#") {
                    continue;
                }
                let n = i + 1;
                let indent = indent_of(line);
                if trimmed == utf16("---").as_slice() {
                    current_kind.clear();
                    in_metadata = false;
                    continue;
                }
                if indent == 0 {
                    let k = regex!(r#"^kind\s*:\s*['"]?([A-Za-z0-9_.-]+)['"]?"#, "");
                    if let Some(t) = k.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !t.is_empty()
                    {
                        current_kind = t.to_vec();
                        continue;
                    }
                    in_metadata = regex!(r"^metadata\s*:", "").test(trimmed);
                    if in_metadata {
                        metadata_line = n;
                    }
                    metadata_indent = -1;
                    continue;
                }
                if in_metadata && metadata_indent < 0 {
                    metadata_indent = indent;
                }
                if in_metadata && indent == metadata_indent && !current_kind.is_empty() {
                    let name = regex!(r#"^name\s*:\s*['"]?([A-Za-z0-9_.-]+)['"]?"#, "");
                    if let Some(t) = name.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                        && !t.is_empty()
                    {
                        let full = concat(&[&current_kind, &utf16(":"), t]);
                        let sig = concat(&[&utf16("k8s "), &current_kind, &utf16(" "), t]);
                        syms.push(make_sym("class", &full, metadata_line, &sig, &current_kind));
                        in_metadata = false;
                        continue;
                    }
                }
                let image = regex!(r#"^(?:-\s*)?image\s*:\s*['"]?([^\s'"]+)"#, "");
                if let Some(t) = image.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !t.is_empty()
                {
                    refs.push(mk_ref(lang, t, n, "import", true));
                }
                let svc = regex!(
                    r#"^(?:serviceName|claimName|configMapName|secretName)\s*:\s*['"]?([A-Za-z0-9_.-]+)['"]?"#,
                    ""
                );
                if let Some(t) = svc.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !t.is_empty()
                {
                    refs.push(mk_ref(lang, t, n, "call", false));
                }
            }
        }
        Kind::Shell => {
            out.unsupported = Some(format!("infra artifact {kind:?} not ported"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    type Syms = Vec<(String, &'static str, usize, String)>;
    type Refs = Vec<(String, &'static str, usize, Option<String>)>;

    fn run(file: &str, src: &str) -> (Syms, Refs) {
        let r = parse(file, &utf16(src), "other");
        (
            r.symbols
                .into_iter()
                .map(|s| (s.name, s.kind, s.line, s.scope))
                .collect(),
            r.refs
                .into_iter()
                .map(|x| (x.to_name, x.call_type, x.line, x.module))
                .collect(),
        )
    }

    #[test]
    fn dockerfile_stages_and_refs() {
        let (s, r) = run(
            "C:/p/Dockerfile",
            "FROM rust:1 AS build\nARG X\nCOPY --from=build /a /b\nfrom alpine\nEXPOSE 80/tcp\n",
        );
        assert_eq!(
            s,
            vec![
                ("build".into(), "namespace", 1, "".into()),
                ("X".into(), "const", 2, "build".into()),
                ("alpine".into(), "namespace", 4, "".into()),
                ("EXPOSE:80/tcp".into(), "property", 5, "alpine".into()),
            ]
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r[1], ("build".into(), "call", 3, None));
    }

    #[test]
    fn markdown_links_skip_fences_images_and_urls() {
        let (_, r) = run(
            "docs/a.md",
            "[a](./b.md#x) ![i](c.png) [u](https://x)\n```\n[n](no.md)\n```\n[[Page#H|L]]\n",
        );
        assert_eq!(
            r,
            vec![
                ("./b.md".into(), "import", 1, Some("./b.md".into())),
                ("Page".into(), "import", 5, Some("Page".into())),
            ]
        );
    }

    #[test]
    fn kinds_follow_the_typescript_order() {
        let e = utf16("");
        assert_eq!(detect_kind("x/Dockerfile.md", &e), Some(Kind::Dockerfile));
        assert_eq!(detect_kind("x/.env.toml", &e), Some(Kind::Dotenv));
        assert_eq!(detect_kind("x/main.tf", &e), Some(Kind::Terraform));
        assert_eq!(detect_kind("x/README.MD", &e), Some(Kind::MarkdownWiki));
        assert_eq!(detect_kind("x/a.css", &e), None);
    }
}
