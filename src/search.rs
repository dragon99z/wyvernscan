//! Search over a finished scan's arena. Pure `std` (no egui), so it is unit
//! tested everywhere.
//!
//! Query syntax, terms separated by spaces (quote a term to keep spaces):
//!   `*.log`  `report?.txt`  `backup`   file name; `*` and `?` are wildcards,
//!                                      a term without wildcards matches anywhere
//!   `type:jpg,png`  `type:folder`      extensions, `folder` or `file`
//!   `size:>100MB`  `size:<=2GB`  `size:5MB`   bigger than / smaller than / exactly
//! Every term must match (AND), so `*.mp4 size:>1GB` is "mp4 files over 1 GiB".

use crate::scanner::Node;

#[derive(Debug, PartialEq)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

#[derive(Debug, Default)]
pub struct Query {
    /// Lowercased glob patterns; each must match the node's name.
    names: Vec<Vec<char>>,
    /// Empty = any type. Otherwise a node must match at least one entry.
    types: Vec<String>,
    sizes: Vec<(Cmp, u64)>,
}

fn split_terms(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// "100MB", "1.5 GiB", "512" (bytes). Binary units, to match what the UI shows.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| format!("bad size: {s}"))?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0_f64.powi(4),
        u => return Err(format!("unknown size unit: {u}")),
    };
    Ok((n * mult) as u64)
}

impl Query {
    pub fn parse(input: &str) -> Result<Query, String> {
        let mut q = Query::default();
        for term in split_terms(input) {
            let lower = term.to_lowercase();
            if let Some(v) = lower.strip_prefix("size:") {
                let (cmp, rest) = if let Some(r) = v.strip_prefix(">=") {
                    (Cmp::Ge, r)
                } else if let Some(r) = v.strip_prefix("<=") {
                    (Cmp::Le, r)
                } else if let Some(r) = v.strip_prefix('>') {
                    (Cmp::Gt, r)
                } else if let Some(r) = v.strip_prefix('<') {
                    (Cmp::Lt, r)
                } else {
                    (Cmp::Eq, v.strip_prefix('=').unwrap_or(v))
                };
                q.sizes.push((cmp, parse_size(rest)?));
            } else if let Some(v) = lower.strip_prefix("type:").or_else(|| lower.strip_prefix("ext:")) {
                q.types.extend(
                    v.split(',')
                        .map(|t| t.trim().trim_start_matches('.').to_string())
                        .filter(|t| !t.is_empty()),
                );
            } else {
                let mut p: Vec<char> = lower.chars().collect();
                if !p.iter().any(|&c| c == '*' || c == '?') {
                    p.insert(0, '*');
                    p.push('*');
                }
                q.names.push(p);
            }
        }
        if q.names.is_empty() && q.types.is_empty() && q.sizes.is_empty() {
            return Err("Empty search".to_string());
        }
        Ok(q)
    }

    pub fn matches(&self, n: &Node) -> bool {
        if !self.sizes.iter().all(|(cmp, v)| match cmp {
            Cmp::Gt => n.size > *v,
            Cmp::Ge => n.size >= *v,
            Cmp::Lt => n.size < *v,
            Cmp::Le => n.size <= *v,
            Cmp::Eq => n.size == *v,
        }) {
            return false;
        }
        if !self.types.is_empty() {
            let ext = n.name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
            let ok = self.types.iter().any(|t| match t.as_str() {
                "folder" | "dir" | "directory" => n.is_dir,
                "file" => !n.is_dir,
                t => !n.is_dir && ext.eq_ignore_ascii_case(t),
            });
            if !ok {
                return false;
            }
        }
        if self.names.is_empty() {
            return true;
        }
        let name: Vec<char> = n.name.to_lowercase().chars().collect();
        self.names.iter().all(|p| glob(p, &name))
    }
}

/// Iterative glob with `*` and `?` (backtracks to the last `*`, so no
/// exponential blow-up on patterns like `*a*a*a*b`).
fn glob(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Indices of every matching node except the scan root, biggest first.
pub fn search(arena: &[Node], root: usize, q: &Query) -> Vec<usize> {
    let mut hits: Vec<usize> = (0..arena.len()).filter(|&i| i != root && q.matches(&arena[i])).collect();
    hits.sort_by(|&a, &b| arena[b].size.cmp(&arena[a].size));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, is_dir: bool, size: u64) -> Node {
        Node { name: name.into(), is_dir, size, parent: None, children: vec![], abs_path: None }
    }

    fn hit(q: &str, n: &Node) -> bool {
        Query::parse(q).unwrap().matches(n)
    }

    #[test]
    fn wildcards_and_substring() {
        let f = node("Holiday.JPG", false, 10);
        assert!(hit("*.jpg", &f));
        assert!(hit("holi", &f)); // no wildcard = substring, case-insensitive
        assert!(hit("h?liday.*", &f));
        assert!(!hit("*.png", &f));
        assert!(!hit("holiday", &node("hol", false, 1)));
        assert!(hit("*a*a*a*b", &node("aaaaaaaaaaaaaaab", false, 1)));
    }

    #[test]
    fn size_bounds() {
        let f = node("a.bin", false, 2 * 1024 * 1024);
        assert!(hit("size:>1MB", &f));
        assert!(!hit("size:<1MB", &f));
        assert!(hit("size:>=2MB", &f));
        assert!(hit("size:<=2MB", &f));
        assert!(hit("size:2MB", &f));
        assert!(!hit("size:>2MB", &f));
        assert!(Query::parse("size:big").is_err());
    }

    #[test]
    fn types_and_combination() {
        let mp4 = node("clip.mp4", false, 3 * 1024 * 1024 * 1024);
        let dir = node("clips", true, 5);
        assert!(hit("type:mp4,mkv", &mp4));
        assert!(!hit("type:mp4", &dir));
        assert!(hit("type:folder", &dir));
        assert!(hit("clip type:.mp4 size:>1GB", &mp4));
        assert!(!hit("clip type:mp4 size:<1GB", &mp4));
        assert!(hit("\"clip\" ext:mp4", &mp4));
        assert!(Query::parse("   ").is_err());
    }

    #[test]
    fn search_skips_root_and_sorts_by_size() {
        let arena = vec![node("", true, 30), node("a.txt", false, 5), node("b.txt", false, 25)];
        let q = Query::parse("type:txt").unwrap();
        assert_eq!(search(&arena, 0, &q), vec![2, 1]);
    }
}
