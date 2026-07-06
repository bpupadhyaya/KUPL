//! A small, self-contained backtracking regular-expression engine, shared by
//! the interpreter and the KVM (zero dependencies, like `src/json.rs`).
//!
//! Supported syntax:
//!   - literal characters and `.` (any char except none — matches any single char)
//!   - quantifiers `*` `+` `?` (greedy)
//!   - character classes `[abc]`, ranges `[a-z]`, negation `[^…]`
//!   - predefined classes `\d \D \w \W \s \S`
//!   - anchors `^` (start) and `$` (end)
//!   - alternation `a|b` and grouping `(...)`
//!   - escapes for metacharacters: `\. \* \+ \? \( \) \[ \] \| \\ \^ \$ \n \t \r`
//!
//! Semantics are **search** (partial match anywhere in the text); wrap a pattern
//! in `^…$` for a full-string match. Matching is greedy with backtracking.
//! An invalid pattern is reported as `Err(message)` by `compile`.

/// A compiled pattern: a sequence of alternatives, each a sequence of atoms.
#[derive(Debug)]
pub struct Regex {
    /// Top-level alternation: any branch may match.
    alts: Vec<Vec<Piece>>,
    anchored_start: bool,
    anchored_end: bool,
}

#[derive(Debug, Clone)]
struct Piece {
    atom: Atom,
    quant: Quant,
}

#[derive(Debug, Clone)]
enum Atom {
    Any,
    Char(char),
    Class { negated: bool, ranges: Vec<(char, char)> },
    Group(Vec<Vec<Piece>>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quant {
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// Backtracking-step budget for one match operation. A pathological pattern over
/// a long non-matching input (e.g. `a*a*a*a*c`, which forces O(n^k) ways to split
/// the run across k quantifiers) would otherwise hang exponentially (ReDoS). When
/// the budget is exhausted the match unwinds and `budget_exceeded()` reports it, so
/// the caller can raise a clean error instead of the process hanging. Generous
/// enough that ordinary matches never approach it. Mirrored in the native runtime.
const MATCH_BUDGET: u64 = 10_000_000;

thread_local! {
    static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BUDGET_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn reset_budget() {
    STEPS.with(|s| s.set(MATCH_BUDGET));
    BUDGET_HIT.with(|h| h.set(false));
}

/// Consume one step; false (and flags the hit) once the budget is exhausted.
fn tick() -> bool {
    STEPS.with(|s| {
        let n = s.get();
        if n == 0 {
            BUDGET_HIT.with(|h| h.set(true));
            false
        } else {
            s.set(n - 1);
            true
        }
    })
}

/// Whether the most recent match aborted after exceeding the step budget.
pub fn budget_exceeded() -> bool {
    BUDGET_HIT.with(|h| h.get())
}

pub fn compile(pattern: &str) -> Result<Regex, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut p = Parser::new(&chars);
    let anchored_start = p.eat('^');
    let alts = p.alternation()?;
    if p.pos != p.chars.len() {
        return Err(format!("unexpected `{}` in pattern", p.chars[p.pos]));
    }
    let anchored_end = p.anchored_end;
    Ok(Regex { alts, anchored_start, anchored_end })
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    anchored_end: bool,
}

impl<'a> Parser<'a> {
    fn new(chars: &'a [char]) -> Self {
        Parser { chars, pos: 0, anchored_end: false }
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// One or more sequences separated by `|`.
    fn alternation(&mut self) -> Result<Vec<Vec<Piece>>, String> {
        let mut alts = vec![self.sequence()?];
        while self.eat('|') {
            alts.push(self.sequence()?);
        }
        Ok(alts)
    }

    /// A run of quantified atoms until `|`, `)`, or end.
    fn sequence(&mut self) -> Result<Vec<Piece>, String> {
        let mut pieces = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                Some('$') if self.pos + 1 == self.chars.len() => {
                    // `$` only anchors at the very end of the whole pattern
                    self.pos += 1;
                    self.anchored_end = true;
                    break;
                }
                _ => {
                    let atom = self.atom()?;
                    let quant = self.quantifier();
                    pieces.push(Piece { atom, quant });
                }
            }
        }
        Ok(pieces)
    }

    fn quantifier(&mut self) -> Quant {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Quant::ZeroOrMore
            }
            Some('+') => {
                self.pos += 1;
                Quant::OneOrMore
            }
            Some('?') => {
                self.pos += 1;
                Quant::ZeroOrOne
            }
            _ => Quant::One,
        }
    }

    fn atom(&mut self) -> Result<Atom, String> {
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let alts = self.alternation()?;
                if !self.eat(')') {
                    return Err("unclosed group `(`".into());
                }
                Ok(Atom::Group(alts))
            }
            Some('[') => self.char_class(),
            Some('.') => {
                self.pos += 1;
                Ok(Atom::Any)
            }
            Some('\\') => {
                self.pos += 1;
                self.escape()
            }
            Some(')') | Some('|') => Err("unexpected metacharacter".into()),
            Some('*') | Some('+') | Some('?') => {
                Err("quantifier with nothing to repeat".into())
            }
            Some(c) => {
                self.pos += 1;
                Ok(Atom::Char(c))
            }
            None => Err("unexpected end of pattern".into()),
        }
    }

    /// After a backslash: predefined class or escaped literal.
    fn escape(&mut self) -> Result<Atom, String> {
        let c = self.peek().ok_or("dangling `\\` at end of pattern")?;
        self.pos += 1;
        Ok(match c {
            'd' => Atom::Class { negated: false, ranges: vec![('0', '9')] },
            'D' => Atom::Class { negated: true, ranges: vec![('0', '9')] },
            'w' => Atom::Class {
                negated: false,
                ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            },
            'W' => Atom::Class {
                negated: true,
                ranges: vec![('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')],
            },
            's' => Atom::Class {
                negated: false,
                ranges: vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            },
            'S' => Atom::Class {
                negated: true,
                ranges: vec![(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')],
            },
            'n' => Atom::Char('\n'),
            't' => Atom::Char('\t'),
            'r' => Atom::Char('\r'),
            other => Atom::Char(other), // escaped metacharacter → literal
        })
    }

    fn char_class(&mut self) -> Result<Atom, String> {
        self.pos += 1; // consume '['
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        // a `]` immediately after `[` or `[^` is a literal
        if self.peek() == Some(']') {
            ranges.push((']', ']'));
            self.pos += 1;
        }
        loop {
            match self.peek() {
                None => return Err("unclosed character class `[`".into()),
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    let c = self.peek().ok_or("dangling `\\` in class")?;
                    self.pos += 1;
                    // predefined classes expand into ranges inside `[...]`
                    match c {
                        'd' => ranges.push(('0', '9')),
                        'w' => {
                            ranges.extend([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]);
                        }
                        's' => {
                            ranges.extend([(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')]);
                        }
                        'n' => ranges.push(('\n', '\n')),
                        't' => ranges.push(('\t', '\t')),
                        'r' => ranges.push(('\r', '\r')),
                        other => ranges.push((other, other)),
                    }
                }
                Some(lo) => {
                    self.pos += 1;
                    // range `a-z` when a `-` is followed by a non-`]`
                    if self.peek() == Some('-')
                        && self.chars.get(self.pos + 1).is_some_and(|&c| c != ']')
                    {
                        self.pos += 1; // consume '-'
                        let hi = self.peek().unwrap();
                        self.pos += 1;
                        if lo <= hi {
                            ranges.push((lo, hi));
                        } else {
                            ranges.push((hi, lo));
                        }
                    } else {
                        ranges.push((lo, lo));
                    }
                }
            }
        }
        Ok(Atom::Class { negated, ranges })
    }
}

impl Regex {
    /// Does the pattern match anywhere in `text`? (Use `^…$` for a full match.)
    pub fn is_match(&self, text: &str) -> bool {
        reset_budget();
        self.find_at_from(text).is_some()
    }

    /// The first (leftmost) matching substring, if any.
    pub fn find(&self, text: &str) -> Option<String> {
        reset_budget();
        let chars: Vec<char> = text.chars().collect();
        let (start, end) = self.leftmost(&chars)?;
        Some(chars[start..end].iter().collect())
    }

    /// All non-overlapping matches, left to right. A zero-width match advances
    /// by one character to guarantee progress.
    pub fn find_all(&self, text: &str) -> Vec<String> {
        reset_budget();
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i <= chars.len() {
            if let Some(end) = self.match_here(&chars, i) {
                out.push(chars[i..end].iter().collect());
                i = if end > i { end } else { i + 1 };
            } else if self.anchored_start {
                break; // ^ can only match at the current search origin
            } else {
                i += 1;
            }
        }
        out
    }

    /// Replace every non-overlapping match with `replacement` (literal text).
    pub fn replace_all(&self, text: &str, replacement: &str) -> String {
        reset_budget();
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            if let Some(end) = self.match_here(&chars, i) {
                out.push_str(replacement);
                if end > i {
                    i = end;
                } else {
                    out.push(chars[i]); // zero-width match: emit char, advance
                    i += 1;
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        // a trailing zero-width match at end-of-string
        if i == chars.len() && self.match_here(&chars, i) == Some(i) {
            out.push_str(replacement);
        }
        out
    }

    fn leftmost(&self, chars: &[char]) -> Option<(usize, usize)> {
        let starts: Vec<usize> = if self.anchored_start { vec![0] } else { (0..=chars.len()).collect() };
        for start in starts {
            if let Some(end) = self.match_here(chars, start) {
                return Some((start, end));
            }
        }
        None
    }

    fn find_at_from(&self, text: &str) -> Option<usize> {
        let chars: Vec<char> = text.chars().collect();
        self.leftmost(&chars).map(|(_, e)| e)
    }

    /// Try to match starting exactly at `pos`; return the end index of the
    /// longest greedy match, honoring `$`.
    fn match_here(&self, chars: &[char], pos: usize) -> Option<usize> {
        for alt in &self.alts {
            if let Some(end) = match_seq(alt, chars, pos) {
                if !self.anchored_end || end == chars.len() {
                    return Some(end);
                }
            }
        }
        None
    }
}

/// Match a sequence of pieces starting at `pos`, returning the end index.
fn match_seq(pieces: &[Piece], chars: &[char], pos: usize) -> Option<usize> {
    // Every recursive descent passes through here; charge a step so a pathological
    // backtracking blow-up unwinds at the budget instead of hanging (see tick()).
    if !tick() {
        return None;
    }
    match pieces.split_first() {
        None => Some(pos),
        Some((first, rest)) => match_piece(first, rest, chars, pos),
    }
}

/// Match `piece` then `rest`, with backtracking over greedy quantifiers.
fn match_piece(piece: &Piece, rest: &[Piece], chars: &[char], pos: usize) -> Option<usize> {
    match piece.quant {
        Quant::One => {
            let np = atom_match(&piece.atom, chars, pos)?;
            match_seq(rest, chars, np)
        }
        Quant::ZeroOrOne => {
            // greedy: try consuming one first, then zero
            if let Some(np) = atom_match(&piece.atom, chars, pos) {
                if let Some(end) = match_seq(rest, chars, np) {
                    return Some(end);
                }
            }
            match_seq(rest, chars, pos)
        }
        Quant::ZeroOrMore | Quant::OneOrMore => {
            // collect greedily, then backtrack
            let mut ends = vec![pos];
            let mut cur = pos;
            while let Some(np) = atom_match(&piece.atom, chars, cur) {
                if np == cur {
                    break; // guard against zero-width infinite loop
                }
                cur = np;
                ends.push(cur);
            }
            let min = if piece.quant == Quant::OneOrMore { 1 } else { 0 };
            // try the longest run first (greedy), backtrack toward `min`
            for k in (min..ends.len()).rev() {
                if let Some(end) = match_seq(rest, chars, ends[k]) {
                    return Some(end);
                }
            }
            None
        }
    }
}

/// Match a single atom at `pos`; return the position after it, or None.
fn atom_match(atom: &Atom, chars: &[char], pos: usize) -> Option<usize> {
    match atom {
        Atom::Any => {
            if pos < chars.len() {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Char(c) => {
            if chars.get(pos) == Some(c) {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Class { negated, ranges } => {
            let ch = *chars.get(pos)?;
            let inside = ranges.iter().any(|&(lo, hi)| ch >= lo && ch <= hi);
            if inside != *negated {
                Some(pos + 1)
            } else {
                None
            }
        }
        Atom::Group(alts) => {
            for alt in alts {
                if let Some(end) = match_seq(alt, chars, pos) {
                    return Some(end);
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        compile(p).unwrap().is_match(t)
    }
    fn f(p: &str, t: &str) -> Option<String> {
        compile(p).unwrap().find(t)
    }

    #[test]
    fn literals_and_dot() {
        assert!(m("abc", "xxabcyy"));
        assert!(!m("abc", "abx"));
        assert!(m("a.c", "axc"));
        assert!(!m("a.c", "ac"));
    }

    #[test]
    fn anchors() {
        assert!(m("^abc", "abcdef"));
        assert!(!m("^abc", "xabc"));
        assert!(m("abc$", "xxabc"));
        assert!(!m("abc$", "abcx"));
        assert!(m("^abc$", "abc"));
        assert!(!m("^abc$", "abcd"));
    }

    #[test]
    fn quantifiers() {
        assert!(m("^a*$", ""));
        assert!(m("^a*$", "aaaa"));
        assert!(m("^ab+c$", "abbbc"));
        assert!(!m("^ab+c$", "ac"));
        assert!(m("^colou?r$", "color"));
        assert!(m("^colou?r$", "colour"));
        assert!(!m("^colou?r$", "colouur"));
    }

    #[test]
    fn redos_pattern_is_bounded_not_a_hang() {
        // A pathological pattern over a long non-matching input (O(n^k) ways to
        // split the run across k quantifiers) must abort at the step budget instead
        // of hanging exponentially. is_match returns fast and budget_exceeded() flags
        // it (the interpreter turns that into a clean error).
        let re = super::compile("a*a*a*a*c").unwrap();
        let big: String = "a".repeat(400);
        let _ = re.is_match(&big); // must return quickly, not hang
        assert!(super::budget_exceeded(), "expected the ReDoS budget to trip");
        // a normal match over the same input does NOT trip the budget
        let ok = super::compile("a+c").unwrap();
        let _ = ok.is_match(&big);
        assert!(!super::budget_exceeded(), "a linear match must not trip the budget");
    }

    #[test]
    fn classes() {
        assert!(m("^[abc]+$", "cabba"));
        assert!(!m("^[abc]+$", "cabxa"));
        assert!(m("^[a-z]+$", "hello"));
        assert!(!m("^[a-z]+$", "Hello"));
        assert!(m("^[^0-9]+$", "abc"));
        assert!(!m("^[^0-9]+$", "ab3"));
        // predefined classes expand inside `[...]`
        assert!(m("^[\\w.]+$", "a.b_9"));
        assert!(!m("^[\\w.]+$", "a b"));
        assert_eq!(f("@[\\w.]+", "ada@math.org here"), Some("@math.org".to_string()));
        assert!(m("^\\d+$", "12345"));
        assert!(!m("^\\d+$", "12a45"));
        assert!(m("^\\w+$", "a_9Z"));
    }

    #[test]
    fn alternation_and_groups() {
        assert!(m("^(cat|dog)$", "cat"));
        assert!(m("^(cat|dog)$", "dog"));
        assert!(!m("^(cat|dog)$", "cow"));
        assert!(m("^(ab)+$", "ababab"));
        assert!(!m("^(ab)+$", "aba"));
        assert!(m("^a(b|c)*d$", "abcbcd"));
    }

    #[test]
    fn find_and_extract() {
        assert_eq!(f("\\d+", "abc123def456"), Some("123".to_string()));
        assert_eq!(f("z+", "abc"), None);
        let re = compile("\\d+").unwrap();
        assert_eq!(re.find_all("a1bb22ccc333"), vec!["1", "22", "333"]);
    }

    #[test]
    fn replace() {
        let re = compile("\\d+").unwrap();
        assert_eq!(re.replace_all("a1b22c333", "#"), "a#b#c#");
        let sp = compile("\\s+").unwrap();
        assert_eq!(sp.replace_all("a  b\tc", "_"), "a_b_c");
    }

    #[test]
    fn escapes() {
        assert!(m("^a\\.b$", "a.b"));
        assert!(!m("^a\\.b$", "axb"));
        assert!(m("^\\(x\\)$", "(x)"));
    }

    #[test]
    fn invalid_patterns() {
        assert!(compile("(abc").is_err());
        assert!(compile("[a-z").is_err());
        assert!(compile("*abc").is_err());
    }
}
