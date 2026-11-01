//! Port of WrongStack's root-`.gitignore` matcher (codebase-index/gitignore.ts)
//! and the glob NFA it uses (core/utils/glob-match.ts, path mode).
//!
//! Positions are counted in `char`s; WrongStack counts UTF-16 code units, so a
//! `?` next to an astral character can differ. All other behaviour is checked
//! against the real implementation by `scripts/oracle/discovery-diff.ts`.

#[derive(Debug, Clone)]
enum Token {
    Literal(char),
    /// `?`: one character except `/`.
    One,
    /// `*`: any run of characters except `/`.
    Star,
    /// `**`: any run of characters except line terminators.
    Globstar,
    Class {
        negated: bool,
        items: Vec<(char, char)>,
    },
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

#[derive(Debug, Clone)]
pub struct Glob {
    tokens: Vec<Token>,
}

impl Glob {
    pub fn compile(pattern: &str) -> Glob {
        let chars: Vec<char> = pattern.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    if chars.get(i + 1) == Some(&'*') {
                        tokens.push(Token::Globstar);
                        i += 2;
                        if chars.get(i) == Some(&'/') {
                            i += 1;
                        }
                    } else {
                        tokens.push(Token::Star);
                        i += 1;
                    }
                }
                '?' => {
                    tokens.push(Token::One);
                    i += 1;
                }
                '[' => {
                    // JS: indexOf(']', i + 1) — no closing bracket => literal '['.
                    let Some(close) = chars[i + 1..].iter().position(|&c| c == ']') else {
                        tokens.push(Token::Literal('['));
                        i += 1;
                        continue;
                    };
                    let _ = close;
                    i += 1;
                    let mut negated = false;
                    if matches!(chars.get(i), Some('!') | Some('^')) {
                        negated = true;
                        i += 1;
                    }
                    let mut members: Vec<char> = Vec::new();
                    let mut first = true;
                    while i < chars.len() && (chars[i] != ']' || first) {
                        members.push(chars[i]);
                        first = false;
                        i += 1;
                    }
                    i += 1; // closing ']'
                    let mut items = Vec::new();
                    let mut k = 0;
                    while k < members.len() {
                        if k + 2 < members.len() && members[k + 1] == '-' {
                            items.push((members[k], members[k + 2]));
                            k += 3;
                        } else {
                            items.push((members[k], members[k]));
                            k += 1;
                        }
                    }
                    tokens.push(Token::Class { negated, items });
                }
                c => {
                    tokens.push(Token::Literal(c));
                    i += 1;
                }
            }
        }
        Glob { tokens }
    }

    fn step_ok(token: &Token, c: char) -> bool {
        match token {
            Token::Literal(l) => *l == c,
            Token::One => c != '/',
            Token::Star => c != '/',
            Token::Globstar => !is_line_terminator(c),
            Token::Class { negated, items } => {
                let hit = items.iter().any(|(a, b)| *a <= c && c <= *b);
                hit != *negated
            }
        }
    }

    /// Epsilon closure: Star/Globstar may match zero characters.
    fn close(&self, states: &mut [bool]) {
        for s in 0..self.tokens.len() {
            if states[s] && matches!(self.tokens[s], Token::Star | Token::Globstar) {
                states[s + 1] = true;
            }
        }
    }

    /// Smallest index `k` with `input[k] == '/'` at which some span starting
    /// at an `is_start` index fully matches the glob. One linear NFA pass.
    pub fn first_slash_end(
        &self,
        input: &[char],
        is_start: &dyn Fn(usize, &[char]) -> bool,
    ) -> Option<usize> {
        let n = self.tokens.len();
        let mut cur = vec![false; n + 1];
        for i in 0..=input.len() {
            if is_start(i, input) {
                cur[0] = true;
            }
            self.close(&mut cur);
            if i == input.len() {
                return None;
            }
            if cur[n] && input[i] == '/' {
                return Some(i);
            }
            let c = input[i];
            let mut next = vec![false; n + 1];
            for s in 0..n {
                if cur[s] && Self::step_ok(&self.tokens[s], c) {
                    match self.tokens[s] {
                        Token::Star | Token::Globstar => next[s] = true,
                        _ => next[s + 1] = true,
                    }
                }
            }
            cur = next;
        }
        None
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// `testSpan`: some start index accepted by `is_start` and end index
    /// accepted by `is_end` bound a full match of the glob.
    pub fn test_span(
        &self,
        input: &[char],
        is_start: &dyn Fn(usize, &[char]) -> bool,
        is_end: &dyn Fn(usize, &[char]) -> bool,
    ) -> bool {
        let n = self.tokens.len();
        let mut cur = vec![false; n + 1];
        for i in 0..=input.len() {
            if is_start(i, input) {
                cur[0] = true;
            }
            self.close(&mut cur);
            if cur[n] && is_end(i, input) {
                return true;
            }
            if i == input.len() {
                break;
            }
            let c = input[i];
            let mut next = vec![false; n + 1];
            let mut any = false;
            for s in 0..n {
                if !cur[s] {
                    continue;
                }
                let t = &self.tokens[s];
                if Self::step_ok(t, c) {
                    match t {
                        Token::Star | Token::Globstar => next[s] = true,
                        _ => next[s + 1] = true,
                    }
                    any = true;
                }
            }
            cur = next;
            let _ = any;
        }
        false
    }
}

struct Rule {
    parts: Vec<Glob>,
    anchored: bool,
    negated: bool,
    dir_only: bool,
}

fn start_anchored(i: usize, _: &[char]) -> bool {
    i == 0
}
fn start_any_segment(i: usize, input: &[char]) -> bool {
    i == 0 || input[i - 1] == '/'
}
fn end_eq_or_under(i: usize, input: &[char]) -> bool {
    i == input.len() || input[i] == '/'
}
fn end_under(i: usize, input: &[char]) -> bool {
    i < input.len() && input[i] == '/'
}
fn end_exact(i: usize, input: &[char]) -> bool {
    i == input.len()
}

fn match_parts(
    parts: &[Glob],
    p: &[char],
    is_start: &dyn Fn(usize, &[char]) -> bool,
    is_end: &dyn Fn(usize, &[char]) -> bool,
) -> bool {
    // Equivalent to WrongStack's recursive search, in polynomial time. Every
    // middle part ends at a '/' index k, and the next part may start at any
    // i > k preceded by '/'. That start set shrinks as k grows, so the
    // smallest reachable k dominates all others: track only it. A hostile
    // `.gitignore` with many `/**/` parts can no longer cause exponential work.
    let Some((last, init)) = parts.split_last() else {
        return false;
    };
    let mut prev_end: Option<usize> = None;
    for part in init {
        // One linear pass per part: the first '/' where the part can end.
        let next = match prev_end {
            None => part.first_slash_end(p, is_start),
            Some(after) => {
                let start = move |i: usize, input: &[char]| i > after && input[i - 1] == '/';
                part.first_slash_end(p, &start)
            }
        };
        match next {
            Some(k) => prev_end = Some(k),
            None => return false,
        }
    }
    match prev_end {
        None => last.test_span(p, is_start, is_end),
        Some(after) => {
            let start = move |i: usize, input: &[char]| i > after && input[i - 1] == '/';
            last.test_span(p, &start, is_end)
        }
    }
}

/// Resource caps for an untrusted `.gitignore`. WrongStack's glob compiler
/// rejects parts over 1024 characters (that throws there); here such a rule
/// is dropped. The rule/part caps bound total matching work.
const MAX_RULES: usize = 10_000;
const MAX_PARTS_PER_RULE: usize = 64;
const MAX_PART_CHARS: usize = 1024;
/// Total compiled glob tokens across all rules (bounds per-path work).
const MAX_TOTAL_TOKENS: usize = 200_000;

/// Compiled root `.gitignore` (WrongStack only reads the project root file).
pub struct Gitignore {
    rules: Vec<Rule>,
    has_negation: bool,
}

impl Gitignore {
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            has_negation: false,
        }
    }

    /// `compileGitignore(raw.split('\n'))`.
    pub fn compile(raw: &str) -> Self {
        let mut rules = Vec::new();
        let mut total_tokens = 0usize;
        for raw_line in raw.split('\n') {
            let mut line = raw_line.strip_suffix('\r').unwrap_or(raw_line).to_string();
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            line = line.trim_end().to_string();
            let mut negated = false;
            if let Some(rest) = line.strip_prefix('!') {
                negated = true;
                line = rest.to_string();
            } else if line.starts_with("\\#") || line.starts_with("\\!") {
                line = line[1..].to_string();
            }
            let mut dir_only = false;
            if let Some(rest) = line.strip_suffix('/') {
                dir_only = true;
                line = rest.to_string();
            }
            if line.is_empty() {
                continue;
            }
            let mut anchored = line.starts_with('/') || line.contains('/');
            if let Some(rest) = line.strip_prefix('/') {
                line = rest.to_string();
            }
            if line.starts_with("**/") {
                while let Some(rest) = line.strip_prefix("**/") {
                    line = rest.to_string();
                }
                anchored = false;
            }
            if line.is_empty() {
                continue;
            }
            // `/**/**/` is one "zero or more directories".
            let collapsed = collapse_globstar_dirs(&line);
            let raw_parts: Vec<&str> = collapsed.split("/**/").collect();
            if raw_parts.len() > MAX_PARTS_PER_RULE
                || raw_parts.iter().any(|p| p.chars().count() > MAX_PART_CHARS)
            {
                continue;
            }
            if rules.len() >= MAX_RULES {
                break;
            }
            let parts: Vec<Glob> = raw_parts.into_iter().map(Glob::compile).collect();
            total_tokens += parts.iter().map(Glob::token_count).sum::<usize>();
            if total_tokens > MAX_TOTAL_TOKENS {
                break;
            }
            rules.push(Rule {
                parts,
                anchored,
                negated,
                dir_only,
            });
        }
        let has_negation = rules.iter().any(|r| r.negated);
        Self {
            rules,
            has_negation,
        }
    }

    pub fn load(project_root: &std::path::Path) -> Self {
        match std::fs::read(project_root.join(".gitignore")) {
            Ok(bytes) => Self::compile(&String::from_utf8_lossy(&bytes)),
            Err(_) => Self::empty(),
        }
    }

    fn evaluate(&self, p: &[char], is_dir: bool) -> bool {
        let mut ignored = false;
        for r in &self.rules {
            if self.has_negation && r.dir_only && !is_dir {
                continue;
            }
            let is_end: &dyn Fn(usize, &[char]) -> bool = if self.has_negation {
                &end_exact
            } else if r.dir_only && !is_dir {
                &end_under
            } else {
                &end_eq_or_under
            };
            let is_start: &dyn Fn(usize, &[char]) -> bool = if r.anchored {
                &start_anchored
            } else {
                &start_any_segment
            };
            if match_parts(&r.parts, p, is_start, is_end) {
                ignored = !r.negated;
                if !self.has_negation && ignored {
                    return true;
                }
            }
        }
        ignored
    }

    /// Whether `rel_path` (project-relative) is ignored.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let mut p = rel_path.replace('\\', "/");
        if let Some(rest) = p.strip_prefix("./") {
            p = rest.to_string();
        }
        let p = p.trim_start_matches('/');
        let chars: Vec<char> = p.chars().collect();
        if self.has_negation {
            for k in (0..chars.len()).filter(|&k| chars[k] == '/') {
                if self.evaluate(&chars[..k], true) {
                    return true;
                }
            }
        }
        self.evaluate(&chars, is_dir)
    }
}

/// JS `line.replace(/(?:\/\*\*)+\//g, '/**/')`.
fn collapse_globstar_dirs(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(pos) = rest.find("/**") {
        out.push_str(&rest[..pos]);
        let mut tail = &rest[pos..];
        let mut count = 0;
        while let Some(t) = tail.strip_prefix("/**") {
            tail = t;
            count += 1;
        }
        if count > 0 && tail.starts_with('/') {
            out.push_str("/**/");
            rest = &tail[1..];
        } else {
            out.push_str(&rest[pos..pos + 3]);
            rest = &rest[pos + 3..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gi(src: &str) -> Gitignore {
        Gitignore::compile(src)
    }

    #[test]
    fn bare_names_match_at_any_depth() {
        let g = gi("node_modules\n*.log\n");
        assert!(g.is_ignored("node_modules", true));
        assert!(g.is_ignored("a/node_modules/x.js", false));
        assert!(g.is_ignored("deep/err.log", false));
        assert!(!g.is_ignored("logs/x.ts", false));
    }

    #[test]
    fn anchored_and_dir_only() {
        let g = gi("/build\nout/\ndocs/*.md\n");
        assert!(g.is_ignored("build/a.ts", false));
        assert!(!g.is_ignored("src/build/a.ts", false));
        assert!(g.is_ignored("out", true));
        assert!(!g.is_ignored("out", false));
        assert!(g.is_ignored("out/x.ts", false));
        assert!(g.is_ignored("docs/a.md", false));
        assert!(!g.is_ignored("docs/sub/a.md", false));
    }

    #[test]
    fn globstar_and_negation() {
        let g = gi("**/gen/**\n!keep.ts\n");
        assert!(g.is_ignored("a/gen/x.ts", false));
        let g = gi("*.ts\n!keep.ts\n");
        assert!(g.is_ignored("a.ts", false));
        assert!(!g.is_ignored("keep.ts", false));
        let g = gi("vendor/\n!vendor/keep.ts\n");
        assert!(
            g.is_ignored("vendor/keep.ts", false),
            "parent excluded wins"
        );
    }

    #[test]
    fn classes_and_escapes() {
        let g = gi("file[0-9].ts\n[!a]x.js\n\\#hash\n");
        assert!(g.is_ignored("file3.ts", false));
        assert!(!g.is_ignored("filea.ts", false));
        assert!(g.is_ignored("bx.js", false));
        assert!(!g.is_ignored("ax.js", false));
        assert!(g.is_ignored("#hash", false));
    }

    #[test]
    fn hostile_globstar_chain_is_polynomial() {
        let pattern = format!("{}x", "a/**/".repeat(40));
        let path = format!("{}y", "a/".repeat(400));
        let g = gi(&pattern);
        let started = std::time::Instant::now();
        assert!(!g.is_ignored(&path, false));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let g = gi("a/**/b/**/c\n");
        assert!(g.is_ignored("a/x/b/y/z/c", false));
        assert!(g.is_ignored("a/b/c", false));
        assert!(!g.is_ignored("a/c/b", false));
    }

    #[test]
    fn collapse() {
        assert_eq!(collapse_globstar_dirs("a/**/**/b"), "a/**/b");
        assert_eq!(collapse_globstar_dirs("a/**"), "a/**");
    }
}
