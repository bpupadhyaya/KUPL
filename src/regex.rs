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
                    // predefined classes expand into ranges inside `[...]`. The
                    // NEGATED predefined classes (`\D` `\W` `\S`) are refused
                    // here rather than silently falling through to the `other`
                    // arm below (which would treat them as the literal letters
                    // `D`/`W`/`S` -- a real, easy-to-hit footgun: `[\D]` looks
                    // exactly like "any non-digit" to anyone used to
                    // PCRE/JS-style regex, but would have silently matched only
                    // the literal character `D`). A negated class can't be
                    // expressed as a small set of INCLUSIVE ranges the way
                    // `\d`/`\w`/`\s` can without either a per-element negation
                    // flag (this engine's `Atom::Class` has a single flag for
                    // the whole class) or hand-enumerating the complement's
                    // ranges -- a clean compile error is far safer than
                    // shipping a second, subtly-wrong implementation of that
                    // math (production-hardening PR-it658).
                    match c {
                        'D' | 'W' | 'S' => {
                            return Err(format!(
                                "`\\{c}` is not supported inside a character class `[...]` (only `\\d`, `\\w`, `\\s`, and single-char escapes are)"
                            ));
                        }
                        'd' => ranges.push(('0', '9')),
                        'w' => {
                            ranges.extend([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]);
                        }
                        's' => {
                            ranges.extend([(' ', ' '), ('\t', '\t'), ('\n', '\n'), ('\r', '\r')]);
                        }
                        // single-char escapes (`\n \t \r` or an escaped literal
                        // like `\.`) resolve to ONE character, so -- unlike
                        // `\d`/`\w`/`\s` above -- they're eligible as either
                        // endpoint of a `lo-hi` range, same as a plain char
                        // (production-hardening PR-it659; previously this arm
                        // pushed a single-char range immediately and never
                        // looked for a following `-`, so `[\t-\r]` silently
                        // parsed as three separate members -- tab, literal
                        // `-`, and CR -- instead of the tab-through-CR range
                        // the syntax visually promises).
                        'n' => self.finish_class_member('\n', &mut ranges)?,
                        't' => self.finish_class_member('\t', &mut ranges)?,
                        'r' => self.finish_class_member('\r', &mut ranges)?,
                        other => self.finish_class_member(other, &mut ranges)?,
                    }
                }
                Some(lo) => {
                    self.pos += 1;
                    self.finish_class_member(lo, &mut ranges)?;
                }
            }
        }
        Ok(Atom::Class { negated, ranges })
    }

    /// Given one already-consumed class member `lo` (a plain char or a
    /// resolved single-char escape), check for a `-hi` range continuation
    /// (a `-` followed by a non-`]`) and push either the resulting range or
    /// `lo` alone. `hi` may itself be a plain char or a single-char escape
    /// (`[\t-\r]`, `[a-\n]`); `\d`/`\w`/`\s`/`\D`/`\W`/`\S` are rejected as a
    /// range endpoint (they don't resolve to one character) rather than
    /// silently taking the raw `\` byte as the boundary.
    fn finish_class_member(&mut self, lo: char, ranges: &mut Vec<(char, char)>) -> Result<(), String> {
        if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&c| c != ']') {
            self.pos += 1; // consume '-'
            let hi = self.range_endpoint()?;
            if lo <= hi {
                ranges.push((lo, hi));
            } else {
                ranges.push((hi, lo));
            }
        } else {
            ranges.push((lo, lo));
        }
        Ok(())
    }

    /// Read one character-class range endpoint at the current position: a
    /// plain character, or (after a leading `\`) a single-char escape.
    fn range_endpoint(&mut self) -> Result<char, String> {
        if self.peek() == Some('\\') {
            self.pos += 1;
            let c = self.peek().ok_or("dangling `\\` in class")?;
            self.pos += 1;
            match c {
                'd' | 'D' | 'w' | 'W' | 's' | 'S' => Err(format!(
                    "`\\{c}` cannot be used as a character-class range endpoint"
                )),
                'n' => Ok('\n'),
                't' => Ok('\t'),
                'r' => Ok('\r'),
                other => Ok(other),
            }
        } else {
            let c = self.peek().ok_or("unclosed character class `[`")?;
            self.pos += 1;
            Ok(c)
        }
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

    /// A REAL BUG found+fixed (production-hardening PR-it658): `\D`/`\W`/`\S`
    /// (the negated predefined classes) work fine OUTSIDE a character class
    /// (`escape()` handles them), but char_class()'s own inline escape match
    /// had NO arm for the uppercase letters -- they silently fell through to
    /// the `other => ranges.push((other, other))` catch-all, so `[\D]`
    /// matched only the LITERAL character `D`, not "any non-digit" as anyone
    /// used to PCRE/JS-style regex would expect from syntax that looks
    /// identical to the well-supported `[\d]`. Rather than attempt to make
    /// `\D`/`\W`/`\S` actually WORK inside `[...]` (their complement can't be
    /// expressed as a small set of inclusive ranges without either a
    /// per-element negation flag this engine's `Atom::Class` doesn't have, or
    /// hand-enumerating the complement's ranges -- real feature work with
    /// real risk of a NEW, subtler bug), a clean compile error is far safer
    /// than silently matching the wrong thing.
    #[test]
    fn negated_predefined_class_inside_a_character_class_is_a_clean_error_not_a_silent_wrong_match() {
        // BEFORE the fix, this compiled successfully and matched only "D".
        assert!(compile("[\\D]").is_err());
        assert!(compile("[\\W]").is_err());
        assert!(compile("[\\S]").is_err());
        // the lowercase (non-negated) forms are unaffected.
        assert!(m("^[\\d]+$", "123"));
        assert!(m("^[\\w]+$", "a_1"));
        assert!(m("^[\\s]+$", " \t"));
        // combined with other members in the same class still errors.
        assert!(compile("[a\\Dz]").is_err());
    }

    /// A REAL BUG found+fixed (production-hardening PR-it659): a single-char
    /// escape (`\n \t \r`, or an escaped literal) used as either endpoint of
    /// a character-class range used to be taken LITERALLY instead of
    /// resolved -- `[\t-\r]` parsed as three separate members (tab, literal
    /// `-`, and CR) instead of the tab-through-CR range the syntax visually
    /// promises (found while auditing PR-it658's neighboring code). Fixed by
    /// routing BOTH the `lo` and `hi` sides of a range through a shared
    /// `range_endpoint` helper that resolves a single-char escape the same
    /// way a plain char is used; `\d`/`\D`/`\w`/`\W`/`\s`/`\S` (which don't
    /// resolve to ONE character) are refused as a range endpoint with a
    /// clean error instead of silently taking the raw `\` byte as the bound
    /// (the SAME "clean rejection over silent wrong output" philosophy as
    /// it658, applied to the endpoint side of a range this time).
    #[test]
    fn escaped_single_chars_compose_into_a_real_range_not_three_separate_members() {
        // BEFORE the fix, `[\t-\r]` matched tab, `-`, or CR individually --
        // never the code points strictly between them (`\n`, 0x0B, 0x0C).
        assert!(m("^[\\t-\\r]$", "\t"));
        assert!(m("^[\\t-\\r]$", "\n")); // 0x0A -- strictly between tab and CR
        assert!(m("^[\\t-\\r]$", "\r"));
        assert!(!m("^[\\t-\\r]$", "a"));
        // BEFORE the fix, `-` between tab and CR would ALSO have matched (as
        // its own literal member) -- now it's consumed as the range
        // operator, not a member.
        assert!(!m("^[\\t-\\r]$", "-"));
        // an escape as the LOW endpoint composes too, and order still
        // normalizes regardless of which side is numerically smaller.
        assert!(m("^[\\n-a]$", "T")); // '\n'(0x0A)..'a'(0x61) covers 'T'
        // a multi-range escape (`\d` etc.) can't be the HI side of a range
        // (it doesn't resolve to one character). Note `\d`/`\w`/`\s` as the
        // LO side were never range-eligible in the first place, before or
        // after this fix -- they push their full expansion immediately and
        // are never followed up with a `-` lookahead, same as always.
        assert!(compile("[a-\\d]").is_err());
        // a trailing escape right before `]` still isn't treated as a range
        // (nothing follows the `-`... here there's no `-` at all, so this
        // just confirms the single-escape-as-sole-member case still works).
        assert!(m("^[\\t]$", "\t"));
    }
}
