//! Graph centrality of a full run: `runGraphRankPass` (`graph-rank-pass.ts`),
//! the wiring graph and PageRank (`graph-rank.ts`) and `replaceRanks`
//! (`writer-rank.ts`).
//!
//! Floating-point work is sequential and in WrongStack's operation order, so
//! scores are bit-identical (IEEE-754 doubles, no fused multiply-add, no
//! reassociated sums).

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::paths::win32_dirname;
use crate::relations::{detect_lang, language_family};
use crate::rows::MODULE_OWNER_NAME;

pub const RANK_VERSION: &str = "1";
const MAX_SQL_VARIABLES: usize = 900;
const DIRECTORY_SCOPED: &[&str] = &["go", "jvm", "dotnet", "swift"];
const DIRECTORY_IMPORT: &[&str] = &["go", "jvm"];
const WALK_RELATIONS: &[&str] = &["call", "import", "type_ref", "inherit", "implement"];
const RESTART: f64 = 0.25;
const ITERATIONS: usize = 25;

/// `FAMILY_BY_LANG` (`writer-rank.ts`): the `lang_family` table, wildcard
/// row included.
fn family_by_lang(lang: &str) -> Option<&'static str> {
    crate::native::LANG_FAMILY
        .iter()
        .find(|(l, _)| *l == lang)
        .map(|(_, f)| *f)
}

fn file_family(f: &str) -> Option<&'static str> {
    detect_lang(f).map(language_family)
}

/// `implicitVisibility(src, dst, imports)`.
fn visible(src: &str, dst: &str, imports: Option<&HashSet<String>>) -> bool {
    let Some(tf) = file_family(dst) else {
        return false;
    };
    let td = win32_dirname(dst);
    if DIRECTORY_SCOPED.contains(&tf) && win32_dirname(src) == td && file_family(src) == Some(tf) {
        return true;
    }
    let Some(imports) = imports else {
        return false;
    };
    if !DIRECTORY_IMPORT.contains(&tf) {
        return false;
    }
    imports.iter().any(|f| win32_dirname(f) == td)
}

/// A weighted, undirected wiring graph over interned symbol ids.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    /// Interned index → symbol id.
    pub ids: Vec<i64>,
    pub from: Vec<usize>,
    pub to: Vec<usize>,
    pub weight: Vec<f64>,
}

/// Per-node PageRank scores (max-normalised) and degrees.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub scores: Vec<f64>,
    pub in_deg: Vec<i64>,
    pub out_deg: Vec<i64>,
}

/// `pageRank` with a uniform restart vector over the CSR adjacency, then
/// normalised by the maximum score.
pub fn page_rank(g: &Graph) -> Ranked {
    let size = g.ids.len();
    let mut counts = vec![0usize; size];
    let mut in_deg = vec![0i64; size];
    let mut out_deg = vec![0i64; size];
    for (&u, &v) in g.from.iter().zip(&g.to) {
        counts[u] += 1;
        counts[v] += 1;
        out_deg[u] += 1;
        in_deg[v] += 1;
    }
    let mut offsets = vec![0usize; size + 1];
    let mut running = 0;
    for u in 0..size {
        offsets[u] = running;
        running += counts[u];
    }
    offsets[size] = running;
    let mut cursor: Vec<usize> = offsets[..size].to_vec();
    let mut neigh = vec![0usize; running];
    let mut wts = vec![0f64; running];
    for ((&u, &v), &w) in g.from.iter().zip(&g.to).zip(&g.weight) {
        neigh[cursor[u]] = v;
        wts[cursor[u]] = w;
        cursor[u] += 1;
        neigh[cursor[v]] = u;
        wts[cursor[v]] = w;
        cursor[v] += 1;
    }
    let walk = 1.0 - RESTART;
    let rd = vec![1.0 / size as f64; size];
    let mut current = rd.clone();
    for _ in 0..ITERATIONS {
        let mut next = vec![0f64; size];
        let mut teleport = 0f64;
        for u in 0..size {
            let mass = current[u];
            if mass == 0.0 {
                continue;
            }
            teleport += RESTART * mass;
            let (start, end) = (offsets[u], offsets[u + 1]);
            let deg = end - start;
            if deg == 0 {
                teleport += walk * mass;
                continue;
            }
            let outgoing = walk * mass;
            let share = outgoing / deg as f64;
            let mut moved = 0f64;
            for e in start..end {
                let tr = share * wts[e];
                next[neigh[e]] += tr;
                moved += tr;
            }
            teleport += outgoing - moved;
        }
        if teleport > 0.0 {
            for u in 0..size {
                next[u] += teleport * rd[u];
            }
        }
        current = next;
    }
    let mut mx = 0f64;
    for &v in &current {
        if v > mx {
            mx = v;
        }
    }
    let scores = if mx > 0.0 {
        current.iter().map(|v| v / mx).collect()
    } else {
        vec![0.0; size]
    };
    Ranked {
        scores,
        in_deg,
        out_deg,
    }
}

type SymbolRankRow = (i64, f64, i64, i64);
type FileRankRow = (String, f64, i64, i64);

/// Rows for `symbol_rank` and `file_rank`, in insertion order.
pub fn rank_rows(tx: &Connection) -> rusqlite::Result<(Vec<SymbolRankRow>, Vec<FileRankRow>)> {
    let mut file_of: HashMap<i64, String> = HashMap::new();
    let mut groups: HashMap<(&'static str, String), usize> = HashMap::new();
    let mut group_of: Vec<(i64, (&'static str, String))> = Vec::new();
    let mut module_of: HashMap<String, i64> = HashMap::new();
    let mut declaring: HashSet<String> = HashSet::new();
    {
        let mut st = tx.prepare("SELECT id, file, name, lang FROM symbols")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let file: String = r.get(1)?;
            let name: String = r.get(2)?;
            let lang: String = r.get(3)?;
            if name == MODULE_OWNER_NAME {
                module_of.insert(file.clone(), id);
            } else {
                declaring.insert(file.clone());
            }
            file_of.insert(id, file);
            if let Some(fam) = family_by_lang(&lang) {
                *groups.entry((fam, name.clone())).or_default() += 1;
                group_of.push((id, (fam, name)));
            }
        }
    }
    let mut candidates: HashMap<i64, usize> = HashMap::new();
    for (id, key) in &group_of {
        let n = groups[key];
        if n >= 2 {
            candidates.insert(*id, n);
        }
    }
    let shadowed: HashSet<i64> = module_of
        .iter()
        .filter(|(f, _)| declaring.contains(*f))
        .map(|(_, id)| *id)
        .collect();
    let refs: Vec<(i64, i64, String)> = tx
        .prepare("SELECT from_id, to_id, call_type FROM refs WHERE to_id IS NOT NULL ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(from, _, _)| !shadowed.contains(from))
        .collect();
    let mut imports_of: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut st = tx.prepare(
            "SELECT DISTINCT s.file AS src, r.to_file AS dst
    FROM refs r
    JOIN symbols s ON s.id = r.from_id
    WHERE r.call_type = 'import' AND r.to_file IS NOT NULL AND r.to_file <> ''",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            imports_of.entry(r.get(0)?).or_default().insert(r.get(1)?);
        }
    }

    // buildWiringGraph
    let mut g = Graph::default();
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut intern = |id: i64, g: &mut Graph| -> usize {
        *index.entry(id).or_insert_with(|| {
            g.ids.push(id);
            g.ids.len() - 1
        })
    };
    for (from_id, to_id, ct) in &refs {
        if !WALK_RELATIONS.contains(&ct.as_str()) || from_id == to_id {
            continue;
        }
        let u = intern(*from_id, &mut g);
        let v = intern(*to_id, &mut g);
        g.from.push(u);
        g.to.push(v);
        let h = candidates.get(to_id).copied().unwrap_or(1);
        let mut w = if h > 1 { 1.0 / h as f64 } else { 1.0 };
        if let (Some(sf), Some(tf)) = (file_of.get(from_id), file_of.get(to_id))
            && sf != tf
        {
            let imports = imports_of.get(sf);
            if visible(sf, tf, imports) {
            } else if imports.is_none() {
                w *= 0.25;
            } else if !imports.is_some_and(|i| i.contains(tf)) {
                w *= 0.02;
            }
        }
        g.weight.push(w);
    }
    if g.ids.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ranked = page_rank(&g);
    let sym_rows: Vec<SymbolRankRow> = (0..g.ids.len())
        .map(|u| {
            (
                g.ids[u],
                ranked.scores[u],
                ranked.in_deg[u],
                ranked.out_deg[u],
            )
        })
        .collect();
    let mut file_rows: Vec<FileRankRow> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (sid, rank, i, o) in &sym_rows {
        let Some(f) = file_of.get(sid) else { continue };
        match at.get(f) {
            None => {
                at.insert(f.clone(), file_rows.len());
                file_rows.push((f.clone(), *rank, *i, *o));
            }
            Some(&k) => {
                let e = &mut file_rows[k];
                e.1 += rank;
                e.2 += i;
                e.3 += o;
            }
        }
    }
    let mut mx = 0f64;
    for e in &file_rows {
        if e.1 > mx {
            mx = e.1;
        }
    }
    if mx > 0.0 {
        for e in &mut file_rows {
            e.1 /= mx;
        }
    }
    Ok((sym_rows, file_rows))
}

/// `replaceRanks(symbolRows, fileRows)` in its own savepoint.
pub fn replace_ranks(
    tx: &Connection,
    savepoint: &str,
    sym_rows: &[SymbolRankRow],
    file_rows: &[FileRankRow],
) -> rusqlite::Result<()> {
    use rusqlite::types::Value as V;
    tx.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
    tx.execute_batch("DELETE FROM symbol_rank")?;
    let mut at = 0;
    for n in crate::native::ladder(sym_rows.len(), MAX_SQL_VARIABLES / 4) {
        let sql = format!(
            "INSERT INTO symbol_rank(symbol_id, rank, in_deg, out_deg) VALUES {}",
            vec!["(?, ?, ?, ?)"; n].join(", ")
        );
        let binds: Vec<V> = sym_rows[at..at + n]
            .iter()
            .flat_map(|(s, r, i, o)| [V::Integer(*s), V::Real(*r), V::Integer(*i), V::Integer(*o)])
            .collect();
        tx.execute(&sql, rusqlite::params_from_iter(binds))?;
        at += n;
    }
    tx.execute_batch("DELETE FROM file_rank")?;
    let mut at = 0;
    for n in crate::native::ladder(file_rows.len(), MAX_SQL_VARIABLES / 4) {
        let sql = format!(
            "INSERT INTO file_rank(file, rank, in_deg, out_deg) VALUES {}",
            vec!["(?, ?, ?, ?)"; n].join(", ")
        );
        let binds: Vec<V> = file_rows[at..at + n]
            .iter()
            .flat_map(|(f, r, i, o)| {
                [
                    V::Text(f.clone()),
                    V::Real(*r),
                    V::Integer(*i),
                    V::Integer(*o),
                ]
            })
            .collect();
        tx.execute(&sql, rusqlite::params_from_iter(binds))?;
        at += n;
    }
    tx.execute_batch(&format!("RELEASE {savepoint}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answers from the validated Python reference (`finalize_ref.py`
    /// `rank_pass`) on a graph with homonyms, a shadowed module owner, a
    /// self-loop, a non-walk relation, Go directory visibility (same
    /// directory and imported directory) and the 0.25 / 0.02 penalties.
    #[test]
    fn rank_pass_is_bit_identical_to_the_reference() {
        let c = Connection::open_in_memory().unwrap();
        for s in crate::native::schema_statements() {
            c.execute_batch(&s).unwrap();
        }
        let (a, b, cc) = (r"C:\p\a.ts", r"C:\p\b.ts", r"C:\p\c.ts");
        let (x, y, z) = (r"C:\p\g\x.go", r"C:\p\g\y.go", r"C:\p\h\z.go");
        let syms = [
            (1, a, "<module>", "ts", "mod"),
            (2, b, "foo", "ts", "function"),
            (3, cc, "foo", "ts", "function"),
            (4, b, "bar", "ts", "function"),
            (5, cc, "<module>", "ts", "mod"),
            (6, x, "Run", "go", "function"),
            (7, y, "Helper", "go", "function"),
            (8, z, "Util", "go", "function"),
            (9, b, "baz", "ts", "function"),
        ];
        for (id, f, name, lang, kind) in syms {
            c.execute(
                "INSERT INTO symbols(id, lang, kind, name, file, line, col, signature, doc_comment, scope, end_line, is_exported, signature_hash) VALUES (?1, ?2, ?3, ?4, ?5, 1, 0, '', '', '', 1, 0, '')",
                rusqlite::params![id, lang, kind, name, f],
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
        let refs: [R; 15] = [
            (1, "foo", Some(2), "import", Some("./b"), Some(b)),
            (1, "foo", Some(3), "import", Some("./c"), Some(cc)),
            (2, "bar", Some(4), "call", None, None),
            (4, "foo", Some(3), "call", None, None),
            (5, "foo", Some(2), "call", None, None),
            (6, "Helper", Some(7), "call", None, None),
            (6, "Util", Some(8), "call", None, None),
            (6, "h", Some(8), "import", Some("h"), Some(z)),
            (7, "Util", Some(8), "call", None, None),
            (2, "foo", Some(2), "call", None, None),
            (9, "bar", Some(4), "reference", None, None),
            (4, "<module>", Some(1), "call", None, None),
            (2, "baz", Some(9), "call", None, None),
            (1, "Run", Some(6), "call", None, None),
            (9, "zzz", None, "call", None, None),
        ];
        for (from, to_name, to_id, ct, module, to_file) in refs {
            c.execute(
                "INSERT INTO refs(from_id, to_name, to_id, call_type, line, lang, module, to_file) VALUES (?1, ?2, ?3, ?4, 1, '', ?5, ?6)",
                rusqlite::params![from, to_name, to_id, ct, module, to_file],
            )
            .unwrap();
        }
        let (sym, file) = rank_rows(&c).unwrap();
        let sym: Vec<(i64, u64, i64, i64)> = sym
            .into_iter()
            .map(|(s, r, i, o)| (s, r.to_bits(), i, o))
            .collect();
        assert_eq!(
            sym,
            vec![
                (1, 0x3fe2413958ea1334, 1, 3),
                (2, 0x3ff0000000000000, 1, 2),
                (3, 0x3fdab3bd607d6074, 2, 0),
                (4, 0x3fe410f75a808d74, 1, 2),
                (6, 0x3fedcf8cc38f6297, 1, 3),
                (7, 0x3fe2185964766b77, 1, 1),
                (8, 0x3fe7e356be1de18a, 3, 0),
                (9, 0x3fe3033992204bb0, 1, 0),
            ]
        );
        let file: Vec<(String, u64, i64, i64)> = file
            .into_iter()
            .map(|(f, r, i, o)| (f, r.to_bits(), i, o))
            .collect();
        let want: Vec<(String, u64, i64, i64)> = [
            (a, 0x3fd06fcf2e75b0f4, 1, 3),
            (b, 0x3ff0000000000000, 3, 4),
            (cc, 0x3fc80af4cc82bdcc, 2, 0),
            (x, 0x3fdad7829cb9f4b1, 1, 3),
            (y, 0x3fd04b0159bb5834, 1, 1),
            (z, 0x3fd5824d24bd4b63, 3, 0),
        ]
        .into_iter()
        .map(|(f, r, i, o)| (f.to_string(), r, i, o))
        .collect();
        assert_eq!(file, want);

        // replaceRanks writes exactly these rows.
        replace_ranks(&c, "t", &rank_rows(&c).unwrap().0, &[]).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM symbol_rank", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 8);
    }

    #[test]
    fn page_rank_of_an_empty_and_a_single_edge_graph() {
        let g = Graph {
            ids: vec![10, 20],
            from: vec![0],
            to: vec![1],
            weight: vec![1.0],
        };
        let r = page_rank(&g);
        assert_eq!(r.scores, vec![1.0, 1.0]);
        assert_eq!((r.in_deg, r.out_deg), (vec![0, 1], vec![1, 0]));
    }
}
