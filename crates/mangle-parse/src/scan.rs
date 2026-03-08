// Copyright 2024 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Result, anyhow};
use std::io;

use crate::error::{ErrorContext, ScanError};
use crate::quote::{DecodedSequence, unquote};
use crate::token::Token;

// Scanner turns a stream of bytes into a stream of tokens.
pub struct Scanner<R>
where
    R: io::Read,
{
    iter: io::Bytes<io::BufReader<R>>,

    // Peeked char.
    ch: Option<char>,

    pub line: usize,
    pub col: usize,
    /// Byte offset of start of current line.
    pub start_of_line: usize,

    // Text for the last token.
    text: String,

    // Path to file we are parsing. Used for error messages only.
    path: String,
}

impl<R> Scanner<R>
where
    R: io::Read,
{
    pub fn new<P: ToString>(reader: R, path: P) -> Self {
        use io::Read;
        let buf_reader = io::BufReader::new(reader);
        Self {
            iter: buf_reader.bytes(),
            ch: None,
            line: 1,
            col: 0,
            start_of_line: 0,
            text: String::new(),
            path: path.to_string(),
        }
    }

    pub fn get_error_context(&self) -> ErrorContext {
        ErrorContext {
            path: self.path.clone(),
            line: self.line,
            col: self.col,
            start_of_line: self.start_of_line,
        }
    }

    pub fn next_token(&mut self) -> Result<Token> {
        self.next_token_internal()
    }

    fn next_token_internal(&mut self) -> Result<Token> {
        match self.next_char_skip()? {
            Some('=') => Ok(Token::Eq),
            Some(';') => Ok(Token::Semi),
            Some(',') => Ok(Token::Comma),
            Some('!') => match self.peek()? {
                Some('=') => {
                    let _ = self.next_char()?;
                    Ok(Token::BangEq)
                }
                _ => Ok(Token::Bang),
            },
            Some('(') => Ok(Token::LParen),
            Some(')') => Ok(Token::RParen),
            Some('{') => Ok(Token::LBrace),
            Some('}') => Ok(Token::RBrace),
            Some('[') => Ok(Token::LBracket),
            Some(']') => Ok(Token::RBracket),
            Some('≤') => Ok(Token::Le), // unicode \u2264
            Some('<') => match self.peek()? {
                Some('=') => {
                    let _ = self.next_char()?;
                    Ok(Token::Le)
                }
                _ => Ok(Token::Lt),
            },
            Some('≥') => Ok(Token::Ge), // unicode \u2265
            Some('>') => match self.peek()? {
                Some('=') => {
                    let _ = self.next_char()?;
                    Ok(Token::Ge)
                }
                _ => Ok(Token::Gt),
            },
            Some(':') => match self.peek()? {
                Some('-') => {
                    let _ = self.next_char()?;
                    Ok(Token::ColonDash)
                }
                Some(c) if is_ident_start(c) => {
                    // Built-in predicate name like :string:contains
                    self.builtin_predicate()
                }
                _ => Ok(Token::Colon),
            },
            Some('|') => match self.peek()? {
                Some('>') => {
                    let _ = self.next_char()?;
                    Ok(Token::PipeGt)
                }
                _ => Ok(Token::Pipe),
            },
            Some('.') => match self.peek()? {
                Some('A'..='Z') => {
                    let first = self.next_char()?.expect("could not get peeked character.");
                    self.ident_or_dot_ident(first, true)
                }
                _ => Ok(Token::Dot),
            },
            Some('/') => self.name(),
            Some('⟸') => Ok(Token::LongLeftDoubleArrow),
            Some(delim @ '\'') => self.string(delim, false),
            Some(delim @ '"') => self.string(delim, false),
            Some(first @ '0'..='9') => self.numeric(first),
            Some('-') => match self.peek()? {
                Some('0'..='9' | '.') => self.numeric('-'),
                _ => Err(anyhow!(ScanError::Unexpected(self.get_error_context(), '-'))),
            },
            Some(ch) if is_ident_start(ch) => {
                if ch == 'b'
                    && let Some(delim @ ('\'' | '"')) = self.peek()?
                {
                    let _ = self.next_char()?;
                    return self.string(delim, true);
                }
                self.ident(ch)
            }
            Some(ch) => Err(anyhow!(ScanError::Unexpected(self.get_error_context(), ch))),
            None => Ok(Token::Eof),
        }
    }

    /// Scans a built-in predicate name starting with `:`, e.g. `:string:contains`.
    fn builtin_predicate(&mut self) -> Result<Token> {
        self.text.clear();
        self.text.push(':');
        loop {
            match self.peek()? {
                Some(ch) if is_ident(ch) => {
                    self.next_char()?;
                    self.text.push(ch);
                }
                Some(':') => {
                    self.next_char()?;
                    self.text.push(':');
                }
                _ => break,
            }
        }
        Ok(Token::Ident {
            name: self.text.clone(),
        })
    }

    fn name(&mut self) -> Result<Token> {
        self.text.clear();
        self.text.push('/');
        let mut seen_char = false;
        loop {
            match self.peek()? {
                Some(c) if is_name_char(c) => {
                    self.next_char()?;
                    self.text.push(c);
                    seen_char = true;
                }
                Some('/') => {
                    self.next_char()?;
                    if !seen_char {
                        anyhow::bail!("name constant: expected char after {}", self.text)
                    }
                    self.text.push('/');
                    seen_char = false;
                }
                _ => break,
            }
        }
        if !seen_char {
            anyhow::bail!("name constant: expected name char after {}", self.text)
        }
        Ok(Token::Name {
            name: self.text.to_string(),
        })
    }

    // TODO: this only handles single-double quoted (short. not long).
    fn string(&mut self, delim: char, is_byte: bool) -> Result<Token> {
        self.text.clear();
        if is_byte {
            self.text.push('b');
        }
        self.text.push(delim); // TODO
        loop {
            match self.next_char()? {
                Some(c) if c == delim => break,
                Some(c) => self.text.push(c),
                _ => break,
            }
        }
        self.text.push(delim); // TODO
        match unquote(self.text.as_str())? {
            DecodedSequence::String(decoded) => Ok(Token::String { decoded }),
            DecodedSequence::Bytes(decoded) => Ok(Token::Bytes { decoded }),
        }
    }

    fn numeric(&mut self, first: char) -> Result<Token> {
        self.text.clear();
        self.text.push(first);
        let mut is_float = false;
        loop {
            match self.peek()? {
                Some(c @ '0'..='9') => {
                    self.next_char()?;
                    self.text.push(c)
                }
                Some(c @ '.') => {
                    self.next_char()?;
                    is_float = true;
                    self.text.push(c)
                }
                _ => break,
            }
        }

        // Check for timestamp: exactly 4 digits followed by '-'
        if !is_float && self.text.len() == 4 && first != '-' {
            if let Some('-') = self.peek()? {
                return self.timestamp();
            }
        }

        // Check for duration suffix: digits followed by d, h, m, s, or ms
        if !is_float && first != '-' {
            if let Some(c @ ('d' | 'h' | 'm' | 's')) = self.peek()? {
                return self.duration_literal(c);
            }
        }

        if is_float {
            let num = self.text.parse::<f64>()?;
            return Ok(Token::Float { decoded: num });
        }
        let num = self.text.parse::<i64>()?;
        Ok(Token::Int { decoded: num })
    }

    /// Scan a timestamp literal: YYYY-MM-DDTHH:MM:SS[.frac][Z]
    /// `self.text` already contains the 4-digit year.
    fn timestamp(&mut self) -> Result<Token> {
        // Consume '-'
        self.next_char()?;
        self.text.push('-');

        // Month (2 digits)
        self.scan_n_digits(2, "timestamp month")?;
        self.expect_char('-', "timestamp")?;

        // Day (2 digits)
        self.scan_n_digits(2, "timestamp day")?;

        // Optional time part starting with 'T'
        let mut has_time = false;
        if let Some('T') = self.peek()? {
            has_time = true;
            self.next_char()?;
            self.text.push('T');

            // Hour
            self.scan_n_digits(2, "timestamp hour")?;
            self.expect_char(':', "timestamp")?;

            // Minute
            self.scan_n_digits(2, "timestamp minute")?;
            self.expect_char(':', "timestamp")?;

            // Second
            self.scan_n_digits(2, "timestamp second")?;

            // Optional fractional seconds
            if let Some('.') = self.peek()? {
                self.next_char()?;
                self.text.push('.');
                let start = self.text.len();
                loop {
                    match self.peek()? {
                        Some(c @ '0'..='9') => {
                            self.next_char()?;
                            self.text.push(c);
                        }
                        _ => break,
                    }
                }
                if self.text.len() == start {
                    return Err(anyhow!("timestamp: expected digits after '.'"));
                }
            }

            // Optional 'Z'
            if let Some('Z') = self.peek()? {
                self.next_char()?;
                self.text.push('Z');
            }
        }

        let nanos = parse_timestamp_to_nanos(&self.text, has_time)?;
        Ok(Token::Timestamp { nanos })
    }

    /// Scan a duration literal. `self.text` contains the digits, `suffix_start` is the first suffix char.
    fn duration_literal(&mut self, suffix_start: char) -> Result<Token> {
        self.next_char()?;
        let unit = if suffix_start == 'm' {
            // Could be 'm' (minutes) or 'ms' (milliseconds)
            if let Some('s') = self.peek()? {
                self.next_char()?;
                "ms"
            } else {
                "m"
            }
        } else {
            match suffix_start {
                'd' => "d",
                'h' => "h",
                's' => "s",
                _ => unreachable!(),
            }
        };

        let amount: i64 = self.text.parse()?;
        let nanos = match unit {
            "d" => amount.checked_mul(24 * 60 * 60 * 1_000_000_000),
            "h" => amount.checked_mul(60 * 60 * 1_000_000_000),
            "m" => amount.checked_mul(60 * 1_000_000_000),
            "s" => amount.checked_mul(1_000_000_000),
            "ms" => amount.checked_mul(1_000_000),
            _ => unreachable!(),
        }
        .ok_or_else(|| anyhow!("duration overflow"))?;
        Ok(Token::Duration { nanos })
    }

    /// Scan exactly `n` digits and append to `self.text`.
    fn scan_n_digits(&mut self, n: usize, context: &str) -> Result<()> {
        for _ in 0..n {
            match self.next_char()? {
                Some(c @ '0'..='9') => self.text.push(c),
                Some(c) => {
                    return Err(anyhow!(
                        "{context}: expected digit, got '{c}'"
                    ))
                }
                None => return Err(anyhow!("{context}: unexpected end of input")),
            }
        }
        Ok(())
    }

    /// Expect a specific character and append to `self.text`.
    fn expect_char(&mut self, expected: char, context: &str) -> Result<()> {
        match self.next_char()? {
            Some(c) if c == expected => {
                self.text.push(c);
                Ok(())
            }
            Some(c) => Err(anyhow!(
                "{context}: expected '{expected}', got '{c}'"
            )),
            None => Err(anyhow!(
                "{context}: expected '{expected}', got end of input"
            )),
        }
    }

    fn ident(&mut self, first: char) -> Result<Token> {
        self.ident_or_dot_ident(first, false)
    }

    fn ident_or_dot_ident(&mut self, first: char, dot_ident: bool) -> Result<Token> {
        self.text.clear();
        self.text.push(first);
        loop {
            match self.peek()? {
                Some(ch) if is_ident(ch) => {
                    self.next_char()?;
                    self.text.push(ch);
                }
                Some(':')
                    if (self.text.starts_with("fn:") || self.text == "fn")
                        && !self.text.ends_with(':') =>
                {
                    self.next_char()?;
                    self.text.push(':');
                }
                _ => {
                    return match self.text.as_str() {
                        "Package" => Ok(Token::Package),
                        "Use" => Ok(Token::Use),
                        "Decl" => Ok(Token::Decl),
                        "bound" => Ok(Token::Bound),
                        "inclusion" => Ok(Token::Inclusion),
                        "do" => Ok(Token::Do),
                        "descr" => Ok(Token::Descr),
                        "let" => Ok(Token::Let),
                        _ if dot_ident => {
                            let mut fn_name = String::new();
                            fn_name.push_str("fn:");
                            fn_name.push_str(&self.text);
                            Ok(Token::DotIdent { name: fn_name })
                        }
                        _ => Ok(Token::Ident {
                            name: self.text.clone(),
                        }),
                    };
                }
            }
        }
    }

    #[inline]
    fn next_char(&mut self) -> Result<Option<char>> {
        if let Some(c) = self.ch.take() {
            return Ok(Some(c));
        }
        macro_rules! next_byte_or_incomplete {
            ($self:expr) => {
                $self
                    .next_byte()?
                    .ok_or_else(|| anyhow!(ScanError::IncompleteUtf8(self.get_error_context())))
            };
        }
        let b = self.next_byte()?;
        match b {
            None => Ok(None),
            Some(b @ 0x00..=0x7F) => Ok(Some(unsafe { char::from_u32_unchecked(b.into()) })),
            Some(b1 @ 0xC0..=0xDF) => {
                let b2 = next_byte_or_incomplete!(self)?;
                let bytes = [b1, b2];
                let s = std::str::from_utf8(&bytes)?;
                Ok(s.chars().next())
            }
            Some(b1 @ 0xE0..=0xEF) => {
                let b2 = next_byte_or_incomplete!(self)?;
                let b3 = next_byte_or_incomplete!(self)?;
                let bytes = [b1, b2, b3];
                let s = std::str::from_utf8(&bytes)?;
                Ok(s.chars().next())
            }
            Some(b1 @ 0xF0..=0xF4) => {
                let b2 = next_byte_or_incomplete!(self)?;
                let b3 = next_byte_or_incomplete!(self)?;
                let b4 = next_byte_or_incomplete!(self)?;
                let bytes = [b1, b2, b3, b4];
                let s = std::str::from_utf8(&bytes)?;
                Ok(s.chars().next())
            }
            _ => Err(anyhow!("invalid utf8")),
        }
    }

    /// Advance to next non-whitespace byte. Skip comments.
    #[inline]
    fn next_char_skip(&mut self) -> Result<Option<char>> {
        loop {
            match self.next_char()? {
                Some(' ' | '\t' | '\n') => {}
                Some('#') => self.skip_line()?,
                z => return Ok(z),
            };
        }
    }

    // Skip bytes until newline (included).
    fn skip_line(&mut self) -> Result<()> {
        loop {
            match self.next_byte()? {
                Some(b'\n') | None => return Ok(()),
                _ => {}
            }
        }
    }

    // Advance exactly one byte.
    fn next_byte(&mut self) -> Result<Option<u8>> {
        match self.iter.next() {
            None => Ok(None),
            Some(Ok(b'\n')) => {
                self.start_of_line += self.col + 1;
                self.line += 1;
                self.col = 0;
                Ok(Some(b'\n'))
            }
            Some(Ok(c)) => {
                self.col += 1;
                Ok(Some(c))
            }
            Some(Err(e)) => Err(e.into()),
        }
    }

    #[inline]
    pub fn peek(&mut self) -> Result<Option<char>> {
        Ok(match self.ch {
            Some(ch) => Some(ch),
            None => {
                self.ch = self.next_char()?;
                self.ch
            }
        })
    }
}

fn is_ident_start(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '_' )
}

fn is_ident(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' )
}

fn is_name_char(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '~' | '.' | '%')
}

/// Parse a timestamp string like "2024-01-15" or "2024-01-15T10:30:00.123Z" into nanoseconds since Unix epoch.
fn parse_timestamp_to_nanos(s: &str, has_time: bool) -> Result<i64> {
    let year: i64 = s[0..4].parse()?;
    let month: u32 = s[5..7].parse()?;
    let day: u32 = s[8..10].parse()?;

    let (hour, minute, second, frac_nanos) = if has_time {
        let h: u32 = s[11..13].parse()?;
        let m: u32 = s[14..16].parse()?;
        let sec: u32 = s[17..19].parse()?;

        let frac = if s.len() > 19 && s.as_bytes()[19] == b'.' {
            let end = if s.ends_with('Z') {
                s.len() - 1
            } else {
                s.len()
            };
            let frac_str = &s[20..end];
            // Pad or truncate to 9 digits (nanoseconds)
            let padded = format!("{frac_str:0<9}");
            padded[..9].parse::<i64>()?
        } else {
            0
        };
        (h, m, sec, frac)
    } else {
        (0, 0, 0, 0)
    };

    // Convert date to days since epoch using Howard Hinnant's algorithm
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m_adj = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;

    let total_seconds = days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    Ok(total_seconds * 1_000_000_000 + frac_nanos)
}

#[cfg(test)]
mod test {

    use super::*;

    #[test]
    fn test_ident() -> Result<()> {
        let mut sc = Scanner::new("hello".as_bytes(), "test");
        let token = sc.next_token()?;
        match token {
            Token::Ident { name } if name == "hello" => {}
            _ => panic!("did not match"),
        }
        Ok(())
    }

    fn scan_all(s: &str) -> Result<Vec<Token>> {
        let mut sc = Scanner::new(s.as_bytes(), "test");
        let mut got = vec![];
        loop {
            let token = sc.next_token()?;
            if let Token::Eof = token {
                return Ok(got);
            }
            got.push(token.clone());
        }
    }

    #[test]
    fn test_keywords() -> Result<()> {
        let got = scan_all("do ⟸ let bound descr inclusion Package Use")?;
        use Token::*;
        let want = vec![
            Do,
            LongLeftDoubleArrow,
            Let,
            Bound,
            Descr,
            Inclusion,
            Package,
            Use,
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_values() -> Result<()> {
        let got = scan_all("1 3.14 'foo🤖' b'foo👷‍♀️' \"bar\" b\"bar\" ")?;
        let want = vec![
            Token::Int { decoded: 1 },
            Token::Float { decoded: 3.14 },
            Token::String {
                decoded: "foo🤖".to_string(),
            },
            Token::Bytes {
                decoded: "foo👷‍♀️".as_bytes().into(),
            },
            Token::String {
                decoded: "bar".to_string(),
            },
            Token::Bytes {
                decoded: "bar".as_bytes().into(),
            },
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_punctuation() -> Result<()> {
        let got = scan_all(".=!!=()[]{}::-|>")?;
        use Token::*;
        let want = vec![
            Dot, Eq, Bang, BangEq, LParen, RParen, LBracket, RBracket, LBrace, RBrace, Colon,
            ColonDash, PipeGt,
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_names() -> Result<()> {
        let got = scan_all("/foo /foo/bar")?;
        let want = vec![
            Token::Name {
                name: "/foo".to_string(),
            },
            Token::Name {
                name: "/foo/bar".to_string(),
            },
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_names_negative() -> Result<()> {
        scan_all("/").unwrap_err();
        scan_all("/foo/").unwrap_err();
        Ok(())
    }

    #[test]
    fn test_negative_numbers() -> Result<()> {
        let got = scan_all("-42 -3.14 -.5")?;
        let want = vec![
            Token::Int { decoded: -42 },
            Token::Float { decoded: -3.14 },
            Token::Float { decoded: -0.5 },
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_timestamp_date_only() -> Result<()> {
        let got = scan_all("2024-01-15")?;
        assert_eq!(got.len(), 1);
        match &got[0] {
            Token::Timestamp { nanos } => {
                // 2024-01-15T00:00:00Z in nanos
                assert_eq!(*nanos, 1705276800_000_000_000);
            }
            _ => panic!("expected Timestamp, got {:?}", got[0]),
        }
        Ok(())
    }

    #[test]
    fn test_timestamp_full() -> Result<()> {
        let got = scan_all("2024-01-15T10:30:00Z")?;
        assert_eq!(got.len(), 1);
        match &got[0] {
            Token::Timestamp { nanos } => {
                // 2024-01-15T10:30:00Z = 2024-01-15T00:00:00Z + 10*3600 + 30*60
                assert_eq!(*nanos, 1705276800_000_000_000 + (10 * 3600 + 30 * 60) * 1_000_000_000);
            }
            _ => panic!("expected Timestamp, got {:?}", got[0]),
        }
        Ok(())
    }

    #[test]
    fn test_timestamp_fractional() -> Result<()> {
        let got = scan_all("2024-01-15T10:30:00.123Z")?;
        assert_eq!(got.len(), 1);
        match &got[0] {
            Token::Timestamp { nanos } => {
                let base = 1705276800_000_000_000i64 + (10 * 3600 + 30 * 60) * 1_000_000_000;
                assert_eq!(*nanos, base + 123_000_000);
            }
            _ => panic!("expected Timestamp, got {:?}", got[0]),
        }
        Ok(())
    }

    #[test]
    fn test_duration_literals() -> Result<()> {
        let got = scan_all("1d 2h 30m 10s 500ms")?;
        let want = vec![
            Token::Duration { nanos: 24 * 60 * 60 * 1_000_000_000 },
            Token::Duration { nanos: 2 * 60 * 60 * 1_000_000_000 },
            Token::Duration { nanos: 30 * 60 * 1_000_000_000 },
            Token::Duration { nanos: 10 * 1_000_000_000 },
            Token::Duration { nanos: 500 * 1_000_000 },
        ];
        assert!(want == got, "want {:?} got {:?}", want, got);
        Ok(())
    }

    #[test]
    fn test_int_not_duration() -> Result<()> {
        // Bare integer followed by non-duration ident should be two tokens
        let got = scan_all("42 x")?;
        assert_eq!(got[0], Token::Int { decoded: 42 });
        assert_eq!(
            got[1],
            Token::Ident {
                name: "x".to_string()
            }
        );
        Ok(())
    }
}
