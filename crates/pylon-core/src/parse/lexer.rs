//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use crate::error::{Position, PyQLSyntaxError};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Select,
    Insert,
    Update,
    Delete,
    Filter,
    Order,
    By,
    Asc,
    Desc,
    First,
    Last,
    Limit,
    Offset,
    With,
    For,
    In,
    Union,
    Except,
    Intersect,
    Not,
    And,
    Or,
    Exists,
    Distinct,
    If,
    Then,
    Else,
    Like,
    Ilike,
    Set,
    True,
    False,
    Is,
    Optional,
    Required,
    Unless,
    Conflict,
    Detached,
    Group,
    Using,

    // Identifiers
    Ident(String),

    // Literals
    IntLit(i64),
    FloatLit(f64),
    DecimalLit(String),
    StrLit(String),

    // Punctuation
    LBrace,    // {
    RBrace,    // }
    LParen,    // (
    RParen,    // )
    LBracket,  // [
    RBracket,  // ]
    Dot,       // .
    Comma,     // ,
    Semicolon, // ;
    Colon,     // :
    At,        // @

    // Operators
    ColonEq,    // :=
    ColonColon, // ::
    Eq,         // =
    Ne,         // !=
    Lt,         // <
    Le,         // <=
    Gt,         // >
    Ge,         // >=
    Plus,       // +
    Minus,      // -
    Star,       // *
    Slash,      // /
    SlashSlash, // //
    Percent,    // %
    StarStar,   // **
    QQ,         // ??
    PlusPlus,   // ++
    PlusEq,     // +=
    MinusEq,    // -=
    Dollar,     // $

    Eof,
}

impl std::fmt::Display for Token {
    /// Human-readable surface form for error messages — `RParen` -> `')'`,
    /// `Eof` -> `end of input`, `Ident("foo")` -> `identifier 'foo'`, rather
    /// than a raw `{:?}` dump of the enum variant name. Used by the
    /// parser's `eat`/`eat_ident`/`err` call sites so "expected X, found Y"
    /// messages read the way a user actually wrote the query, not the way
    /// the lexer named its own token type.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s: &str = match self {
            // Keywords — surface form, lowercase (matches how they're
            // actually typed; PyQL keywords are case-insensitive but this
            // is just for display).
            Token::Select => "'select'",
            Token::Insert => "'insert'",
            Token::Update => "'update'",
            Token::Delete => "'delete'",
            Token::Filter => "'filter'",
            Token::Order => "'order'",
            Token::By => "'by'",
            Token::Asc => "'asc'",
            Token::Desc => "'desc'",
            Token::First => "'first'",
            Token::Last => "'last'",
            Token::Limit => "'limit'",
            Token::Offset => "'offset'",
            Token::With => "'with'",
            Token::For => "'for'",
            Token::In => "'in'",
            Token::Union => "'union'",
            Token::Except => "'except'",
            Token::Intersect => "'intersect'",
            Token::Not => "'not'",
            Token::And => "'and'",
            Token::Or => "'or'",
            Token::Exists => "'exists'",
            Token::Distinct => "'distinct'",
            Token::If => "'if'",
            Token::Then => "'then'",
            Token::Else => "'else'",
            Token::Like => "'like'",
            Token::Ilike => "'ilike'",
            Token::Set => "'set'",
            Token::True => "'true'",
            Token::False => "'false'",
            Token::Is => "'is'",
            Token::Optional => "'optional'",
            Token::Required => "'required'",
            Token::Unless => "'unless'",
            Token::Conflict => "'conflict'",
            Token::Detached => "'detached'",
            Token::Group => "'group'",
            Token::Using => "'using'",
            // Identifiers/literals carry their own value.
            Token::Ident(name) => return write!(f, "identifier '{name}'"),
            Token::IntLit(n) => return write!(f, "integer literal '{n}'"),
            Token::FloatLit(n) => return write!(f, "float literal '{n}'"),
            Token::DecimalLit(s) => return write!(f, "decimal literal '{s}'"),
            Token::StrLit(s) => return write!(f, "string literal {s:?}"),
            // Punctuation — the literal character(s), quoted.
            Token::LBrace => "'{'",
            Token::RBrace => "'}'",
            Token::LParen => "'('",
            Token::RParen => "')'",
            Token::LBracket => "'['",
            Token::RBracket => "']'",
            Token::Dot => "'.'",
            Token::Comma => "','",
            Token::Semicolon => "';'",
            Token::Colon => "':'",
            Token::At => "'@'",
            // Operators.
            Token::ColonEq => "':='",
            Token::ColonColon => "'::'",
            Token::Eq => "'='",
            Token::Ne => "'!='",
            Token::Lt => "'<'",
            Token::Le => "'<='",
            Token::Gt => "'>'",
            Token::Ge => "'>='",
            Token::Plus => "'+'",
            Token::Minus => "'-'",
            Token::Star => "'*'",
            Token::Slash => "'/'",
            Token::SlashSlash => "'//'",
            Token::Percent => "'%'",
            Token::StarStar => "'**'",
            Token::QQ => "'??'",
            Token::PlusPlus => "'++'",
            Token::PlusEq => "'+='",
            Token::MinusEq => "'-='",
            Token::Dollar => "'$'",
            Token::Eof => "end of input",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub pos: Position,
    /// Byte offset of this token's first byte in the source string. Tracked
    /// directly from the lexer's own running byte cursor (no line/col
    /// conversion needed) — used to place `analyze` markers in the echoed
    /// query text.
    pub byte_offset: usize,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer {
            input: input.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<SpannedToken>, PyQLSyntaxError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let pos = Position {
                line: self.line,
                col: self.col,
            };
            let byte_offset = self.pos;
            if self.pos >= self.input.len() {
                tokens.push(SpannedToken {
                    token: Token::Eof,
                    pos,
                    byte_offset,
                });
                break;
            }
            let tok = self.next_token(pos.clone())?;
            tokens.push(SpannedToken {
                token: tok,
                pos,
                byte_offset,
            });
        }
        Ok(tokens)
    }

    fn current(&self) -> u8 {
        self.input[self.pos]
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let ch = self.input[self.pos];
        self.pos += 1;
        if ch == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        ch
    }

    /// Reassemble a full UTF-8 scalar value starting from `lead` (a byte
    /// already consumed via `advance()`), consuming any continuation bytes.
    /// The lexer scans raw bytes so multi-byte UTF-8 sequences (accented
    /// letters, emoji, …) must be decoded explicitly here — otherwise each
    /// byte would be pushed into the token buffer as its own Latin-1-style
    /// codepoint (`byte as char`), corrupting any non-ASCII text literal.
    fn decode_utf8_char(&mut self, lead: u8) -> char {
        let extra = match lead {
            0xC0..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF7 => 3,
            _ => 0,
        };
        let mut buf = [0u8; 4];
        buf[0] = lead;
        for slot in buf.iter_mut().take(extra + 1).skip(1) {
            *slot = self.advance();
        }
        std::str::from_utf8(&buf[..=extra])
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(lead as char)
    }

    fn skip_whitespace_and_comments(&mut self) {
        while self.pos < self.input.len() {
            let ch = self.current();
            if ch == b'#' {
                while self.pos < self.input.len() && self.current() != b'\n' {
                    self.pos += 1;
                }
            } else if ch.is_ascii_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn next_token(&mut self, pos: Position) -> Result<Token, PyQLSyntaxError> {
        let ch = self.current();

        // Backtick-quoted identifiers: `Order`, `default`::`Filter`, etc.
        // Allows any keyword to be used as a type or field name.
        if ch == b'`' {
            return self.lex_backtick_ident(pos);
        }

        // String literals (including raw: r'...' / r"...")
        if ch == b'\'' || ch == b'"' {
            return self.lex_string(pos);
        }
        if ch == b'r' && self.peek().is_some_and(|c| c == b'\'' || c == b'"') {
            self.advance(); // consume 'r'
            return self.lex_raw_string(pos);
        }

        // Numeric literals
        if ch.is_ascii_digit() {
            return self.lex_number(pos);
        }

        // Identifiers and keywords
        if ch.is_ascii_alphabetic() || ch == b'_' {
            return Ok(self.lex_ident());
        }

        self.advance();

        match ch {
            b'{' => Ok(Token::LBrace),
            b'}' => Ok(Token::RBrace),
            b'(' => Ok(Token::LParen),
            b')' => Ok(Token::RParen),
            b'[' => Ok(Token::LBracket),
            b']' => Ok(Token::RBracket),
            b',' => Ok(Token::Comma),
            b';' => Ok(Token::Semicolon),
            b'@' => Ok(Token::At),
            b'$' => Ok(Token::Dollar),
            b'.' => Ok(Token::Dot),
            b'+' => {
                if self.pos < self.input.len() {
                    if self.current() == b'+' {
                        self.advance();
                        return Ok(Token::PlusPlus);
                    } else if self.current() == b'=' {
                        self.advance();
                        return Ok(Token::PlusEq);
                    }
                }
                Ok(Token::Plus)
            }
            b'-' => {
                if self.pos < self.input.len() && self.current() == b'=' {
                    self.advance();
                    Ok(Token::MinusEq)
                } else {
                    Ok(Token::Minus)
                }
            }
            b'*' => {
                if self.pos < self.input.len() && self.current() == b'*' {
                    self.advance();
                    Ok(Token::StarStar)
                } else {
                    Ok(Token::Star)
                }
            }
            b'/' => {
                if self.pos < self.input.len() && self.current() == b'/' {
                    self.advance();
                    Ok(Token::SlashSlash)
                } else {
                    Ok(Token::Slash)
                }
            }
            b'%' => Ok(Token::Percent),
            b'=' => Ok(Token::Eq),
            b'!' => {
                if self.pos < self.input.len() && self.current() == b'=' {
                    self.advance();
                    Ok(Token::Ne)
                } else {
                    Err(self.err(pos, "unexpected '!'"))
                }
            }
            b'<' => {
                if self.pos < self.input.len() && self.current() == b'=' {
                    self.advance();
                    Ok(Token::Le)
                } else {
                    Ok(Token::Lt)
                }
            }
            b'>' => {
                if self.pos < self.input.len() && self.current() == b'=' {
                    self.advance();
                    Ok(Token::Ge)
                } else {
                    Ok(Token::Gt)
                }
            }
            b':' => {
                if self.pos < self.input.len() {
                    if self.current() == b'=' {
                        self.advance();
                        return Ok(Token::ColonEq);
                    } else if self.current() == b':' {
                        self.advance();
                        return Ok(Token::ColonColon);
                    }
                }
                Ok(Token::Colon)
            }
            b'?' => {
                if self.pos < self.input.len() && self.current() == b'?' {
                    self.advance();
                    Ok(Token::QQ)
                } else {
                    Err(self.err(pos, "unexpected '?'"))
                }
            }
            _ => Err(self.err(pos, &format!("unexpected character '{}'", ch as char))),
        }
    }

    fn lex_ident(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.input.len()
            && (self.input[self.pos].is_ascii_alphanumeric() || self.input[self.pos] == b'_')
        {
            self.pos += 1;
            self.col += 1;
        }
        let raw = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        keyword_or_ident(raw)
    }

    fn lex_number(&mut self, pos: Position) -> Result<Token, PyQLSyntaxError> {
        let start = self.pos;
        while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
            self.advance();
        }
        // Check for float
        let is_float = self.pos < self.input.len()
            && ((self.input[self.pos] == b'.' && self.peek().is_some_and(|c| c.is_ascii_digit()))
                || self.input[self.pos] == b'e'
                || self.input[self.pos] == b'E');

        if is_float {
            if self.pos < self.input.len() && self.input[self.pos] == b'.' {
                self.advance();
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                    self.advance();
                }
            }
            if self.pos < self.input.len() && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E') {
                self.advance();
                if self.pos < self.input.len() && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-') {
                    self.advance();
                }
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                    self.advance();
                }
            }
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            // `n` suffix → decimal literal (e.g. 123.45n)
            if self.pos < self.input.len() && self.input[self.pos] == b'n' {
                self.advance();
                return Ok(Token::DecimalLit(s.to_string()));
            }
            s.parse::<f64>()
                .map(Token::FloatLit)
                .map_err(|_| self.err(pos, &format!("invalid float '{s}'")))
        } else {
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            // `n` suffix → decimal literal (e.g. 42n)
            if self.pos < self.input.len() && self.input[self.pos] == b'n' {
                self.advance();
                return Ok(Token::DecimalLit(s.to_string()));
            }
            s.parse::<i64>()
                .map(Token::IntLit)
                .map_err(|_| self.err(pos, &format!("integer overflow '{s}'")))
        }
    }

    fn lex_backtick_ident(&mut self, pos: Position) -> Result<Token, PyQLSyntaxError> {
        self.advance(); // consume opening backtick
        let mut buf = String::new();
        loop {
            if self.pos >= self.input.len() {
                return Err(self.err(pos, "unterminated backtick identifier"));
            }
            let ch = self.advance();
            if ch == b'`' {
                break;
            }
            buf.push(if ch < 0x80 {
                ch as char
            } else {
                self.decode_utf8_char(ch)
            });
        }
        if buf.is_empty() {
            return Err(self.err(pos, "backtick identifier must not be empty"));
        }
        Ok(Token::Ident(buf))
    }

    fn lex_raw_string(&mut self, pos: Position) -> Result<Token, PyQLSyntaxError> {
        let quote = self.advance(); // consume the opening quote
        let mut buf = String::new();
        loop {
            if self.pos >= self.input.len() {
                return Err(self.err(pos, "unterminated raw string literal"));
            }
            let ch = self.advance();
            if ch == quote {
                break;
            }
            buf.push(if ch < 0x80 {
                ch as char
            } else {
                self.decode_utf8_char(ch)
            });
        }
        Ok(Token::StrLit(buf))
    }

    fn lex_string(&mut self, pos: Position) -> Result<Token, PyQLSyntaxError> {
        let quote = self.advance();
        let mut buf = String::new();
        loop {
            if self.pos >= self.input.len() {
                return Err(self.err(pos, "unterminated string literal"));
            }
            let ch = self.advance();
            if ch == quote {
                break;
            }
            if ch == b'\\' {
                if self.pos >= self.input.len() {
                    return Err(self.err(pos, "unterminated escape in string"));
                }
                let esc = self.advance();
                match esc {
                    b'\'' => buf.push('\''),
                    b'"' => buf.push('"'),
                    b'\\' => buf.push('\\'),
                    b'n' => buf.push('\n'),
                    b't' => buf.push('\t'),
                    b'r' => buf.push('\r'),
                    b'0' => buf.push('\0'),
                    b'u' => {
                        // \uXXXX
                        if self.pos + 4 > self.input.len() {
                            return Err(self.err(pos, "invalid \\u escape"));
                        }
                        let hex = std::str::from_utf8(&self.input[self.pos..self.pos + 4])
                            .map_err(|_| self.err(pos.clone(), "invalid \\u escape"))?;
                        let code =
                            u32::from_str_radix(hex, 16).map_err(|_| self.err(pos.clone(), "invalid \\u escape"))?;
                        let c =
                            char::from_u32(code).ok_or_else(|| self.err(pos.clone(), "invalid unicode codepoint"))?;
                        buf.push(c);
                        for _ in 0..4 {
                            self.advance();
                        }
                    }
                    other => {
                        buf.push('\\');
                        buf.push(if other < 0x80 {
                            other as char
                        } else {
                            self.decode_utf8_char(other)
                        });
                    }
                }
            } else {
                buf.push(if ch < 0x80 {
                    ch as char
                } else {
                    self.decode_utf8_char(ch)
                });
            }
        }
        Ok(Token::StrLit(buf))
    }

    fn err(&self, pos: Position, msg: &str) -> PyQLSyntaxError {
        PyQLSyntaxError {
            message: msg.to_string(),
            position: pos,
        }
    }
}

fn keyword_or_ident(s: &str) -> Token {
    match s.to_ascii_uppercase().as_str() {
        "SELECT" => Token::Select,
        "INSERT" => Token::Insert,
        "UPDATE" => Token::Update,
        "DELETE" => Token::Delete,
        "FILTER" => Token::Filter,
        "ORDER" => Token::Order,
        "BY" => Token::By,
        "ASC" => Token::Asc,
        "DESC" => Token::Desc,
        "FIRST" => Token::First,
        "LAST" => Token::Last,
        "LIMIT" => Token::Limit,
        "OFFSET" => Token::Offset,
        "WITH" => Token::With,
        "FOR" => Token::For,
        "IN" => Token::In,
        "UNION" => Token::Union,
        "EXCEPT" => Token::Except,
        "INTERSECT" => Token::Intersect,
        "NOT" => Token::Not,
        "AND" => Token::And,
        "OR" => Token::Or,
        "EXISTS" => Token::Exists,
        "DISTINCT" => Token::Distinct,
        "IF" => Token::If,
        "THEN" => Token::Then,
        "ELSE" => Token::Else,
        "LIKE" => Token::Like,
        "ILIKE" => Token::Ilike,
        "SET" => Token::Set,
        "TRUE" => Token::True,
        "FALSE" => Token::False,
        "IS" => Token::Is,
        "OPTIONAL" => Token::Optional,
        "REQUIRED" => Token::Required,
        "UNLESS" => Token::Unless,
        "CONFLICT" => Token::Conflict,
        "DETACHED" => Token::Detached,
        "GROUP" => Token::Group,
        "USING" => Token::Using,
        _ => Token::Ident(s.to_string()),
    }
}
