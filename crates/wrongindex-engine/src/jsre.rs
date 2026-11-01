//! A small ECMAScript regular-expression engine over UTF-16 code units.
//!
//! WrongStack's regex extractors (`generic-parser.ts`, `infra-parser.ts`,
//! `import-extractor.ts`) are reproduced here from their literal source text
//! instead of hand-written matchers: the engine implements the ECMAScript
//! (Annex B, non-`u`) backtracking semantics for the subset those patterns
//! use, so the leftmost match, the greedy/lazy choice at every quantifier and
//! the capture spans are the ones V8 reports.
//!
//! Supported: literals, `.`, `^`/`$` (multiline when `m`), `\b`/`\B`,
//! `\d \D \s \S \w \W`, character classes with ranges and escapes, groups
//! (capturing and `(?:`), alternation, `* + ? {n} {n,} {n,m}` (greedy and
//! lazy), lookahead `(?= (?!` and fixed-width lookbehind `(?<= (?<!`, flags
//! `g` (ignored: callers choose `exec` or `matchAll`), `i` and `m`.
//!
//! Semantics follow the spec for these constructs: `\s` is the ECMAScript
//! WhiteSpace + LineTerminator set, `\w`/`\b` are ASCII, `.` excludes the
//! four line terminators, and `i` without `u` folds through `toUpperCase`
//! never mapping a non-ASCII unit onto ASCII - for the ASCII-only patterns
//! used here that is exactly ASCII case folding. Anything outside the subset
//! (backreferences, the `u`/`y`/`s` flags, captures inside lookarounds,
//! quantified sub-patterns that can match empty) panics when the pattern is
//! compiled, so an unsupported pattern can never silently diverge.

use crate::js;

#[derive(Debug, Clone)]
enum ClassItem {
    Range(u16, u16),
    Space(bool),
    Word(bool),
    Digit(bool),
}

#[derive(Debug, Clone)]
struct Class {
    items: Vec<ClassItem>,
    negate: bool,
}

impl Class {
    fn has(&self, c: u16) -> bool {
        self.items.iter().any(|it| match *it {
            ClassItem::Range(a, b) => a <= c && c <= b,
            ClassItem::Space(pos) => js::is_space(c) == pos,
            ClassItem::Word(pos) => js::is_word(c) == pos,
            ClassItem::Digit(pos) => js::is_digit(c) == pos,
        })
    }

    fn matches(&self, c: u16, icase: bool) -> bool {
        let found = self.has(c) || (icase && other_case(c).is_some_and(|o| self.has(o)));
        found != self.negate
    }
}

/// The other ASCII case of a letter.
fn other_case(c: u16) -> Option<u16> {
    match c {
        0x41..=0x5A => Some(c + 32),
        0x61..=0x7A => Some(c - 32),
        _ => None,
    }
}

#[derive(Debug, Clone)]
enum Node {
    Empty,
    Char(u16),
    Any,
    Class(Class),
    Start,
    End,
    WordBoundary(bool),
    Group(Box<Node>, Option<usize>),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
    Look {
        node: Box<Node>,
        ahead: bool,
        negate: bool,
    },
}

impl Node {
    /// Minimum and (if bounded) maximum width in code units.
    fn width(&self) -> (usize, Option<usize>) {
        match self {
            Node::Empty | Node::Start | Node::End | Node::WordBoundary(_) | Node::Look { .. } => {
                (0, Some(0))
            }
            Node::Char(_) | Node::Any | Node::Class(_) => (1, Some(1)),
            Node::Group(n, _) => n.width(),
            Node::Concat(v) => v.iter().fold((0, Some(0)), |(a, b), n| {
                let (c, d) = n.width();
                (a + c, b.zip(d).map(|(x, y)| x + y))
            }),
            Node::Alt(v) => {
                let ws: Vec<_> = v.iter().map(Node::width).collect();
                let min = ws.iter().map(|w| w.0).min().unwrap_or(0);
                let max = ws.iter().try_fold(0usize, |m, w| w.1.map(|x| m.max(x)));
                (min, max)
            }
            Node::Repeat { node, min, max, .. } => {
                let (a, b) = node.width();
                (
                    a * *min as usize,
                    max.and_then(|m| b.map(|x| x * m as usize)),
                )
            }
        }
    }

    fn has_capture(&self) -> bool {
        match self {
            Node::Group(n, c) => c.is_some() || n.has_capture(),
            Node::Concat(v) | Node::Alt(v) => v.iter().any(Node::has_capture),
            Node::Repeat { node, .. } | Node::Look { node, .. } => node.has_capture(),
            _ => false,
        }
    }
}

/// The units a match of `node` can start with, when that set is small and
/// known; `None` when any unit (or an empty match) is possible.
fn first_units(node: &Node, icase: bool) -> Option<Vec<u16>> {
    match node {
        Node::Char(c) => {
            let mut v = vec![*c];
            if icase && let Some(o) = other_case(*c) {
                v.push(o);
            }
            Some(v)
        }
        Node::Class(c) if !c.negate => {
            let mut v = Vec::new();
            for it in &c.items {
                let ClassItem::Range(a, b) = *it else {
                    return None;
                };
                if b - a > 64 {
                    return None;
                }
                for u in a..=b {
                    v.push(u);
                    if icase && let Some(o) = other_case(u) {
                        v.push(o);
                    }
                }
            }
            Some(v)
        }
        Node::Group(n, _) => first_units(n, icase),
        Node::Alt(v) => {
            let mut out = Vec::new();
            for n in v {
                out.extend(first_units(n, icase)?);
            }
            Some(out)
        }
        Node::Repeat { node, min, .. } if *min >= 1 => first_units(node, icase),
        Node::Concat(v) => {
            for n in v {
                match n {
                    // Zero-width assertions consume nothing.
                    Node::WordBoundary(_) | Node::Look { .. } | Node::Start | Node::End => {}
                    n if n.width().0 > 0 => return first_units(n, icase),
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

struct Parser<'a> {
    src: &'a [u16],
    pos: usize,
    groups: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u16> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, k: usize) -> Option<u16> {
        self.src.get(self.pos + k).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c as u16) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn fail(&self, what: &str) -> ! {
        panic!(
            "unsupported regex construct ({what}) at {} in /{}/",
            self.pos,
            String::from_utf16_lossy(self.src)
        )
    }

    fn alternation(&mut self) -> Node {
        let mut alts = vec![self.concat()];
        while self.eat(b'|') {
            alts.push(self.concat());
        }
        if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Node::Alt(alts)
        }
    }

    fn concat(&mut self) -> Node {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'|' as u16 || c == b')' as u16 {
                break;
            }
            let atom = self.atom();
            let atom = self.quantified(atom);
            items.push(atom);
        }
        match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap(),
            _ => Node::Concat(items),
        }
    }

    /// `{n}`, `{n,}` or `{n,m}` at the cursor, without consuming it.
    fn braces(&self) -> Option<(u32, Option<u32>, usize)> {
        let s = self.src;
        let mut i = self.pos;
        if s.get(i) != Some(&(b'{' as u16)) {
            return None;
        }
        i += 1;
        let num = |i: &mut usize| -> Option<u32> {
            let st = *i;
            while s.get(*i).is_some_and(|&c| js::is_digit(c)) {
                *i += 1;
            }
            (*i > st).then(|| String::from_utf16_lossy(&s[st..*i]).parse().unwrap())
        };
        let min = num(&mut i)?;
        let max = if s.get(i) == Some(&(b',' as u16)) {
            i += 1;
            num(&mut i)
        } else {
            Some(min)
        };
        if s.get(i) != Some(&(b'}' as u16)) {
            return None;
        }
        Some((min, max, i + 1 - self.pos))
    }

    fn quantified(&mut self, atom: Node) -> Node {
        let (min, max) = match self.peek() {
            Some(0x2A) => (0, None),
            Some(0x2B) => (1, None),
            Some(0x3F) => (0, Some(1)),
            Some(0x7B) => match self.braces() {
                Some((a, b, len)) => {
                    self.pos += len - 1;
                    (a, b)
                }
                None => return atom,
            },
            _ => return atom,
        };
        self.pos += 1;
        let greedy = !self.eat(b'?');
        if matches!(
            atom,
            Node::Start | Node::End | Node::WordBoundary(_) | Node::Look { .. }
        ) {
            self.fail("quantified assertion");
        }
        if max.is_none_or(|m| m > 1) && atom.width().0 == 0 {
            self.fail("repeated sub-pattern that can match empty");
        }
        Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        }
    }

    fn atom(&mut self) -> Node {
        let c = self.peek().unwrap();
        self.pos += 1;
        match c {
            0x2E => Node::Any,
            0x5E => Node::Start,
            0x24 => Node::End,
            0x28 => self.group(),
            0x5B => Node::Class(self.class()),
            0x5C => self.escape(),
            0x2A | 0x2B | 0x3F => self.fail("nothing to repeat"),
            _ => Node::Char(c),
        }
    }

    fn group(&mut self) -> Node {
        if self.eat(b'?') {
            let look = |p: &mut Self, ahead: bool, negate: bool| {
                let node = p.alternation();
                if !p.eat(b')') {
                    p.fail("unterminated group");
                }
                if node.has_capture() {
                    p.fail("capture inside a lookaround");
                }
                if !ahead && node.width().1 != Some(node.width().0) {
                    p.fail("variable-width lookbehind");
                }
                Node::Look {
                    node: Box::new(node),
                    ahead,
                    negate,
                }
            };
            if self.eat(b':') {
                let node = self.alternation();
                if !self.eat(b')') {
                    self.fail("unterminated group");
                }
                return Node::Group(Box::new(node), None);
            }
            if self.eat(b'=') {
                return look(self, true, false);
            }
            if self.eat(b'!') {
                return look(self, true, true);
            }
            if self.peek() == Some(b'<' as u16) && self.peek_at(1) == Some(b'=' as u16) {
                self.pos += 2;
                return look(self, false, false);
            }
            if self.peek() == Some(b'<' as u16) && self.peek_at(1) == Some(b'!' as u16) {
                self.pos += 2;
                return look(self, false, true);
            }
            self.fail("group modifier");
        }
        self.groups += 1;
        let idx = self.groups;
        let node = self.alternation();
        if !self.eat(b')') {
            self.fail("unterminated group");
        }
        Node::Group(Box::new(node), Some(idx))
    }

    /// A character escape shared by atoms and classes (after the `\`).
    fn char_escape(&mut self, c: u16) -> u16 {
        match c {
            0x74 => 0x09,
            0x6E => 0x0A,
            0x72 => 0x0D,
            0x66 => 0x0C,
            0x76 => 0x0B,
            0x30 if !self.peek().is_some_and(js::is_digit) => 0,
            0x75 | 0x78 => {
                let n = if c == 0x75 { 4 } else { 2 };
                let hex = self.src.get(self.pos..self.pos + n).unwrap_or(&[]);
                if hex.len() == n && hex.iter().all(|&h| (h as u8 as char).is_ascii_hexdigit()) {
                    self.pos += n;
                    u16::from_str_radix(&String::from_utf16_lossy(hex), 16).unwrap()
                } else {
                    c
                }
            }
            0x31..=0x39 | 0x30 | 0x63 | 0x6B | 0x70 | 0x50 => self.fail("escape"),
            _ => c,
        }
    }

    fn escape(&mut self) -> Node {
        let Some(c) = self.peek() else {
            self.fail("trailing backslash")
        };
        self.pos += 1;
        let class = |item| {
            Node::Class(Class {
                items: vec![item],
                negate: false,
            })
        };
        match c {
            0x62 => Node::WordBoundary(true),
            0x42 => Node::WordBoundary(false),
            0x64 => class(ClassItem::Digit(true)),
            0x44 => class(ClassItem::Digit(false)),
            0x73 => class(ClassItem::Space(true)),
            0x53 => class(ClassItem::Space(false)),
            0x77 => class(ClassItem::Word(true)),
            0x57 => class(ClassItem::Word(false)),
            _ => Node::Char(self.char_escape(c)),
        }
    }

    /// One class atom: `Err(item)` for a class escape, `Ok(unit)` otherwise.
    fn class_atom(&mut self) -> Result<u16, ClassItem> {
        let c = self
            .peek()
            .unwrap_or_else(|| self.fail("unterminated class"));
        self.pos += 1;
        if c != b'\\' as u16 {
            return Ok(c);
        }
        let Some(e) = self.peek() else {
            self.fail("trailing backslash")
        };
        self.pos += 1;
        match e {
            0x64 => Err(ClassItem::Digit(true)),
            0x44 => Err(ClassItem::Digit(false)),
            0x73 => Err(ClassItem::Space(true)),
            0x53 => Err(ClassItem::Space(false)),
            0x77 => Err(ClassItem::Word(true)),
            0x57 => Err(ClassItem::Word(false)),
            0x62 => Ok(0x08),
            0x2D => Ok(0x2D),
            _ => Ok(self.char_escape(e)),
        }
    }

    fn class(&mut self) -> Class {
        let negate = self.eat(b'^');
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None => self.fail("unterminated class"),
                Some(0x5D) => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }
            let a = self.class_atom();
            let is_range = self.peek() == Some(b'-' as u16)
                && self.peek_at(1).is_some_and(|c| c != b']' as u16);
            if !is_range {
                items.push(a.map_or_else(|i| i, |c| ClassItem::Range(c, c)));
                continue;
            }
            self.pos += 1; // '-'
            let b = self.class_atom();
            match (a, b) {
                (Ok(x), Ok(y)) => {
                    if x > y {
                        self.fail("class range out of order");
                    }
                    items.push(ClassItem::Range(x, y));
                }
                // Annex B: a class escape at either end makes `-` literal.
                (a, b) => {
                    items.push(a.map_or_else(|i| i, |c| ClassItem::Range(c, c)));
                    items.push(ClassItem::Range(0x2D, 0x2D));
                    items.push(b.map_or_else(|i| i, |c| ClassItem::Range(c, c)));
                }
            }
        }
        Class { items, negate }
    }
}

#[derive(Debug, Clone)]
enum Inst {
    Char(u16),
    Any,
    Class(usize),
    Start,
    End,
    WordBoundary(bool),
    /// Try the first target, backtrack into the second.
    Split(usize, usize),
    Jmp(usize),
    Save(usize),
    Look(usize),
    Match,
}

#[derive(Debug, Clone)]
struct Program {
    insts: Vec<Inst>,
}

#[derive(Debug, Clone)]
struct Lookaround {
    prog: Program,
    ahead: bool,
    negate: bool,
    width: usize,
}

/// A compiled pattern.
#[derive(Debug, Clone)]
pub struct Regex {
    prog: Program,
    classes: Vec<Class>,
    looks: Vec<Lookaround>,
    groups: usize,
    icase: bool,
    multiline: bool,
    /// The pattern begins with `^`: only line starts (or 0) can match.
    anchored: bool,
    /// Every match starts with one of these units (a pure prefilter).
    first: Option<Vec<u16>>,
}

struct Compiler<'a> {
    classes: &'a mut Vec<Class>,
    looks: &'a mut Vec<Lookaround>,
}

impl Compiler<'_> {
    fn program(&mut self, node: &Node, save_whole: bool) -> Program {
        let mut insts = Vec::new();
        if save_whole {
            insts.push(Inst::Save(0));
        }
        self.emit(node, &mut insts);
        if save_whole {
            insts.push(Inst::Save(1));
        }
        insts.push(Inst::Match);
        Program { insts }
    }

    fn emit(&mut self, node: &Node, out: &mut Vec<Inst>) {
        match node {
            Node::Empty => {}
            Node::Char(c) => out.push(Inst::Char(*c)),
            Node::Any => out.push(Inst::Any),
            Node::Class(c) => {
                self.classes.push(c.clone());
                out.push(Inst::Class(self.classes.len() - 1));
            }
            Node::Start => out.push(Inst::Start),
            Node::End => out.push(Inst::End),
            Node::WordBoundary(b) => out.push(Inst::WordBoundary(*b)),
            Node::Group(n, cap) => {
                if let Some(i) = cap {
                    out.push(Inst::Save(2 * i));
                }
                self.emit(n, out);
                if let Some(i) = cap {
                    out.push(Inst::Save(2 * i + 1));
                }
            }
            Node::Concat(v) => v.iter().for_each(|n| self.emit(n, out)),
            Node::Alt(v) => {
                let mut jumps = Vec::new();
                for (i, n) in v.iter().enumerate() {
                    if i + 1 < v.len() {
                        let split = out.len();
                        out.push(Inst::Split(split + 1, 0));
                        self.emit(n, out);
                        jumps.push(out.len());
                        out.push(Inst::Jmp(0));
                        let next = out.len();
                        out[split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(n, out);
                    }
                }
                let end = out.len();
                for j in jumps {
                    out[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => {
                for _ in 0..*min {
                    self.emit(node, out);
                }
                let split = |a: usize, b: usize| {
                    if *greedy {
                        Inst::Split(a, b)
                    } else {
                        Inst::Split(b, a)
                    }
                };
                match max {
                    None => {
                        let top = out.len();
                        out.push(Inst::Split(0, 0));
                        self.emit(node, out);
                        out.push(Inst::Jmp(top));
                        let end = out.len();
                        out[top] = split(top + 1, end);
                    }
                    Some(m) => {
                        let mut splits = Vec::new();
                        for _ in *min..*m {
                            splits.push(out.len());
                            out.push(Inst::Split(0, 0));
                            self.emit(node, out);
                        }
                        let end = out.len();
                        for s in splits {
                            out[s] = split(s + 1, end);
                        }
                    }
                }
            }
            Node::Look {
                node,
                ahead,
                negate,
            } => {
                let prog = self.program(node, true);
                self.looks.push(Lookaround {
                    prog,
                    ahead: *ahead,
                    negate: *negate,
                    width: node.width().0,
                });
                out.push(Inst::Look(self.looks.len() - 1));
            }
        }
    }
}

enum Backtrack {
    State(usize, usize),
    Restore(usize, Option<usize>),
}

/// Capture spans of one match (`match[i]` → `get(i)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captures {
    slots: Vec<Option<usize>>,
}

impl Captures {
    /// Start of the whole match (`match.index`).
    pub fn start(&self) -> usize {
        self.slots[0].unwrap()
    }

    pub fn end(&self) -> usize {
        self.slots[1].unwrap()
    }

    /// `match[i]` as a range; `None` when the group did not participate.
    pub fn get(&self, i: usize) -> Option<(usize, usize)> {
        match (self.slots.get(2 * i)?, self.slots.get(2 * i + 1)?) {
            (Some(a), Some(b)) => Some((*a, *b)),
            _ => None,
        }
    }

    /// `match[i]` as text.
    pub fn text<'s>(&self, s: &'s [u16], i: usize) -> Option<&'s [u16]> {
        self.get(i).map(|(a, b)| &s[a..b])
    }
}

impl Regex {
    /// Compile `/source/flags`.
    pub fn new(source: &str, flags: &str) -> Self {
        let mut icase = false;
        let mut multiline = false;
        for f in flags.chars() {
            match f {
                'g' => {}
                'i' => icase = true,
                'm' => multiline = true,
                _ => panic!("unsupported regex flag {f}"),
            }
        }
        let src: Vec<u16> = source.encode_utf16().collect();
        let mut p = Parser {
            src: &src,
            pos: 0,
            groups: 0,
        };
        let node = p.alternation();
        if p.pos != src.len() {
            p.fail("unbalanced parenthesis");
        }
        let anchored = match &node {
            Node::Concat(v) => matches!(v.first(), Some(Node::Start)),
            Node::Start => true,
            _ => false,
        };
        let first = first_units(&node, icase).map(|mut v| {
            v.sort_unstable();
            v.dedup();
            v
        });
        let mut classes = Vec::new();
        let mut looks = Vec::new();
        let prog = Compiler {
            classes: &mut classes,
            looks: &mut looks,
        }
        .program(&node, true);
        Self {
            prog,
            classes,
            looks,
            groups: p.groups,
            icase,
            multiline,
            anchored,
            first,
        }
    }

    fn eq(&self, a: u16, b: u16) -> bool {
        a == b || (self.icase && other_case(a) == Some(b))
    }

    /// Run `prog` at `start`; on success the slots hold the captures.
    fn run(&self, prog: &Program, s: &[u16], start: usize, slots: &mut [Option<usize>]) -> bool {
        let mut stack: Vec<Backtrack> = Vec::new();
        let (mut pc, mut pos) = (0usize, start);
        loop {
            let ok = match prog.insts[pc] {
                Inst::Char(c) => {
                    let hit = pos < s.len() && self.eq(s[pos], c);
                    if hit {
                        pos += 1;
                        pc += 1;
                    }
                    hit
                }
                Inst::Any => {
                    let hit = pos < s.len() && !js::is_line_terminator(s[pos]);
                    if hit {
                        pos += 1;
                        pc += 1;
                    }
                    hit
                }
                Inst::Class(i) => {
                    let hit = pos < s.len() && self.classes[i].matches(s[pos], self.icase);
                    if hit {
                        pos += 1;
                        pc += 1;
                    }
                    hit
                }
                Inst::Start => {
                    let hit = if self.multiline {
                        js::is_line_start(s, pos)
                    } else {
                        pos == 0
                    };
                    pc += 1;
                    hit
                }
                Inst::End => {
                    let hit = pos == s.len() || (self.multiline && js::is_line_terminator(s[pos]));
                    pc += 1;
                    hit
                }
                Inst::WordBoundary(want) => {
                    let a = pos > 0 && js::is_word(s[pos - 1]);
                    let b = pos < s.len() && js::is_word(s[pos]);
                    pc += 1;
                    (a != b) == want
                }
                Inst::Split(a, b) => {
                    stack.push(Backtrack::State(b, pos));
                    pc = a;
                    true
                }
                Inst::Jmp(a) => {
                    pc = a;
                    true
                }
                Inst::Save(k) => {
                    if k < slots.len() {
                        stack.push(Backtrack::Restore(k, slots[k]));
                        slots[k] = Some(pos);
                    }
                    pc += 1;
                    true
                }
                Inst::Look(i) => {
                    let l = &self.looks[i];
                    let found = if l.ahead {
                        self.run(&l.prog, s, pos, &mut [None, None])
                    } else {
                        // Fixed width: the body must match exactly the
                        // `width` units that end at `pos`.
                        pos >= l.width && self.run_exact(&l.prog, s, pos - l.width, pos)
                    };
                    pc += 1;
                    found != l.negate
                }
                Inst::Match => return true,
            };
            if ok {
                continue;
            }
            loop {
                match stack.pop() {
                    None => return false,
                    Some(Backtrack::Restore(k, v)) => slots[k] = v,
                    Some(Backtrack::State(p, q)) => {
                        pc = p;
                        pos = q;
                        break;
                    }
                }
            }
        }
    }

    /// Does the fixed-width body `prog` match the units `s[start..end]`?
    /// It runs forward on the full input, so assertions inside see the same
    /// context as ECMAScript's backward lookbehind matching; every match of
    /// a fixed-width body has the same length, so the first one decides.
    fn run_exact(&self, prog: &Program, s: &[u16], start: usize, end: usize) -> bool {
        let mut slots = [None, None];
        self.run(prog, s, start, &mut slots) && slots[1] == Some(end)
    }

    fn try_at(&self, s: &[u16], pos: usize, slots: &mut [Option<usize>]) -> Option<Captures> {
        slots.fill(None);
        self.run(&self.prog, s, pos, slots).then(|| Captures {
            slots: slots.to_vec(),
        })
    }

    /// `re.exec(s)` with `lastIndex = from` (global) or `0` (non-global).
    pub fn find_at(&self, s: &[u16], from: usize) -> Option<Captures> {
        let mut slots = vec![None; 2 * (self.groups + 1)];
        for p in from..=s.len() {
            if self.anchored
                && !(if self.multiline {
                    js::is_line_start(s, p)
                } else {
                    p == 0
                })
            {
                if !self.multiline {
                    return None;
                }
                continue;
            }
            if let Some(f) = &self.first
                && !(p < s.len() && f.binary_search(&s[p]).is_ok())
            {
                continue;
            }
            if let Some(m) = self.try_at(s, p, &mut slots) {
                return Some(m);
            }
        }
        None
    }

    /// `re.exec(s)` for a non-global regex.
    pub fn exec(&self, s: &[u16]) -> Option<Captures> {
        self.find_at(s, 0)
    }

    /// `re.test(s)`.
    pub fn test(&self, s: &[u16]) -> bool {
        self.exec(s).is_some()
    }

    /// `[...s.matchAll(re)]` (global): an empty match advances by one unit.
    pub fn match_all(&self, s: &[u16]) -> Vec<Captures> {
        let mut out = Vec::new();
        let mut from = 0;
        while from <= s.len() {
            let Some(m) = self.find_at(s, from) else {
                break;
            };
            from = if m.end() == m.start() {
                m.end() + 1
            } else {
                m.end()
            };
            out.push(m);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::utf16;

    fn all(src: &str, flags: &str, text: &str) -> Vec<Vec<Option<String>>> {
        let re = Regex::new(src, flags);
        let s = utf16(text);
        re.match_all(&s)
            .into_iter()
            .map(|m| {
                (0..=re.groups)
                    .map(|i| m.text(&s, i).map(js::string))
                    .collect()
            })
            .collect()
    }

    fn first(src: &str, flags: &str, text: &str) -> Vec<Option<String>> {
        all(src, flags, text).into_iter().next().unwrap_or_default()
    }

    #[test]
    fn greedy_lazy_and_backtracking() {
        assert_eq!(
            first(r"^(#{1,6})\s+(.+)$", "gm", "#  \n# real"),
            vec![
                Some("#  \n# real".into()),
                Some("#".into()),
                Some("# real".into())
            ]
        );
        assert_eq!(
            first(r"^(#{1,6})\s+(.+)$", "gm", "# x\r\n"),
            vec![Some("# x".into()), Some("#".into()), Some("x".into())]
        );
        assert!(all(r"^(#{1,6})\s+(.+)$", "gm", "####### x").is_empty());
        assert_eq!(
            first(r"^\[\[?\s*([^[\]\s][^[\]]*?)\s*\]\]?", "gm", "[[ a b ]]"),
            vec![Some("[[ a b ]]".into()), Some("a b".into())]
        );
        assert_eq!(
            first(
                r"\b([A-Za-z_]\w*)\s*\([^;]*\)\s*(?:async\s*)?\{",
                "g",
                "f(a) { g(b) {"
            ),
            vec![Some("f(a) { g(b) {".into()), Some("f".into())]
        );
        assert_eq!(
            first(
                r"<script[^>]+src\s*=\s*['\x22]([^'\x22]+)['\x22]",
                "g",
                "<script data-src='a' src=\"b\">"
            )[1],
            Some("b".into())
        );
    }

    #[test]
    fn flags_classes_and_assertions() {
        assert_eq!(
            first(
                r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:TABLE|VIEW)\s+([A-Za-z_\x22][\w.\x22]*)",
                "gi",
                "create or replace view \"S\".v ("
            )[1],
            Some("\"S\".v".into())
        );
        assert!(all(r"\bid", "gi", "valid ID").len() == 1);
        assert_eq!(
            all(
                r"(?<!!)\[[^\]]+\]\(([^)\s]+)\)",
                "g",
                "![i](a.png) [d](b.md)"
            )
            .into_iter()
            .map(|m| m[1].clone().unwrap())
            .collect::<Vec<_>>(),
            vec!["b.md".to_string()]
        );
        assert_eq!(
            first(r"^([A-Za-z_][\w.-]*)\s*:(?!=)\s*(.*)$", "", "all:\r")[2],
            Some("".into())
        );
        assert!(all(r"^([A-Za-z_][\w.-]*)\s*:(?!=)\s*(.*)$", "", "all: a\r").is_empty());
        assert!(all(r"^([A-Za-z_][\w.-]*)\s*:(?!=)", "", "x := 1").is_empty());
        assert_eq!(
            first(r"[\w.\-:/#!?]+", "", "a-b:c d")[0],
            Some("a-b:c".into())
        );
        assert_eq!(
            first(r"^\s{0,3}(`{3,}|~{3,})", "", "   ````x")[1],
            Some("````".into())
        );
        assert!(all(r"^\s{0,3}(`{3,}|~{3,})", "", "    ```").is_empty());
        // `\s` is the ECMAScript set; `.` stops at U+2028.
        assert_eq!(
            first(r"a\s+(.+)", "", "a\u{3000}b\u{2028}c")[1],
            Some("b".into())
        );
    }

    #[test]
    #[should_panic(expected = "unsupported")]
    fn rejects_backreferences() {
        Regex::new(r"(a)\1", "");
    }
}
