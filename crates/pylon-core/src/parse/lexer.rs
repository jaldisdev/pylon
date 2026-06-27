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

    // Identifiers
    Ident(String),

    // Literals
    IntLit(i64),
    FloatLit(f64),
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
    ColonEq,   // :=
    ColonColon,// ::
    Eq,        // =
    Ne,        // !=
    Lt,        // <
    Le,        // <=
    Gt,        // >
    Ge,        // >=
    Plus,      // +
    Minus,     // -
    Star,      // *
    Slash,     // /
    SlashSlash,// //
    Percent,   // %
    StarStar,  // **
    QQ,        // ??
    PlusPlus,  // ++
    PlusEq,    // +=
    MinusEq,   // -=
    Dollar,    // $

    Eof,
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub pos: Position,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer { input: input.as_bytes(), pos: 0, line: 1, col: 1 }
    }

    pub fn tokenize(mut self) -> Result<Vec<SpannedToken>, PyQLSyntaxError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let pos = Position { line: self.line, col: self.col };
            if self.pos >= self.input.len() {
                tokens.push(SpannedToken { token: Token::Eof, pos });
                break;
            }
            let tok = self.next_token(pos.clone())?;
            tokens.push(SpannedToken { token: tok, pos });
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

        // String literals
        if ch == b'\'' || ch == b'"' {
            return self.lex_string(pos);
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
            && ((self.input[self.pos] == b'.'
                    && self.peek().map_or(false, |c| c.is_ascii_digit()))
                || self.input[self.pos] == b'e'
                || self.input[self.pos] == b'E');

        if is_float {
            if self.pos < self.input.len() && self.input[self.pos] == b'.' {
                self.advance();
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                    self.advance();
                }
            }
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E')
            {
                self.advance();
                if self.pos < self.input.len()
                    && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
                {
                    self.advance();
                }
                while self.pos < self.input.len() && self.input[self.pos].is_ascii_digit() {
                    self.advance();
                }
            }
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            s.parse::<f64>()
                .map(Token::FloatLit)
                .map_err(|_| self.err(pos, &format!("invalid float '{s}'")))
        } else {
            let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
            s.parse::<i64>()
                .map(Token::IntLit)
                .map_err(|_| self.err(pos, &format!("integer overflow '{s}'")))
        }
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
                        let code = u32::from_str_radix(hex, 16)
                            .map_err(|_| self.err(pos.clone(), "invalid \\u escape"))?;
                        let c = char::from_u32(code)
                            .ok_or_else(|| self.err(pos.clone(), "invalid unicode codepoint"))?;
                        buf.push(c);
                        for _ in 0..4 {
                            self.advance();
                        }
                    }
                    other => {
                        buf.push('\\');
                        buf.push(other as char);
                    }
                }
            } else {
                buf.push(ch as char);
            }
        }
        Ok(Token::StrLit(buf))
    }

    fn err(&self, pos: Position, msg: &str) -> PyQLSyntaxError {
        PyQLSyntaxError { message: msg.to_string(), position: pos }
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
        _ => Token::Ident(s.to_string()),
    }
}
