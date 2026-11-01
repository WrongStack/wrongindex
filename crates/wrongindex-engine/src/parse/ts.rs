//! TypeScript/JavaScript symbols and references: a port of WrongStack's
//! `ts-parser.ts` (`parseSymbols` through `parseFileContent(file, content,
//! 'ts' | 'tsx' | 'js' | 'jsx')`) on the oxc parser instead of the TypeScript
//! compiler.
//!
//! `ts-parser.ts` reads everything it stores straight from the source text
//! (signatures are whitespace-collapsed source slices, doc comments are the
//! first `/** */` among the leading comments), so a different parser yields
//! identical output as long as it produces the same declarations and
//! references at the same positions. The mapping follows the validated spike
//! port (oxc ESTree → `ts-parser.ts`):
//!
//! - one node record per oxc AST node; emitted symbols and refs hang off the
//!   node that TypeScript would visit for them, and the output order is a
//!   pre-order walk with every node's children sorted by `(start, longest
//!   first)` - TypeScript's `forEachChild` source order;
//! - TypeScript node positions: `getStart` includes decorators and the
//!   `export` modifier that oxc keeps on a wrapper node; `pos` (full start) is
//!   found by walking back over TypeScript whitespace and comments;
//! - lines use TypeScript's line breaks (CR, LF, CRLF, U+2028, U+2029) and
//!   columns count UTF-16 code units.
//!
//! Where the trees could differ the file is refused (`Parsed::unsupported`) so
//! the native writer leaves the project to WrongStack: any oxc syntax error
//! (TypeScript recovers and still indexes), a top-level `await` in a file
//! without module syntax that TypeScript parses as an identifier there
//! (`await (x)` is a call, `await -x` a subtraction; `await f()` is the same
//! in both), `for await`/`await using` in such files, and
//! string-literal names or module specifiers holding lone surrogates.

use std::collections::HashMap;

use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    BindingPattern, Class, ClassType, Decorator, Expression, ForStatementInit, ForStatementLeft,
    FunctionType, ImportDeclarationSpecifier, JSXMemberExpressionObject, MethodDefinitionKind,
    ModuleExportName, PropertyKey, PropertyKind, Statement, StringLiteral, TSMethodSignatureKind,
    TSModuleDeclarationBody, TSModuleDeclarationName, TSModuleReference, TSTypeName,
    VariableDeclarationKind,
};
use oxc_ast_visit::Visit;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::{GetSpan, SourceType};

use super::{Parsed, ParsedRef, ParsedSymbol};
use crate::js::{self, U16};

/// Languages routed to `ts-parser.ts`.
pub const LANGS: &[&str] = &["ts", "tsx", "js", "jsx"];

/// Longest signature stored, after whitespace is collapsed.
const SIGNATURE_MAX_CHARS: usize = 500;
/// Raw source read for one signature.
const SIGNATURE_SCAN_CHARS: usize = 4_000;
/// Longest doc comment line stored.
const DOC_MAX_CHARS: usize = 200;

/// `parseFileContent(file, content, lang)` for the TS/JS languages. The
/// language's refs carry `lang`; `withRelations` has no import fallback for
/// them.
pub fn parse(file: &str, lang: &'static str, content: &U16) -> Parsed {
    match extract(file, lang, content) {
        Ok(p) => p,
        Err(why) => Parsed {
            unsupported: Some(why),
            ..Parsed::default()
        },
    }
}

/// The oxc source type for TypeScript's script kind of `file`: `.tsx` is TSX;
/// `.js`/`.mjs`/`.cjs`/`.jsx` are JavaScript, which TypeScript parses with the
/// JSX language variant; every other extension (`.ts`, `.mts`, `.cts`, and
/// unknown ones) is TS; `.d.ts`/`.d.mts`/`.d.cts` are declaration files.
/// Always a module: TypeScript re-parses top-level `await` only in files with
/// module syntax, and other files are refused when they use it.
fn source_type(file: &str) -> SourceType {
    let ext = file
        .rfind('.')
        .map(|i| file[i..].to_ascii_lowercase())
        .unwrap_or_default();
    let st = match ext.as_str() {
        ".tsx" => SourceType::tsx(),
        ".js" | ".mjs" | ".cjs" | ".jsx" => SourceType::jsx(),
        _ if file.ends_with(".d.ts") || file.ends_with(".d.mts") || file.ends_with(".d.cts") => {
            SourceType::d_ts()
        }
        _ => SourceType::ts(),
    };
    st.with_module(true)
}

fn extract(file: &str, lang: &'static str, content: &U16) -> Result<Parsed, String> {
    // Lone surrogates become U+FFFD: one UTF-16 unit either way, so offsets
    // agree; every stored slice is read from `content` itself.
    let text = String::from_utf16_lossy(content);
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, &text, source_type(file))
        .with_options(ParseOptions {
            preserve_parens: true,
            // TypeScript parses a top-level `return` (CommonJS) as a plain
            // return statement; the binder, not the parser, reports it.
            allow_return_outside_function: true,
            ..ParseOptions::default()
        })
        .parse();
    if let Some(e) = ret.errors.first() {
        return Err(format!(
            "TS/JS syntax that TypeScript would recover from ({} error(s), first: {})",
            ret.errors.len(),
            e
        ));
    }
    let program = &ret.program;

    let mut map = vec![0u32; text.len() + 1];
    let mut u = 0u32;
    for (i, ch) in text.char_indices() {
        map[i] = u;
        u += ch.len_utf16() as u32;
    }
    map[text.len()] = u;

    let cx = Ctx::new(content, &map, program);
    let mut w = Walker {
        cx: &cx,
        recs: Vec::new(),
        events: Vec::new(),
        frames: Vec::new(),
        scope: Vec::new(),
        depth: 0,
        fn_depth: 0,
        suppressed: 0,
        top_level_await: false,
        gate: None,
        this_param: None,
    };
    w.visit_program(program);
    if let Some(why) = w.gate {
        return Err(why);
    }
    if w.top_level_await && !has_module_syntax(program) {
        return Err(
            "top-level await that TypeScript parses differently in a file without module syntax"
                .into(),
        );
    }
    Ok(w.finish(lang))
}

/// TypeScript's `isAnExternalModuleIndicatorNode` over the top-level
/// statements (`import.meta` is ignored: such files are refused instead).
fn has_module_syntax(program: &oxc_ast::ast::Program) -> bool {
    program.body.iter().any(|s| match s {
        Statement::ImportDeclaration(_)
        | Statement::ExportAllDeclaration(_)
        | Statement::ExportDefaultDeclaration(_)
        | Statement::ExportNamedDeclaration(_)
        | Statement::TSExportAssignment(_) => true,
        Statement::TSImportEqualsDeclaration(d) => matches!(
            d.module_reference,
            TSModuleReference::ExternalModuleReference(_)
        ),
        _ => false,
    })
}

/// TypeScript's whitespace (`isWhiteSpaceLike`).
fn is_ts_white(c: u16) -> bool {
    matches!(
        c,
        0x20 | 0x09 | 0x0B | 0x0C | 0xA0 | 0x85 | 0x1680 | 0x2000
            ..=0x200B | 0x202F | 0x205F | 0x3000 | 0xFEFF | 0x0A | 0x0D | 0x2028 | 0x2029
    )
}

struct Ctx<'c> {
    content: &'c U16,
    map: &'c [u32],
    /// `ts.computeLineStarts`.
    line_starts: Vec<usize>,
    /// Comments as UTF-16 `(start, end)`, in source order.
    comments: Vec<(usize, usize)>,
    /// Comment start by comment end.
    comment_by_end: HashMap<usize, usize>,
}

impl<'c> Ctx<'c> {
    fn new(content: &'c U16, map: &'c [u32], program: &oxc_ast::ast::Program) -> Self {
        let mut line_starts = vec![0];
        let mut i = 0;
        while i < content.len() {
            match content[i] {
                0x0D => {
                    if content.get(i + 1) == Some(&0x0A) {
                        i += 1;
                    }
                    line_starts.push(i + 1);
                }
                0x0A | 0x2028 | 0x2029 => line_starts.push(i + 1),
                _ => {}
            }
            i += 1;
        }
        let comments: Vec<(usize, usize)> = program
            .comments
            .iter()
            .map(|c| {
                (
                    map[c.span.start as usize] as usize,
                    map[c.span.end as usize] as usize,
                )
            })
            .collect();
        let comment_by_end = comments.iter().map(|&(s, e)| (e, s)).collect();
        Self {
            content,
            map,
            line_starts,
            comments,
            comment_by_end,
        }
    }

    /// UTF-16 offset of a byte offset.
    fn u(&self, byte: u32) -> usize {
        self.map[byte as usize] as usize
    }

    /// 0-based line of a UTF-16 offset.
    fn line_of(&self, pos: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= pos) - 1
    }

    /// TypeScript `node.pos` for a node whose first token starts at `start`:
    /// the end of the previous token.
    fn full_start(&self, start: usize) -> usize {
        let mut p = start;
        loop {
            while p > 0 && is_ts_white(self.content[p - 1]) {
                p -= 1;
            }
            match self.comment_by_end.get(&p) {
                Some(&s) if s < p => p = s,
                _ => return p,
            }
        }
    }

    /// `getJsDoc`: the first line of the first `/** */` comment among the
    /// leading comments at the host's full start. `ts.getLeadingCommentRanges`
    /// collects only after the first CR/LF of the trivia, unless it starts at
    /// position 0.
    fn js_doc(&self, host_start: usize) -> Vec<u16> {
        let fs = self.full_start(host_start);
        let first = self.comments.partition_point(|&(s, _)| s < fs);
        let mut collecting = fs == 0;
        let mut gap_start = fs;
        for &(s, e) in &self.comments[first..] {
            if e > host_start {
                break;
            }
            if !collecting
                && self.content[gap_start..s]
                    .iter()
                    .any(|&c| c == 0x0A || c == 0x0D)
            {
                collecting = true;
            }
            gap_start = e;
            if !collecting {
                continue;
            }
            let trimmed = js::trim(&self.content[s..e]);
            if trimmed.starts_with(&[0x2F, 0x2A, 0x2A]) && trimmed.ends_with(&[0x2A, 0x2F]) {
                let inner: &U16 = if trimmed.len() >= 5 {
                    &trimmed[3..trimmed.len() - 2]
                } else {
                    &[]
                };
                let stripped = strip_star_prefixes(inner);
                let t = js::trim(&stripped);
                let first_line = t.split(|&c| c == 0x0A).next().unwrap_or(&[]);
                let fl = js::trim(first_line);
                return fl[..fl.len().min(DOC_MAX_CHARS)].to_vec();
            }
        }
        Vec::new()
    }

    /// `text.slice(start, min(end, start + 4000)).replace(/\s+/g, ' ').trim().slice(0, 500)`.
    fn signature(&self, start: usize, end: usize) -> Vec<u16> {
        let end = end
            .min(start + SIGNATURE_SCAN_CHARS)
            .min(self.content.len());
        let raw: &U16 = if end > start {
            &self.content[start..end]
        } else {
            &[]
        };
        let mut out = Vec::with_capacity(raw.len());
        let mut in_space = false;
        for &c in raw {
            if js::is_space(c) {
                if !in_space {
                    out.push(0x20);
                }
                in_space = true;
            } else {
                out.push(c);
                in_space = false;
            }
        }
        let t = js::trim(&out);
        t[..t.len().min(SIGNATURE_MAX_CHARS)].to_vec()
    }
}

/// `.replace(/^[ \t]*\*[ ]?/gm, '')`.
fn strip_star_prefixes(s: &U16) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if js::is_line_start(s, i) {
            let mut j = i;
            while j < s.len() && (s[j] == 0x20 || s[j] == 0x09) {
                j += 1;
            }
            if j < s.len() && s[j] == 0x2A {
                j += 1;
                if j < s.len() && s[j] == 0x20 {
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// `/\bexport\b/` (ASCII word characters).
fn mentions_export(sig: &U16) -> bool {
    let word = |c: u16| js::is_word(c);
    let kw: Vec<u16> = js::utf16("export");
    let mut from = 0;
    while let Some(i) = js::index_of(sig, &kw, from) {
        let before = i == 0 || !word(sig[i - 1]);
        let after = i + kw.len() >= sig.len() || !word(sig[i + kw.len()]);
        if before && after {
            return true;
        }
        from = i + 1;
    }
    false
}

/// ECMAScript `Number::toString(10)`.
fn js_number(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v == 0.0 {
        return "0".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if v < 0.0 {
        return format!("-{}", js_number(-v));
    }
    // Shortest round-trip digits and exponent.
    let e = format!("{v:e}");
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let ex = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{sign}{ex}")
        } else {
            format!("{}.{}e{sign}{ex}", &digits[..1], &digits[1..])
        }
    }
}

/// The text TypeScript stores for a numeric literal name: the value's
/// `Number` string (`0x10` -> `16`, `1_000` -> `1000`, `017` -> `15`, and a
/// long decimal integer in exponent form).
fn numeric_name(value: f64) -> String {
    js_number(value)
}

/// One emitted record: a symbol or a reference, attached to a node record.
enum Ev {
    Sym(ParsedSymbol),
    Ref {
        to_name: String,
        call_type: &'static str,
        line: usize,
        module: Option<String>,
    },
}

/// A node of the order tree (byte span, parent record).
struct Rec {
    start: u32,
    end: u32,
    parent: u32,
}

struct Frame<'a> {
    kind: AstKind<'a>,
    rec: u32,
    pushed_scope: bool,
    depth: bool,
    fn_depth: bool,
}

struct Walker<'a, 'c> {
    cx: &'c Ctx<'c>,
    recs: Vec<Rec>,
    events: Vec<(u32, Ev)>,
    frames: Vec<Frame<'a>>,
    /// Names of the enclosing scopes (`pushScopeName`).
    scope: Vec<String>,
    /// TypeScript's `funcDepth` (function-like nesting).
    depth: u32,
    /// Function/arrow nesting, for the top-level `await` gate.
    fn_depth: u32,
    /// Open nodes inside an anonymous declaration TypeScript does not descend
    /// into.
    suppressed: u32,
    top_level_await: bool,
    gate: Option<String>,
    /// The last `this` parameter: its parent record and end.
    this_param: Option<(u32, u32)>,
}

/// A bound name: text and UTF-16 start.
struct Name {
    text: String,
    start: usize,
}

impl<'a> Walker<'a, '_> {
    fn parent(&self, up: usize) -> Option<AstKind<'a>> {
        let n = self.frames.len();
        (n > up).then(|| self.frames[n - 1 - up].kind)
    }

    fn is_export(kind: Option<AstKind<'a>>) -> bool {
        matches!(
            kind,
            Some(AstKind::ExportNamedDeclaration(_) | AstKind::ExportDefaultDeclaration(_))
        )
    }

    /// TypeScript `getStart` of a declaration: its first decorator or the
    /// `export` modifier that oxc keeps on the wrapper.
    fn ts_start(&self, start: u32, decorators: &[Decorator], parent: Option<AstKind<'a>>) -> usize {
        let mut s = start;
        if let Some(d) = decorators.first() {
            s = s.min(d.span.start);
        }
        if let Some(p) = parent
            && Self::is_export(Some(p))
        {
            s = s.min(p.span().start);
        }
        self.cx.u(s)
    }

    fn lit(&mut self, s: &StringLiteral) -> String {
        if s.lone_surrogates {
            self.gate
                .get_or_insert_with(|| "string literal with lone surrogates".into());
        }
        s.value.as_str().to_string()
    }

    fn export_name(&mut self, n: &ModuleExportName) -> String {
        match n {
            ModuleExportName::IdentifierName(i) => i.name.as_str().to_string(),
            ModuleExportName::IdentifierReference(i) => i.name.as_str().to_string(),
            ModuleExportName::StringLiteral(s) => self.lit(s),
        }
    }

    /// `declarationNameNodes` for a property name.
    fn key_names(&mut self, key: &PropertyKey, computed: bool) -> Vec<Name> {
        if computed {
            return Vec::new();
        }
        let start = self.cx.u(key.span().start);
        let text = match key {
            PropertyKey::StaticIdentifier(i) => i.name.as_str().to_string(),
            PropertyKey::PrivateIdentifier(i) => format!("#{}", i.name.as_str()),
            PropertyKey::StringLiteral(s) => self.lit(s),
            PropertyKey::NumericLiteral(n) => numeric_name(n.value),
            _ => return Vec::new(),
        };
        vec![Name { text, start }]
    }

    fn pattern_names(&self, p: &BindingPattern, out: &mut Vec<Name>) {
        match p {
            BindingPattern::BindingIdentifier(i) => out.push(Name {
                text: i.name.as_str().to_string(),
                start: self.cx.u(i.span.start),
            }),
            BindingPattern::AssignmentPattern(a) => self.pattern_names(&a.left, out),
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    self.pattern_names(&prop.value, out);
                }
                if let Some(r) = &o.rest {
                    self.pattern_names(&r.argument, out);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for e in a.elements.iter().flatten() {
                    self.pattern_names(e, out);
                }
                if let Some(r) = &a.rest {
                    self.pattern_names(&r.argument, out);
                }
            }
        }
    }

    /// Signature end of a variable/property initialised with a function or
    /// class expression.
    fn init_end(&self, init: Option<&Expression>, node_end: u32) -> usize {
        match init {
            Some(Expression::ArrowFunctionExpression(a)) if !a.expression => {
                self.cx.u(a.body.span.start)
            }
            Some(Expression::FunctionExpression(f)) if f.body.is_some() => self
                .cx
                .u(f.body.as_ref().map_or(node_end, |b| b.span.start)),
            Some(Expression::ClassExpression(c)) => self.cx.u(c.body.span.start) + 1,
            _ => self.cx.u(node_end),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        rec: u32,
        kind: &'static str,
        names: Vec<Name>,
        start: usize,
        end: usize,
        host_start: usize,
        host_end: usize,
        host_exported: bool,
    ) {
        if names.is_empty() {
            return;
        }
        let cx = self.cx;
        let signature = cx.signature(start, end);
        let doc = cx.js_doc(host_start);
        let end_line = cx.line_of(host_end.saturating_sub(1)) + 1;
        let is_exported = mentions_export(&signature) || host_exported;
        let scope = self.scope.join(".");
        let sig = js::string(&signature);
        let doc = js::string(&doc);
        for n in names {
            let line = cx.line_of(n.start);
            let text = [n.text.as_str(), sig.as_str(), doc.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" | ");
            self.events.push((
                rec,
                Ev::Sym(ParsedSymbol {
                    kind,
                    name: n.text,
                    line: line + 1,
                    col: n.start - cx.line_starts[line],
                    signature: sig.clone(),
                    doc_comment: doc.clone(),
                    scope: scope.clone(),
                    text,
                    is_exported: Some(is_exported),
                    end_line: Some((line + 1).max(end_line)),
                }),
            ));
        }
    }

    fn push_ref(&mut self, rec: u32, to_name: String, call_type: &'static str, pos: usize) {
        let line = self.cx.line_of(pos) + 1;
        self.events.push((
            rec,
            Ev::Ref {
                to_name,
                call_type,
                line,
                module: None,
            },
        ));
    }

    fn push_import(&mut self, rec: u32, to_name: String, pos: usize, module: &str) {
        let line = self.cx.line_of(pos) + 1;
        self.events.push((
            rec,
            Ev::Ref {
                to_name,
                call_type: "import",
                line,
                module: Some(module.to_string()),
            },
        ));
    }

    /// A record placed just before the heritage child at `span`: TypeScript
    /// emits heritage refs when it visits the clause, before its types.
    fn heritage(
        &mut self,
        parent: u32,
        first: oxc_span::Span,
        refs: Vec<String>,
        ty: &'static str,
    ) {
        if refs.is_empty() {
            return;
        }
        let kw = self.cx.full_start(self.cx.u(first.start)).saturating_sub(1);
        let rec = self.recs.len() as u32;
        self.recs.push(Rec {
            start: first.start,
            end: first.end,
            parent,
        });
        for r in refs {
            self.push_ref(rec, r, ty, kw);
        }
    }

    /// Whether TypeScript parses this top-level `await` as an await
    /// expression even in a script: it does when the next token is an
    /// identifier, keyword, numeric or string literal on the same line
    /// (`nextTokenIsIdentifierOrKeywordOrLiteralOnSameLine`); otherwise
    /// (`await (x)`, `await -x`, `await` + line break) it is an identifier.
    fn await_parses_alike(&self, a: &oxc_ast::ast::AwaitExpression) -> bool {
        let cx = self.cx;
        let (kw, arg) = (cx.u(a.span.start), cx.u(a.argument.span().start));
        let c = cx.content;
        if c.get(kw..kw + 5) != Some(&js::utf16("await")[..]) || arg < kw + 5 || arg >= c.len() {
            return false;
        }
        if c[kw + 5..arg].iter().any(|&ch| js::is_line_terminator(ch)) {
            return false;
        }
        let ch = c[arg];
        let next = c.get(arg + 1).copied().unwrap_or(0);
        ch == u16::from(b'_')
            || ch == u16::from(b'$')
            || ch == u16::from(b'\\')
            || ch == u16::from(b'\'')
            || ch == u16::from(b'"')
            || (ch < 0x80 && (ch as u8).is_ascii_alphanumeric())
            || (ch == u16::from(b'.') && js::is_digit(next))
            || (ch >= 0x80 && !is_ts_white(ch))
    }

    fn type_name(n: &TSTypeName) -> String {
        match n {
            TSTypeName::IdentifierReference(i) => i.name.as_str().to_string(),
            TSTypeName::QualifiedName(q) => {
                let left = Self::type_name(&q.left);
                if left.is_empty() {
                    String::new()
                } else {
                    format!("{left}.{}", q.right.name.as_str())
                }
            }
            TSTypeName::ThisExpression(_) => String::new(),
        }
    }

    fn class(&mut self, rec: u32, c: &'a Class<'a>, parent: Option<AstKind<'a>>) -> Option<String> {
        let mut push = None;
        if c.r#type == ClassType::ClassDeclaration
            && let Some(id) = &c.id
        {
            let start = self.ts_start(c.span.start, &c.decorators, parent);
            let names = vec![Name {
                text: id.name.as_str().to_string(),
                start: self.cx.u(id.span.start),
            }];
            let end = self.cx.u(c.body.span.start) + 1;
            let host_end = self.cx.u(c.span.end);
            self.emit(
                rec,
                "class",
                names,
                start,
                end,
                start,
                host_end,
                Self::is_export(parent),
            );
            push = Some(id.name.as_str().to_string());
        }
        if let Some(sc) = &c.super_class {
            let refs = match sc {
                Expression::Identifier(i) => vec![i.name.as_str().to_string()],
                _ => Vec::new(),
            };
            self.heritage(rec, sc.span(), refs, "inherit");
        }
        if let Some(first) = c.implements.first() {
            let refs = c
                .implements
                .iter()
                .filter_map(|h| match &h.expression {
                    TSTypeName::IdentifierReference(i) => Some(i.name.as_str().to_string()),
                    _ => None,
                })
                .collect();
            self.heritage(rec, first.span, refs, "implement");
        }
        push
    }

    fn enter(&mut self, kind: AstKind<'a>) -> Frame<'a> {
        let parent = self.parent(0);
        let grand = self.parent(1);
        let span = kind.span();
        let rec = self.recs.len() as u32;
        let parent_rec = self.frames.last().map_or(u32::MAX, |f| f.rec);
        let mut start = span.start;
        match kind {
            AstKind::TSThisParameter(t) => self.this_param = Some((parent_rec, t.span.end)),
            // oxc's parameter list spans the `this` parameter it does not
            // contain; TypeScript visits `this` first.
            AstKind::FormalParameters(_) => {
                if let Some((p, end)) = self.this_param.take()
                    && p == parent_rec
                {
                    start = start.max(end);
                }
            }
            _ => {}
        }
        self.recs.push(Rec {
            start,
            end: span.end,
            parent: parent_rec,
        });
        let mut push: Option<String> = None;
        let mut depth = false;
        let mut fn_depth = false;
        let cx = self.cx;
        match kind {
            AstKind::Class(c) => push = self.class(rec, c, parent),
            AstKind::Function(f) => {
                depth = true;
                fn_depth = true;
                if matches!(
                    f.r#type,
                    FunctionType::FunctionDeclaration | FunctionType::TSDeclareFunction
                ) && let Some(id) = &f.id
                {
                    let start = self.ts_start(f.span.start, &[], parent);
                    let end = f
                        .body
                        .as_ref()
                        .map_or(cx.u(f.span.end), |b| cx.u(b.span.start));
                    let names = vec![Name {
                        text: id.name.as_str().to_string(),
                        start: cx.u(id.span.start),
                    }];
                    let host_end = cx.u(f.span.end);
                    self.emit(
                        rec,
                        "function",
                        names,
                        start,
                        end,
                        start,
                        host_end,
                        Self::is_export(parent),
                    );
                    push = Some(id.name.as_str().to_string());
                }
            }
            AstKind::ArrowFunctionExpression(_) => {
                depth = true;
                fn_depth = true;
            }
            AstKind::TSInterfaceDeclaration(i) => {
                let start = self.ts_start(i.span.start, &[], parent);
                let names = vec![Name {
                    text: i.id.name.as_str().to_string(),
                    start: cx.u(i.id.span.start),
                }];
                let end = cx.u(i.span.end);
                self.emit(
                    rec,
                    "interface",
                    names,
                    start,
                    end,
                    start,
                    end,
                    Self::is_export(parent),
                );
                push = Some(i.id.name.as_str().to_string());
                if let Some(first) = i.extends.first() {
                    let refs = i
                        .extends
                        .iter()
                        .filter_map(|h| match &h.expression {
                            Expression::Identifier(id) => Some(id.name.as_str().to_string()),
                            _ => None,
                        })
                        .collect();
                    self.heritage(rec, first.span, refs, "inherit");
                }
            }
            AstKind::TSEnumDeclaration(e) => {
                let start = self.ts_start(e.span.start, &[], parent);
                let names = vec![Name {
                    text: e.id.name.as_str().to_string(),
                    start: cx.u(e.id.span.start),
                }];
                let end = cx.u(e.span.end);
                self.emit(
                    rec,
                    "enum",
                    names,
                    start,
                    end,
                    start,
                    end,
                    Self::is_export(parent),
                );
                push = Some(e.id.name.as_str().to_string());
            }
            AstKind::TSTypeAliasDeclaration(t) => {
                let start = self.ts_start(t.span.start, &[], parent);
                let names = vec![Name {
                    text: t.id.name.as_str().to_string(),
                    start: cx.u(t.id.span.start),
                }];
                let end = cx.u(t.span.end);
                self.emit(
                    rec,
                    "type",
                    names,
                    start,
                    end,
                    start,
                    end,
                    Self::is_export(parent),
                );
                push = Some(t.id.name.as_str().to_string());
            }
            AstKind::TSNamespaceExportDeclaration(n) => {
                let start = cx.u(n.span.start);
                let end = cx.u(n.span.end);
                let names = vec![Name {
                    text: n.id.name.as_str().to_string(),
                    start: cx.u(n.id.span.start),
                }];
                self.emit(rec, "namespace", names, start, end, start, end, false);
            }
            AstKind::TSModuleDeclaration(m) => {
                // `namespace A.B {}` nests one declaration per segment.
                let nested = matches!(parent, Some(AstKind::TSModuleDeclaration(_)));
                let (text, id_start) = match &m.id {
                    TSModuleDeclarationName::Identifier(i) => {
                        (i.name.as_str().to_string(), i.span.start)
                    }
                    TSModuleDeclarationName::StringLiteral(s) => (self.lit(s), s.span.start),
                };
                let start = if nested {
                    cx.u(id_start)
                } else {
                    self.ts_start(m.span.start, &[], parent)
                };
                let end = match &m.body {
                    Some(TSModuleDeclarationBody::TSModuleBlock(b)) => cx.u(b.span.start) + 1,
                    _ => cx.u(m.span.end),
                };
                let names = vec![Name {
                    text,
                    start: cx.u(id_start),
                }];
                let host_end = cx.u(m.span.end);
                let exported = !nested && Self::is_export(parent);
                self.emit(
                    rec,
                    "namespace",
                    names,
                    start,
                    end,
                    start,
                    host_end,
                    exported,
                );
            }
            AstKind::TSGlobalDeclaration(g) => {
                let start = self.ts_start(g.span.start, &[], parent);
                let names = vec![Name {
                    text: "global".into(),
                    start: cx.u(g.global_span.start),
                }];
                let end = cx.u(g.body.span.start) + 1;
                let host_end = cx.u(g.span.end);
                self.emit(
                    rec,
                    "namespace",
                    names,
                    start,
                    end,
                    start,
                    host_end,
                    Self::is_export(parent),
                );
            }
            AstKind::MethodDefinition(md) => {
                depth = true;
                if md.kind != MethodDefinitionKind::Constructor {
                    let k = if matches!(
                        md.kind,
                        MethodDefinitionKind::Get | MethodDefinitionKind::Set
                    ) {
                        "property"
                    } else {
                        "method"
                    };
                    let names = self.key_names(&md.key, md.computed);
                    let start = self.ts_start(md.span.start, &md.decorators, None);
                    let end = md
                        .value
                        .body
                        .as_ref()
                        .map_or(cx.u(md.span.end), |b| cx.u(b.span.start));
                    self.emit(rec, k, names, start, end, start, cx.u(md.span.end), false);
                    if let (PropertyKey::StaticIdentifier(i), false) = (&md.key, md.computed) {
                        push = Some(i.name.as_str().to_string());
                    }
                }
            }
            AstKind::ObjectProperty(p) => {
                if p.method || p.kind != PropertyKind::Init {
                    depth = true;
                    let k = if p.kind == PropertyKind::Init {
                        "method"
                    } else {
                        "property"
                    };
                    let names = self.key_names(&p.key, p.computed);
                    let start = cx.u(p.span.start);
                    let end = match &p.value {
                        Expression::FunctionExpression(f) => f
                            .body
                            .as_ref()
                            .map_or(cx.u(p.span.end), |b| cx.u(b.span.start)),
                        _ => cx.u(p.span.end),
                    };
                    self.emit(rec, k, names, start, end, start, cx.u(p.span.end), false);
                    if let (PropertyKey::StaticIdentifier(i), false) = (&p.key, p.computed) {
                        push = Some(i.name.as_str().to_string());
                    }
                }
            }
            AstKind::TSMethodSignature(ms) => {
                depth = true;
                if ms.kind != TSMethodSignatureKind::Method {
                    let names = self.key_names(&ms.key, ms.computed);
                    let start = cx.u(ms.span.start);
                    let end = cx.u(ms.span.end);
                    self.emit(rec, "property", names, start, end, start, end, false);
                    if let (PropertyKey::StaticIdentifier(i), false) = (&ms.key, ms.computed) {
                        push = Some(i.name.as_str().to_string());
                    }
                }
            }
            AstKind::PropertyDefinition(pd) => {
                let names = self.key_names(&pd.key, pd.computed);
                let start = self.ts_start(pd.span.start, &pd.decorators, None);
                let end = self.init_end(pd.value.as_ref(), pd.span.end);
                self.emit(
                    rec,
                    "property",
                    names,
                    start,
                    end,
                    start,
                    cx.u(pd.span.end),
                    false,
                );
                if let (PropertyKey::StaticIdentifier(i), false) = (&pd.key, pd.computed) {
                    push = Some(i.name.as_str().to_string());
                }
            }
            AstKind::AccessorProperty(ap) => {
                let names = self.key_names(&ap.key, ap.computed);
                let start = self.ts_start(ap.span.start, &ap.decorators, None);
                let end = self.init_end(ap.value.as_ref(), ap.span.end);
                self.emit(
                    rec,
                    "property",
                    names,
                    start,
                    end,
                    start,
                    cx.u(ap.span.end),
                    false,
                );
                if let (PropertyKey::StaticIdentifier(i), false) = (&ap.key, ap.computed) {
                    push = Some(i.name.as_str().to_string());
                }
            }
            AstKind::VariableDeclarator(d) => {
                if self.depth == 0
                    && let Some(AstKind::VariableDeclaration(decl)) = parent
                {
                    let k = match decl.kind {
                        VariableDeclarationKind::Let => "let",
                        VariableDeclarationKind::Const | VariableDeclarationKind::AwaitUsing => {
                            "const"
                        }
                        VariableDeclarationKind::Var | VariableDeclarationKind::Using => "var",
                    };
                    let mut names = Vec::new();
                    self.pattern_names(&d.id, &mut names);
                    let start = cx.u(d.span.start);
                    let end = self.init_end(d.init.as_ref(), d.span.end);
                    let for_head = match grand {
                        Some(AstKind::ForStatement(f)) => matches!(
                            &f.init,
                            Some(ForStatementInit::VariableDeclaration(v)) if std::ptr::eq(&**v, decl)
                        ),
                        Some(AstKind::ForInStatement(f)) => matches!(
                            &f.left,
                            ForStatementLeft::VariableDeclaration(v) if std::ptr::eq(&**v, decl)
                        ),
                        Some(AstKind::ForOfStatement(f)) => matches!(
                            &f.left,
                            ForStatementLeft::VariableDeclaration(v) if std::ptr::eq(&**v, decl)
                        ),
                        _ => false,
                    };
                    let first = decl
                        .declarations
                        .first()
                        .is_some_and(|f| std::ptr::eq(f, d));
                    let (hs, he, hx) = if !for_head && first {
                        (
                            self.ts_start(decl.span.start, &[], grand),
                            cx.u(decl.span.end),
                            Self::is_export(grand),
                        )
                    } else {
                        (start, cx.u(d.span.end), false)
                    };
                    self.emit(rec, k, names, start, end, hs, he, hx);
                }
            }
            AstKind::AwaitExpression(a) if self.fn_depth == 0 && !self.await_parses_alike(a) => {
                self.top_level_await = true;
            }
            AstKind::ForOfStatement(f) if f.r#await && self.fn_depth == 0 => {
                self.top_level_await = true;
            }
            AstKind::VariableDeclaration(v)
                if v.kind == VariableDeclarationKind::AwaitUsing && self.fn_depth == 0 =>
            {
                self.top_level_await = true;
            }
            AstKind::CallExpression(c) => {
                if let Expression::Identifier(i) = &c.callee {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(c.span.start));
                }
            }
            AstKind::NewExpression(n) => {
                if let Expression::Identifier(i) = &n.callee {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(n.span.start));
                }
            }
            AstKind::StaticMemberExpression(m) => {
                if let Expression::Identifier(i) = &m.object {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(m.span.start));
                }
            }
            AstKind::PrivateFieldExpression(m) => {
                if let Expression::Identifier(i) = &m.object {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(m.span.start));
                }
            }
            AstKind::JSXMemberExpression(j) => {
                if let JSXMemberExpressionObject::IdentifierReference(i) = &j.object {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(j.span.start));
                }
            }
            AstKind::TSTypeReference(t) => {
                let n = Self::type_name(&t.type_name);
                if !n.is_empty() {
                    self.push_ref(rec, n, "type_ref", cx.u(t.span.start));
                }
            }
            AstKind::TSQualifiedName(q) => {
                // `implements A.B`: TypeScript holds a property access there,
                // whose innermost `A` is a `call` ref like any `A.b`.
                let mut up = 0;
                while matches!(self.parent(up), Some(AstKind::TSQualifiedName(_))) {
                    up += 1;
                }
                if matches!(self.parent(up), Some(AstKind::TSClassImplements(_)))
                    && let TSTypeName::IdentifierReference(i) = &q.left
                {
                    self.push_ref(rec, i.name.as_str().to_string(), "call", cx.u(q.span.start));
                }
            }
            AstKind::ImportDeclaration(d) => {
                let module = self.lit(&d.source);
                let pos = cx.u(d.span.start);
                let specs = d.specifiers.as_ref().map_or(&[][..], |s| &s[..]);
                let head = &cx.content[pos..cx.u(d.source.span.start)];
                if specs.is_empty() && !head.contains(&0x7B) {
                    self.push_import(rec, module.clone(), pos, &module);
                }
                for s in specs {
                    let name = match s {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            self.export_name(&s.imported)
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            s.local.name.as_str().to_string()
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            s.local.name.as_str().to_string()
                        }
                    };
                    self.push_import(rec, name, pos, &module);
                }
            }
            AstKind::ExportAllDeclaration(d) => {
                let module = self.lit(&d.source);
                let pos = cx.u(d.span.start);
                let name = match &d.exported {
                    Some(n) => self.export_name(n),
                    None => module.clone(),
                };
                self.push_import(rec, name, pos, &module);
            }
            AstKind::ExportNamedDeclaration(d) => {
                if let Some(src) = &d.source {
                    let module = self.lit(src);
                    let pos = cx.u(d.span.start);
                    for s in &d.specifiers {
                        let name = self.export_name(&s.local);
                        self.push_import(rec, name, pos, &module);
                    }
                }
            }
            _ => {}
        }
        let pushed_scope = if let Some(n) = push {
            self.scope.push(n);
            true
        } else {
            false
        };
        if depth {
            self.depth += 1;
        }
        if fn_depth {
            self.fn_depth += 1;
        }
        Frame {
            kind,
            rec,
            pushed_scope,
            depth,
            fn_depth,
        }
    }

    /// Pre-order over the record tree with every node's children in
    /// `(start, longest first, creation)` order, then `deduplicateRefs`.
    fn finish(self, lang: &'static str) -> Parsed {
        let n = self.recs.len();
        let mut kids: Vec<u32> = (0..n as u32)
            .filter(|&i| self.recs[i as usize].parent != u32::MAX)
            .collect();
        kids.sort_by_key(|&i| {
            let r = &self.recs[i as usize];
            (r.parent, r.start, std::cmp::Reverse(r.end), i)
        });
        let mut first = vec![0usize; n + 1];
        for &k in &kids {
            first[self.recs[k as usize].parent as usize + 1] += 1;
        }
        for i in 0..n {
            first[i + 1] += first[i];
        }
        let mut rank = vec![0u32; n];
        let mut next = 0u32;
        let mut stack: Vec<u32> = (0..n as u32)
            .filter(|&i| self.recs[i as usize].parent == u32::MAX)
            .rev()
            .collect();
        while let Some(r) = stack.pop() {
            rank[r as usize] = next;
            next += 1;
            let (a, b) = (first[r as usize], first[r as usize + 1]);
            stack.extend(kids[a..b].iter().rev());
        }
        let mut events = self.events;
        events.sort_by_key(|(r, _)| rank[*r as usize]); // stable
        let mut symbols = Vec::new();
        let mut refs: Vec<ParsedRef> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (_, ev) in events {
            match ev {
                Ev::Sym(s) => symbols.push(s),
                Ev::Ref {
                    to_name,
                    call_type,
                    line,
                    module,
                } => {
                    let key = (
                        to_name.clone(),
                        call_type,
                        line,
                        module.clone().unwrap_or_default(),
                    );
                    if seen.insert(key) {
                        refs.push(ParsedRef {
                            to_name,
                            call_type,
                            line,
                            lang,
                            module,
                        });
                    }
                }
            }
        }
        Parsed {
            symbols,
            unsupported: None,
            refs,
        }
    }
}

impl<'a> Visit<'a> for Walker<'a, '_> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        if self.suppressed > 0 {
            self.suppressed += 1;
            return;
        }
        // An anonymous class/function declaration (`export default class {}`)
        // has no name: TypeScript indexes nothing in it and does not descend.
        let anonymous = match kind {
            AstKind::Class(c) => c.r#type == ClassType::ClassDeclaration && c.id.is_none(),
            AstKind::Function(f) => {
                matches!(
                    f.r#type,
                    FunctionType::FunctionDeclaration | FunctionType::TSDeclareFunction
                ) && f.id.is_none()
            }
            _ => false,
        };
        if anonymous {
            self.suppressed = 1;
            return;
        }
        let frame = self.enter(kind);
        self.frames.push(frame);
    }

    fn leave_node(&mut self, _kind: AstKind<'a>) {
        if self.suppressed > 0 {
            self.suppressed -= 1;
            return;
        }
        let f = self.frames.pop().expect("balanced enter/leave");
        if f.pushed_scope {
            self.scope.pop();
        }
        if f.depth {
            self.depth -= 1;
        }
        if f.fn_depth {
            self.fn_depth -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_ecmascript() {
        assert_eq!(js_number(16.0), "16");
        assert_eq!(js_number(1000.0), "1000");
        assert_eq!(js_number(0.5), "0.5");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(1e-7), "1e-7");
        assert_eq!(js_number(0.000001), "0.000001");
        assert_eq!(js_number(1.2345678901234568e29), "1.2345678901234568e+29");
        assert_eq!(js_number(123.456), "123.456");
        assert_eq!(
            numeric_name(1.2345678901234568e29),
            "1.2345678901234568e+29"
        );
    }

    #[test]
    fn syntax_errors_are_refused() {
        let p = parse("C:\\x\\a.ts", "ts", &js::utf16("export class {"));
        assert!(p.unsupported.is_some());
        let p = parse("C:\\x\\a.ts", "ts", &js::utf16("await (x);\n"));
        assert!(p.unsupported.unwrap().contains("top-level await"));
        let p = parse("C:\\x\\a.ts", "ts", &js::utf16("await\nf();\n"));
        assert!(p.unsupported.unwrap().contains("top-level await"));
        let p = parse("C:\\x\\a.ts", "ts", &js::utf16("await -x;\n"));
        assert!(p.unsupported.is_some());
        // An await expression in a script too: the same tree.
        let p = parse(
            "C:\\x\\a.mjs",
            "js",
            &js::utf16("await main();\nawait 'x';\n"),
        );
        assert!(p.unsupported.is_none());
        assert_eq!(p.refs.len(), 1);
        let p = parse("C:\\x\\a.ts", "ts", &js::utf16("export {};\nawait (x);\n"));
        assert!(p.unsupported.is_none());
    }
}
