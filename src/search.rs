//! Search over a finished scan's arena. Pure `std` (no egui), so it is unit
//! tested everywhere.
//!
//! Query syntax, terms separated by spaces (quote a term to keep spaces):
//!   `*.log`  `report?.txt`  `backup`   file name; `*` and `?` are wildcards,
//!                                      a term without wildcards matches anywhere
//!   `type:jpg,png`  `type:folder`      extensions, `folder` or `file`
//!   `size:>100MB`  `size:<=2GB`  `size:5MB`            bigger / smaller / exactly
//!   `modified:>2024-05-17`  `created:<2023-01`  `date:2024`
//!                                      after / before / on that day (month, year);
//!                                      `date:` is short for `modified:`
//! Every term must match (AND), so `*.mp4 size:>1GB` is "mp4 files over 1 GiB".

use crate::scanner::Node;
use std::time::{Duration, Instant};

/// What the toolbar's "insert" dropdown offers: (label, text put in the box).
pub const PRESETS: &[(&str, &str)] = &[
    ("Size: bigger than", "size:>"),
    ("Size: smaller than", "size:<"),
    ("Size: exactly", "size:"),
    ("Type: extension", "type:"),
    ("Type: folder", "type:folder"),
    ("Modified: after", "modified:>"),
    ("Modified: before", "modified:<"),
    ("Modified: on", "modified:"),
    ("Created: after", "created:>"),
    ("Created: before", "created:<"),
    ("Created: on", "created:"),
];

#[derive(Debug, PartialEq, Clone, Copy)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

/// Splits a leading comparison off `v`: `"<=5MB"` -> `(Le, "5MB")`.
fn split_cmp(v: &str) -> (Cmp, &str) {
    for (p, c) in [(">=", Cmp::Ge), ("<=", Cmp::Le), (">", Cmp::Gt), ("<", Cmp::Lt), ("=", Cmp::Eq)] {
        if let Some(r) = v.strip_prefix(p) {
            return (c, r);
        }
    }
    (Cmp::Eq, v)
}

/// A day, month or year as the half-open range `[start, end)` of Unix seconds.
#[derive(Debug)]
struct DateTerm {
    created: bool,
    cmp: Cmp,
    start: i64,
    end: i64,
}

/// A lowercased name pattern. Most names and patterns are plain ASCII, which
/// can be matched on bytes with no allocation; `chars` is the general path.
#[derive(Debug)]
struct Pat {
    chars: Vec<char>,
    bytes: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct Query {
    /// Each must match the node's name.
    names: Vec<Pat>,
    /// Empty = any type. Otherwise a node must match at least one entry.
    types: Vec<String>,
    sizes: Vec<(Cmp, u64)>,
    dates: Vec<DateTerm>,
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

// Days since 1970-01-01 <-> (year, month, day): Howard Hinnant's civil-date
// algorithms, so no date crate is needed.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD` of a stored timestamp, or `-` when it is unknown.
/// shortcut: UTC days, not the local time zone, switch if a day boundary matters.
pub fn fmt_date(secs: u32) -> String {
    if secs == 0 {
        return "-".to_string();
    }
    let (y, m, d) = civil_from_days(i64::from(secs) / 86_400);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2024`, `2024-05` or `2024-05-17` as the `[start, end)` seconds it covers.
fn parse_date(s: &str) -> Result<(i64, i64), String> {
    let bad = || format!("bad date: {s} (use YYYY, YYYY-MM or YYYY-MM-DD)");
    let p: Vec<i64> = s.split('-').map(|x| x.parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
    let (y, m, d) = (p[0], *p.get(1).unwrap_or(&1), *p.get(2).unwrap_or(&1));
    // Timestamps are u32 seconds, so 1970..=2105; the round trip rejects Feb 30 etc.
    if p.len() > 3 || !(1970..=2105).contains(&y) || civil_from_days(days_from_civil(y, m, d)) != (y, m, d) {
        return Err(bad());
    }
    let start = days_from_civil(y, m, d);
    let end = match p.len() {
        1 => days_from_civil(y + 1, 1, 1),
        2 => days_from_civil(y + i64::from(m == 12), m % 12 + 1, 1),
        _ => start + 1,
    };
    Ok((start * 86_400, end * 86_400))
}

impl Query {
    pub fn parse(input: &str) -> Result<Query, String> {
        let mut q = Query::default();
        for term in split_terms(input) {
            let lower = term.to_lowercase();
            if let Some(v) = lower.strip_prefix("size:") {
                let (cmp, rest) = split_cmp(v);
                q.sizes.push((cmp, parse_size(rest)?));
            } else if let Some(v) = lower.strip_prefix("type:").or_else(|| lower.strip_prefix("ext:")) {
                q.types.extend(
                    v.split(',')
                        .map(|t| t.trim().trim_start_matches('.').to_string())
                        .filter(|t| !t.is_empty()),
                );
            } else if let Some((created, v)) = lower
                .strip_prefix("created:")
                .map(|v| (true, v))
                .or_else(|| lower.strip_prefix("modified:").or_else(|| lower.strip_prefix("date:")).map(|v| (false, v)))
            {
                let (cmp, rest) = split_cmp(v);
                let (start, end) = parse_date(rest)?;
                q.dates.push(DateTerm { created, cmp, start, end });
            } else {
                let mut p: Vec<char> = lower.chars().collect();
                if !p.iter().any(|&c| c == '*' || c == '?') {
                    p.insert(0, '*');
                    p.push('*');
                }
                let bytes = p.iter().all(char::is_ascii).then(|| p.iter().map(|&c| c as u8).collect());
                q.names.push(Pat { chars: p, bytes });
            }
        }
        if q.names.is_empty() && q.types.is_empty() && q.sizes.is_empty() && q.dates.is_empty() {
            return Err("Empty search".to_string());
        }
        Ok(q)
    }

    /// `buf` is scratch space for the non-ASCII path (a lowercased copy of the
    /// name); ASCII names, the common case, are matched in place. Either way a
    /// search over millions of entries does no per-entry allocation, and the
    /// cheap numeric checks run first so most nodes never reach the name.
    pub fn matches(&self, n: &Node, buf: &mut Vec<char>) -> bool {
        if !self.sizes.iter().all(|(cmp, v)| match cmp {
            Cmp::Gt => n.size > *v,
            Cmp::Ge => n.size >= *v,
            Cmp::Lt => n.size < *v,
            Cmp::Le => n.size <= *v,
            Cmp::Eq => n.size == *v,
        }) {
            return false;
        }
        for d in &self.dates {
            let t = i64::from(if d.created { n.created } else { n.modified });
            // 0 means unknown, which matches no date.
            let ok = t != 0
                && match d.cmp {
                    Cmp::Gt => t >= d.end,
                    Cmp::Ge => t >= d.start,
                    Cmp::Lt => t < d.start,
                    Cmp::Le => t < d.end,
                    Cmp::Eq => d.start <= t && t < d.end,
                };
            if !ok {
                return false;
            }
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
        let ascii = n.name.is_ascii();
        let mut filled = false;
        self.names.iter().all(|p| match (&p.bytes, ascii) {
            (Some(b), true) => glob(b, n.name.as_bytes(), b'?', b'*', |c| c.to_ascii_lowercase()),
            _ => {
                if !filled {
                    buf.clear();
                    buf.extend(n.name.chars().flat_map(char::to_lowercase));
                    filled = true;
                }
                glob(&p.chars, buf, '?', '*', |c| c)
            }
        })
    }
}

/// Iterative glob with `*` and `?` (backtracks to the last `*`, so no
/// exponential blow-up on patterns like `*a*a*a*b`).
/// `fold` is applied to each name element before comparing (the pattern is
/// already lowercase), so ASCII names need no lowercased copy.
fn glob<T: Copy + PartialEq>(p: &[T], s: &[T], one: T, any: T, fold: impl Fn(T) -> T) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == one || p[pi] == fold(s[si])) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == any {
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
    p[pi..].iter().all(|&c| c == any)
}

/// Entries each thread examines per round. Big enough that spawning the
/// threads is a small part of the round, small enough that one round is ~1 ms.
const PER_THREAD: usize = 16_384;

/// Examines arena entries from `*cursor` on for about `budget`, appending the
/// matching indices (never the scan root, always in arena order) to `hits`;
/// returns true once every entry has been looked at. Called once per frame by
/// the GUI so a search over millions of entries neither freezes the window
/// nor waits to show results; each round is spread over `threads` threads.
/// Always makes progress, even with a zero budget.
pub fn step(
    arena: &[Node],
    root: usize,
    q: &Query,
    cursor: &mut usize,
    hits: &mut Vec<usize>,
    budget: Duration,
    threads: usize,
) -> bool {
    let start = Instant::now();
    let threads = threads.max(1);
    let scan = |range: std::ops::Range<usize>| -> Vec<usize> {
        let mut buf = Vec::new();
        range.filter(|&i| i != root && q.matches(&arena[i], &mut buf)).collect()
    };
    while *cursor < arena.len() {
        let end = (*cursor + threads * PER_THREAD).min(arena.len());
        let per = (end - *cursor).div_ceil(threads);
        if threads == 1 || per == end - *cursor {
            hits.extend(scan(*cursor..end));
        } else {
            std::thread::scope(|s| {
                let scan = &scan;
                let parts: Vec<_> = (*cursor..end)
                    .step_by(per)
                    .map(|a| s.spawn(move || scan(a..(a + per).min(end))))
                    .collect();
                for p in parts {
                    hits.extend(p.join().unwrap());
                }
            });
        }
        *cursor = end;
        if start.elapsed() >= budget {
            break;
        }
    }
    *cursor >= arena.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, is_dir: bool, size: u64) -> Node {
        Node { name: name.into(), is_dir, size, ..Default::default() }
    }

    /// Unix seconds of a UTC date and hour.
    fn ts(y: i64, m: i64, d: i64, h: i64) -> u32 {
        (days_from_civil(y, m, d) * 86_400 + h * 3600) as u32
    }

    fn hit(q: &str, n: &Node) -> bool {
        Query::parse(q).unwrap().matches(n, &mut Vec::new())
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
    fn dates_before_after_on() {
        assert_eq!(ts(2024, 1, 1, 0), 1_704_067_200); // known-good anchor for the maths
        let f = Node { modified: ts(2024, 5, 17, 12), created: ts(2020, 2, 29, 0), ..node("a.txt", false, 1) };
        // On: the whole day, not just midnight.
        assert!(hit("modified:2024-05-17", &f));
        assert!(hit("modified:=2024-05-17", &f));
        assert!(!hit("modified:2024-05-18", &f));
        // After / before exclude the named day; >= / <= include it.
        assert!(hit("modified:>2024-05-16", &f));
        assert!(!hit("modified:>2024-05-17", &f));
        assert!(hit("modified:>=2024-05-17", &f));
        assert!(hit("modified:<2024-05-18", &f));
        assert!(!hit("modified:<2024-05-17", &f));
        assert!(hit("modified:<=2024-05-17", &f));
        // Months and years, and `date:` meaning modified.
        assert!(hit("modified:2024-05", &f) && hit("date:2024", &f) && !hit("date:2023", &f));
        assert!(hit("modified:>2024-04", &f) && !hit("modified:>2024-05", &f));
        // Created is separate from modified; a range is two terms.
        assert!(hit("created:2020-02-29", &f) && !hit("created:2024", &f));
        assert!(hit("modified:>=2024-01-01 modified:<=2024-12-31 *.txt", &f));
        // Unknown (0) matches no date at all, not even "before".
        assert!(!hit("modified:<2100", &node("old", false, 1)));
        // Bad input.
        for bad in ["modified:2023-02-29", "modified:2024-13", "modified:soon", "modified:1969", "created:2024-1-1-1"] {
            assert!(Query::parse(bad).is_err(), "{bad}");
        }
        assert!(Query::parse("modified:2024-02-29").is_ok());
    }

    #[test]
    fn dates_round_trip_through_text() {
        assert_eq!(fmt_date(ts(2024, 12, 31, 23)), "2024-12-31");
        assert_eq!(fmt_date(ts(2000, 2, 29, 0)), "2000-02-29");
        assert_eq!(fmt_date(0), "-");
    }

    #[test]
    fn step_makes_progress_in_slices_and_skips_root() {
        let arena: Vec<Node> = (0..40_000).map(|i| node(&format!("f{i}.txt"), false, i)).collect();
        let q = Query::parse("type:txt size:>=39000").unwrap();
        let run = |threads| {
            let (mut cursor, mut hits, mut calls) = (0, Vec::new(), 0);
            // Zero budget = one round per call: it must still finish.
            while !step(&arena, 39_500, &q, &mut cursor, &mut hits, Duration::ZERO, threads) {
                calls += 1;
            }
            (hits, calls)
        };
        let (hits, calls) = run(1);
        assert_eq!(calls, 2); // 16384 entries per round on one thread
        assert_eq!(hits.len(), 999); // 39000..=39999 minus the root (39500)
        assert!(!hits.contains(&39_500));
        // Spreading rounds over threads finds exactly the same hits, in the same order.
        let (hits4, calls4) = run(4);
        assert_eq!(hits4, hits);
        assert_eq!(calls4, 0); // all 40000 fit in one 4-thread round
    }

    #[test]
    fn ascii_and_unicode_names_match_the_same_way() {
        let u = node("Größe_ÄÖÜ.TXT", false, 1);
        assert!(hit("*.txt", &u) && hit("größe*", &u) && hit("*äöü*", &u) && hit("gr??e*", &u));
        assert!(!hit("gr?e*", &u));
        // A non-ASCII pattern against an ASCII name falls back to the char path.
        assert!(!hit("ä*", &node("abc", false, 1)));
        assert!(hit("*", &node("abc", false, 1)) && hit("a?c", &node("ABC", false, 1)));
    }

    /// `cargo test --release search_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn search_speed() {
        let arena: Vec<Node> = (0..3_000_000u64)
            .map(|i| Node { modified: 1_700_000_000 + i as u32, ..node(&format!("Report_{i}_final.DAT"), false, i) })
            .collect();
        for q in ["*7*final.dat", "final", "size:>2900000", "modified:>2023-12-01 type:dat"] {
            for threads in [1, num_cpus::get()] {
                let (q2, mut cursor, mut hits) = (Query::parse(q).unwrap(), 0, Vec::new());
                let t = Instant::now();
                step(&arena, 0, &q2, &mut cursor, &mut hits, Duration::from_secs(60), threads);
                println!("{q:32} {threads} thread(s) {:>8} hits in {:?}", hits.len(), t.elapsed());
            }
        }
    }
}
