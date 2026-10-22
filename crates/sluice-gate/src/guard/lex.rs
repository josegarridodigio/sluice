//! A deliberately small shell tokenizer for the guard rewrite.
//!
//! [`split`] cuts a command line into simple commands ([`Segment`]s) with byte
//! offsets into the original string. It models only what the rewrite needs
//! and refuses (returns `None`) everything else, so a rewrite never relies on
//! a guess about how the shell parses the text. Design:
//! `kb/sluice/design/command-control.md` section 4.2.

/// Longest command the tokenizer accepts.
pub const MAX_LEN: usize = 4096;

/// Words that start compound commands; a segment starting with one makes the
/// whole command unparseable for the guard.
const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until", "do",
    "done", "function", "coproc", "in", "!", "[[", "]]",
];

/// The operator that ends a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `;`
    Semi,
    /// `|`
    Pipe,
}

/// One word (an assignment or an argument; redirections are not words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Byte offset of the first character in the original string.
    pub start: usize,
    /// Byte offset just past the last character.
    pub end: usize,
    /// The word with its quotes removed.
    pub value: String,
    /// True when any part of it was quoted.
    pub quoted: bool,
}

/// A simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Byte offset of its first token.
    pub start: usize,
    /// Byte offset just past its last token (operator excluded).
    pub end: usize,
    /// Leading assignments followed by the command words.
    pub words: Vec<Word>,
    /// How many of `words` are leading `VAR=value` assignments.
    pub assignments: usize,
    /// Number of output redirections (`>`, `>>`, `2>&1`, `&>` ...).
    pub redirections: usize,
    /// The operator that ends it (`None` for the last segment).
    pub op: Option<Op>,
}

impl Segment {
    /// The command words (after the assignments).
    pub fn command_words(&self) -> &[Word] {
        &self.words[self.assignments..]
    }
}

fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// Bytes that end an unquoted word.
fn is_meta(b: u8) -> bool {
    is_blank(b) || matches!(b, b'&' | b'|' | b';' | b'<' | b'>' | b'(' | b')')
}

fn is_assignment(raw: &str) -> bool {
    let Some((name, _)) = raw.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct Lexer<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl Lexer<'_> {
    fn peek(&self, off: usize) -> Option<u8> {
        self.b.get(self.i + off).copied()
    }

    fn skip_blanks(&mut self) {
        while self.peek(0).is_some_and(is_blank) {
            self.i += 1;
        }
    }

    /// Reads one word starting at `self.i` (not at a meta byte).
    fn word(&mut self) -> Option<Word> {
        let start = self.i;
        let mut value = String::new();
        let mut quoted = false;
        if self.peek(0) == Some(b'#') {
            return None; // a comment
        }
        while let Some(c) = self.peek(0) {
            match c {
                _ if is_meta(c) => break,
                b'\'' => {
                    quoted = true;
                    let close = self.s[self.i + 1..].find('\'')? + self.i + 1;
                    value.push_str(&self.s[self.i + 1..close]);
                    self.i = close + 1;
                }
                b'"' => {
                    quoted = true;
                    let close = self.s[self.i + 1..].find('"')? + self.i + 1;
                    let inner = &self.s[self.i + 1..close];
                    if inner.bytes().any(|b| matches!(b, b'$' | b'`' | b'\\')) {
                        return None;
                    }
                    value.push_str(inner);
                    self.i = close + 1;
                }
                b'\\' | b'`' | b'{' | b'}' => return None,
                b'$' => {
                    if matches!(self.peek(1), Some(b'(' | b'{' | b'[' | b'\'' | b'"') | None) {
                        return None;
                    }
                    value.push('$');
                    self.i += 1;
                }
                _ => {
                    // Copy one UTF-8 character.
                    let ch = self.s[self.i..].chars().next()?;
                    value.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
        (self.i > start).then_some(Word {
            start,
            end: self.i,
            value,
            quoted,
        })
    }

    /// Reads a redirection whose operator starts at `self.i` (`>` or `&>`);
    /// returns the end offset of its target.
    fn redirection(&mut self) -> Option<usize> {
        if self.peek(0) == Some(b'&') {
            self.i += 1; // `&>` / `&>>`
        }
        if self.peek(0) != Some(b'>') {
            return None;
        }
        self.i += 1;
        match self.peek(0) {
            Some(b'>' | b'|' | b'&') => self.i += 1,
            Some(b'(') => return None, // process substitution
            _ => {}
        }
        self.skip_blanks();
        match self.peek(0) {
            Some(c) if !is_meta(c) => self.word().map(|w| w.end),
            _ => None,
        }
    }
}

/// Splits `command` into simple commands, or `None` when it uses anything
/// the guard does not model: newlines or other control characters,
/// backslashes, heredocs and every input redirection (`<`), process and
/// command substitution (`$(`, backticks, `<(`, `>(`), subshells and groups
/// (`(`, `)`, `{`, `}`), a background `&`, `|&`, `;;`, comments, `$'...'`,
/// `$`, backticks or backslashes inside double quotes, unbalanced quotes,
/// compound-command keywords, empty commands, more than [`MAX_LEN`] bytes.
pub fn split(command: &str) -> Option<Vec<Segment>> {
    if command.len() > MAX_LEN
        || command
            .bytes()
            .any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)
    {
        return None;
    }
    let mut lx = Lexer {
        s: command,
        b: command.as_bytes(),
        i: 0,
    };
    let mut segments = Vec::new();
    let mut cur: Option<Segment> = None;
    let new_seg = |start: usize| Segment {
        start,
        end: start,
        words: Vec::new(),
        assignments: 0,
        redirections: 0,
        op: None,
    };
    loop {
        lx.skip_blanks();
        let Some(c) = lx.peek(0) else { break };
        let op = match (c, lx.peek(1)) {
            (b'&', Some(b'&')) => Some((Op::And, 2)),
            (b'|', Some(b'|')) => Some((Op::Or, 2)),
            (b'|', Some(b'&')) | (b';', Some(b';' | b'&')) => return None,
            (b'|', _) => Some((Op::Pipe, 1)),
            (b';', _) => Some((Op::Semi, 1)),
            _ => None,
        };
        if let Some((op, len)) = op {
            let mut seg = cur.take()?; // an operator with no command before it
            seg.op = Some(op);
            segments.push(seg);
            lx.i += len;
            continue;
        }
        let start = lx.i;
        let seg = cur.get_or_insert_with(|| new_seg(start));
        match c {
            b'&' if lx.peek(1) == Some(b'>') => {
                seg.end = lx.redirection()?;
                seg.redirections += 1;
            }
            b'>' => {
                seg.end = lx.redirection()?;
                seg.redirections += 1;
            }
            b'&' | b'<' | b'(' | b')' => return None,
            _ => {
                let word = lx.word()?;
                let fd_prefix = !word.quoted
                    && word.value.bytes().all(|b| b.is_ascii_digit())
                    && lx.peek(0) == Some(b'>');
                if fd_prefix {
                    seg.end = lx.redirection()?;
                    seg.redirections += 1;
                } else {
                    seg.end = word.end;
                    let raw = &command[word.start..word.end];
                    if seg.assignments == seg.words.len() && is_assignment(raw) {
                        seg.assignments += 1;
                    }
                    seg.words.push(word);
                }
            }
        }
    }
    match cur {
        Some(seg) => segments.push(seg),
        // Only a trailing `;` may end the command.
        None => match segments.last() {
            Some(last) if last.op == Some(Op::Semi) => {}
            _ => return None,
        },
    }
    let keyword = segments.iter().any(|seg| {
        seg.command_words()
            .first()
            .is_some_and(|w| !w.quoted && KEYWORDS.contains(&w.value.as_str()))
    });
    (!keyword).then_some(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders segments as `words|op` with `=` marking assignments and
    /// `>N` the number of redirections, for compact fixtures.
    fn shape(cmd: &str) -> Option<String> {
        let segs = split(cmd)?;
        Some(
            segs.iter()
                .map(|s| {
                    let mut out: Vec<String> = s
                        .words
                        .iter()
                        .enumerate()
                        .map(|(i, w)| {
                            if i < s.assignments {
                                format!("={}", w.value)
                            } else {
                                w.value.clone()
                            }
                        })
                        .collect();
                    if s.redirections > 0 {
                        out.push(format!(">{}", s.redirections));
                    }
                    let op = match s.op {
                        None => "",
                        Some(Op::And) => " &&",
                        Some(Op::Or) => " ||",
                        Some(Op::Semi) => " ;",
                        Some(Op::Pipe) => " |",
                    };
                    format!("[{}]{op}", out.join(","))
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    #[test]
    fn parses() {
        for (cmd, want) in [
            ("make", "[make]"),
            ("  make   -j8  ", "[make,-j8]"),
            ("make && make install", "[make] && [make,install]"),
            ("a || b; c | d", "[a] || [b] ; [c] | [d]"),
            ("a&&b", "[a] && [b]"),
            ("make;", "[make] ;"),
            ("A=1 B=x make", "[=A=1,=B=x,make]"),
            ("make A=1", "[make,A=1]"),
            ("A=\"x y\" make", "[=A=x y,make]"),
            ("echo 'a && b'", "[echo,a && b]"),
            ("echo \"a | b\"", "[echo,a | b]"),
            ("docker build -t \"my app\" .", "[docker,build,-t,my app,.]"),
            ("echo it''s", "[echo,its]"),
            (
                "cargo build 2>&1 | tail -20",
                "[cargo,build,>1] | [tail,-20]",
            ),
            ("make > out.txt 2> err.txt", "[make,>2]"),
            ("make >> log", "[make,>1]"),
            ("make &> log", "[make,>1]"),
            ("make &>> log", "[make,>1]"),
            ("make >| log", "[make,>1]"),
            ("make>log", "[make,>1]"),
            ("echo $HOME ~/x *.rs", "[echo,$HOME,~/x,*.rs]"),
            ("echo a=b", "[echo,a=b]"),
            ("1=x make", "[1=x,make]"),
            ("echo héllo 'ñ'", "[echo,héllo,ñ]"),
            ("echo a#b", "[echo,a#b]"),
            ("\tmake\t", "[make]"),
        ] {
            assert_eq!(shape(cmd).as_deref(), Some(want), "{cmd:?}");
        }
    }

    #[test]
    fn refuses() {
        for cmd in [
            "",
            "   ",
            "make\nmake",
            "make \\\n install",
            "make\r",
            "cat <<EOF",
            "make < in",
            "diff <(a) b",
            "tee >(cat)",
            "echo $(date)",
            "echo `date`",
            "echo \"$(date)\"",
            "echo \"$HOME\"",
            "echo \"a\\\"b\"",
            "(make)",
            "{ make; }",
            "echo ${HOME}",
            "echo {a,b}",
            "make &",
            "make & ls",
            "make |& tee",
            "case x in a) make;; esac",
            "if make; then ls; fi",
            "for f in a; do make; done",
            "while true; do make; done",
            "! make",
            "echo 'unbalanced",
            "echo \"unbalanced",
            "make # comment",
            "echo \\$x",
            "echo $'a'",
            "&& make",
            "make &&",
            "make ||",
            "make |",
            "; make",
            "make ;; ls",
            "make >",
            "make > && ls",
            "make 2>",
            "echo $",
            "echo $[1]",
            "\u{7f}",
        ] {
            assert_eq!(shape(cmd), None, "{cmd:?}");
        }
        assert_eq!(split(&"a ".repeat(MAX_LEN / 2 + 1)), None);
        assert!(split(&"a ".repeat(MAX_LEN / 2)).is_some());
    }

    #[test]
    fn offsets_point_into_the_original() {
        let cmd = "cd web && RUSTFLAGS=-D cargo  build 2>&1 | tail";
        let segs = split(cmd).unwrap();
        assert_eq!(segs.len(), 3);
        assert_eq!(&cmd[segs[0].start..segs[0].end], "cd web");
        assert_eq!(
            &cmd[segs[1].start..segs[1].end],
            "RUSTFLAGS=-D cargo  build 2>&1"
        );
        let cargo = &segs[1].command_words()[0];
        assert_eq!(&cmd[cargo.start..cargo.end], "cargo");
        assert_eq!(&cmd[segs[2].start..segs[2].end], "tail");
        let quoted = "docker build -t \"my app\" .";
        let w = &split(quoted).unwrap()[0].words[3];
        assert_eq!(&quoted[w.start..w.end], "\"my app\"");
        assert!(w.quoted);
    }
}
