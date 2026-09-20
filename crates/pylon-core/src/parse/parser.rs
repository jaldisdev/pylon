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

use super::ast::*;
use super::lexer::{SpannedToken, Token};
use crate::error::{Position, PyQLSyntaxError};

pub struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>) -> Self {
        Parser { tokens, pos: 0 }
    }

    // ── Token access ────────────────────────────────────────────────────────────

    fn current(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn current_pos(&self) -> Position {
        self.tokens[self.pos].pos.clone()
    }

    fn current_offset(&self) -> usize {
        self.tokens[self.pos].byte_offset
    }

    fn peek_ahead(&self, offset: usize) -> &Token {
        let idx = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[idx].token
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos].token;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, expected: &Token) -> Result<(), PyQLSyntaxError> {
        if self.current() == expected {
            self.advance();
            Ok(())
        } else {
            Err(self.err(&format!("expected {expected}, found {}", self.current())))
        }
    }

    fn eat_ident(&mut self) -> Result<String, PyQLSyntaxError> {
        let name = self.keyword_as_ident();
        if let Some(s) = name {
            self.advance();
            return Ok(s);
        }
        match self.current().clone() {
            Token::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(self.err(&format!("expected an identifier, found {other}"))),
        }
    }

    /// If the current token is a keyword that is also a legal identifier,
    /// return its string form without consuming it. Returns `None` for tokens
    /// that are never valid identifiers (punctuation, literals, EOF).
    fn keyword_as_ident(&self) -> Option<String> {
        let spelling = self.keyword_ident_spelling()?;
        // The keyword table can only offer one casing; the source says which
        // one was actually written.
        Some(match &self.tokens[self.pos].keyword_text {
            Some(raw) => raw.to_string(),
            None => spelling,
        })
    }

    /// The keywords Gel classifies as *unreserved* — legal wherever an
    /// identifier is, so a shape can carry a field called `last` or `order`.
    /// The reserved ones (`select`, `filter`, `limit`, …) are not identifiers
    /// there, and accepting them would take more than Gel does.
    fn unreserved_keyword_as_ident(&self) -> Option<String> {
        let spelling = self.keyword_ident_spelling()?;
        matches!(
            spelling.as_str(),
            "asc" | "conflict" | "desc" | "first" | "last" | "order" | "required" | "then" | "unless" | "using"
        )
        .then(|| self.keyword_as_ident())
        .flatten()
    }

    fn keyword_ident_spelling(&self) -> Option<String> {
        match self.current() {
            Token::Select => Some("select".into()),
            Token::Insert => Some("insert".into()),
            Token::Update => Some("update".into()),
            Token::Delete => Some("delete".into()),
            Token::Filter => Some("filter".into()),
            Token::Order => Some("order".into()),
            Token::By => Some("by".into()),
            Token::Asc => Some("asc".into()),
            Token::Desc => Some("desc".into()),
            Token::First => Some("first".into()),
            Token::Last => Some("last".into()),
            Token::Limit => Some("limit".into()),
            Token::Offset => Some("offset".into()),
            Token::With => Some("with".into()),
            Token::For => Some("for".into()),
            Token::In => Some("in".into()),
            Token::Union => Some("union".into()),
            Token::Except => Some("except".into()),
            Token::Intersect => Some("intersect".into()),
            Token::Not => Some("not".into()),
            Token::And => Some("and".into()),
            Token::Or => Some("or".into()),
            Token::Exists => Some("exists".into()),
            Token::Distinct => Some("distinct".into()),
            Token::If => Some("if".into()),
            Token::Then => Some("then".into()),
            Token::Else => Some("else".into()),
            Token::Set => Some("set".into()),
            Token::Is => Some("is".into()),
            Token::Optional => Some("optional".into()),
            Token::Required => Some("required".into()),
            Token::Unless => Some("unless".into()),
            Token::Conflict => Some("conflict".into()),
            Token::Detached => Some("detached".into()),
            Token::Group => Some("group".into()),
            Token::Using => Some("using".into()),
            Token::Like => Some("like".into()),
            Token::Ilike => Some("ilike".into()),
            Token::True => Some("true".into()),
            Token::False => Some("false".into()),
            _ => None,
        }
    }

    /// True for a keyword that can legally be a bare name in expression
    /// position, because nothing in the grammar lets it *begin* an
    /// expression. Deliberately excludes the ones that can (`select`,
    /// `not`, `exists`, `distinct`, `if`, `detached`, `true`/`false`, and
    /// the statement keywords), so this only ever accepts input that was a
    /// syntax error before.
    fn keyword_is_never_expression_start(&self) -> bool {
        matches!(
            self.current(),
            Token::Filter
                | Token::Order
                | Token::By
                | Token::Asc
                | Token::Desc
                | Token::First
                | Token::Last
                | Token::Limit
                | Token::Offset
                | Token::In
                | Token::Then
                | Token::Else
                | Token::Set
                | Token::Is
                | Token::Optional
                | Token::Required
                | Token::Unless
                | Token::Conflict
                | Token::Using
                | Token::Like
                | Token::Ilike
                | Token::And
                | Token::Or
                | Token::Union
                | Token::Except
                | Token::Intersect
        )
    }

    fn at_stmt_start(&self) -> bool {
        matches!(
            self.current(),
            Token::With | Token::For | Token::Select | Token::Insert | Token::Update | Token::Delete | Token::Group
        )
    }

    fn err(&self, msg: &str) -> PyQLSyntaxError {
        PyQLSyntaxError {
            message: msg.to_string(),
            position: self.current_pos(),
        }
    }

    fn at_end(&self) -> bool {
        matches!(self.current(), Token::Eof)
    }

    // ── Top-level ───────────────────────────────────────────────────────────────

    /// Every statement in the input, for a script written as several
    /// statements separated by semicolons. A single statement parses to a
    /// one-element script, so callers need no special case for the common form.
    pub fn parse_script(&mut self) -> Result<Vec<Stmt>, PyQLSyntaxError> {
        let mut statements = vec![self.parse_stmt_inner()?];
        while matches!(self.current(), Token::Semicolon) {
            self.advance();
            if self.at_end() {
                break;
            }
            statements.push(self.parse_stmt_inner()?);
        }
        if !self.at_end() {
            return Err(self.err(&format!("unexpected {} after the end of the query", self.current())));
        }
        Ok(statements)
    }

    pub fn parse_stmt(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        let stmt = self.parse_stmt_inner()?;
        // Optional trailing semicolon
        if matches!(self.current(), Token::Semicolon) {
            self.advance();
        }
        if !self.at_end() {
            return Err(self.err(&format!("unexpected {} after the end of the query", self.current())));
        }
        Ok(stmt)
    }

    /// A "soft" keyword: `analyze` isn't reserved (it stays a legal type/field
    /// name everywhere else), so it's only recognized here, as the leading
    /// token of a statement.
    fn at_analyze_keyword(&self) -> bool {
        matches!(self.current(), Token::Ident(s) if s.eq_ignore_ascii_case("analyze"))
    }

    /// Like `parse_inner_stmt`, but additionally accepts a leading `analyze`
    /// — deliberately *not* folded into `parse_inner_stmt` itself, since
    /// `analyze` is only a top-level statement form (unlike a parenthesised
    /// subquery's `(select ...)`, `(insert ...)`, etc., it can't appear as a
    /// nested expression).
    fn parse_stmt_inner(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        if self.at_analyze_keyword() {
            self.advance();
            let inner = self.parse_stmt_inner()?;
            return Ok(Stmt::Analyze(Box::new(inner)));
        }
        self.parse_inner_stmt()
    }

    // ── SELECT ──────────────────────────────────────────────────────────────────

    fn parse_select(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Select)?;
        // `select max_priority := max(…)` — EdgeQL lets a select name its own
        // result (`SELECT OptionallyAliasedExpr`), and the select's own
        // clauses may read it by that name. Substituted back in below, which
        // is what the name means; Gel keeps it as `result_alias` and scopes it
        // the same way.
        let result_alias = if matches!(self.current(), Token::Ident(_)) && matches!(self.peek_ahead(1), Token::ColonEq)
        {
            let name = self.eat_ident()?;
            self.eat(&Token::ColonEq)?;
            Some(name)
        } else {
            None
        };
        let result = self.parse_expr()?;

        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        let order_by = if matches!(self.current(), Token::Order) {
            self.advance();
            self.eat(&Token::By)?;
            self.parse_sort_list()?
        } else {
            vec![]
        };

        let offset = if matches!(self.current(), Token::Offset) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        let limit = if matches!(self.current(), Token::Limit) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        let lock = self.parse_lock_clause()?;

        let (filter, order_by, offset, limit) = match &result_alias {
            Some(alias) => (
                filter.map(|e| Self::substitute_alias(e, alias, &result)),
                order_by
                    .into_iter()
                    .map(|s| SortExpr {
                        expr: Self::substitute_alias(s.expr, alias, &result),
                        direction: s.direction,
                        nones: s.nones,
                    })
                    .collect(),
                offset.map(|e| Self::substitute_alias(e, alias, &result)),
                limit.map(|e| Self::substitute_alias(e, alias, &result)),
            ),
            None => (filter, order_by, offset, limit),
        };

        Ok(Stmt::Select(SelectStmt {
            result,
            filter,
            order_by,
            offset,
            limit,
            lock,
        }))
    }

    /// Put `value` wherever a select's own clauses name its result alias.
    fn substitute_alias(expr: Expr, alias: &str, value: &Expr) -> Expr {
        match expr {
            Expr::Path(ref p)
                if !p.partial
                    && p.steps.len() == 1
                    && matches!(&p.steps[0], PathStep::Name(n) if n == alias) =>
            {
                value.clone()
            }
            Expr::BinOp(b) => Expr::BinOp(Box::new(BinOp {
                left: Self::substitute_alias(b.left, alias, value),
                op: b.op,
                right: Self::substitute_alias(b.right, alias, value),
            })),
            Expr::UnaryOp(u) => Expr::UnaryOp(Box::new(UnaryOp {
                op: u.op,
                operand: Self::substitute_alias(u.operand, alias, value),
            })),
            Expr::FunctionCall(f) => Expr::FunctionCall(FunctionCall {
                module: f.module,
                name: f.name,
                args: f.args.into_iter().map(|a| Self::substitute_alias(a, alias, value)).collect(),
                kwargs: f
                    .kwargs
                    .into_iter()
                    .map(|(k, v)| (k, Self::substitute_alias(v, alias, value)))
                    .collect(),
            }),
            other => other,
        }
    }

    /// `FOR UPDATE|SHARE|NO KEY UPDATE|KEY SHARE [NOWAIT|SKIP LOCKED]` —
    /// Postgres's own trailing row-locking clause, placed after
    /// `ORDER BY`/`LIMIT`/`OFFSET`, not right after `WHERE`. `SHARE`/`NO`/
    /// `KEY`/`NOWAIT`/`SKIP`/`LOCKED` aren't reserved keywords elsewhere in
    /// PyQL (unlike `FOR`/`UPDATE`, already tokens for the `for`-loop and
    /// `update` statements), so they're matched case-insensitively off
    /// `Token::Ident` here rather than added as new lexer keywords — the
    /// same convention `parse_sort_expr` already uses for `EMPTY`.
    fn parse_lock_clause(&mut self) -> Result<Option<LockClause>, PyQLSyntaxError> {
        if !matches!(self.current(), Token::For) {
            return Ok(None);
        }
        self.advance();

        fn ident_eq(tok: &Token, s: &str) -> bool {
            matches!(tok, Token::Ident(i) if i.eq_ignore_ascii_case(s))
        }

        let strength = match self.current() {
            Token::Update => {
                self.advance();
                LockStrength::Update
            }
            tok if ident_eq(tok, "SHARE") => {
                self.advance();
                LockStrength::Share
            }
            tok if ident_eq(tok, "NO") => {
                self.advance();
                if !ident_eq(self.current(), "KEY") {
                    return Err(self.err("expected KEY after NO in FOR NO KEY UPDATE"));
                }
                self.advance();
                self.eat(&Token::Update)?;
                LockStrength::NoKeyUpdate
            }
            tok if ident_eq(tok, "KEY") => {
                self.advance();
                if !ident_eq(self.current(), "SHARE") {
                    return Err(self.err("expected SHARE after KEY in FOR KEY SHARE"));
                }
                self.advance();
                LockStrength::KeyShare
            }
            _ => {
                return Err(self.err("expected UPDATE, SHARE, NO KEY UPDATE, or KEY SHARE after FOR"));
            }
        };

        let wait = match self.current() {
            tok if ident_eq(tok, "NOWAIT") => {
                self.advance();
                LockWait::NoWait
            }
            tok if ident_eq(tok, "SKIP") => {
                self.advance();
                if !ident_eq(self.current(), "LOCKED") {
                    return Err(self.err("expected LOCKED after SKIP"));
                }
                self.advance();
                LockWait::SkipLocked
            }
            _ => LockWait::Block,
        };

        Ok(Some(LockClause { strength, wait }))
    }

    fn parse_sort_list(&mut self) -> Result<Vec<SortExpr>, PyQLSyntaxError> {
        let mut list = vec![self.parse_sort_expr()?];
        while matches!(self.current(), Token::Then)
            || matches!(self.current(), Token::Ident(s) if s.eq_ignore_ascii_case("then"))
        {
            self.advance();
            list.push(self.parse_sort_expr()?);
        }
        Ok(list)
    }

    fn parse_sort_expr(&mut self) -> Result<SortExpr, PyQLSyntaxError> {
        let expr = self.parse_expr()?;
        let direction = match self.current() {
            Token::Asc => {
                self.advance();
                SortDirection::Asc
            }
            Token::Desc => {
                self.advance();
                SortDirection::Desc
            }
            _ => SortDirection::Asc,
        };
        let nones = match self.current() {
            Token::Ident(s) if s.eq_ignore_ascii_case("EMPTY") => {
                self.advance();
                match self.current() {
                    Token::First => {
                        self.advance();
                        NonesOrder::First
                    }
                    Token::Last => {
                        self.advance();
                        NonesOrder::Last
                    }
                    _ => return Err(self.err("expected FIRST or LAST after EMPTY")),
                }
            }
            _ => NonesOrder::Last,
        };
        Ok(SortExpr { expr, direction, nones })
    }

    // ── INSERT ──────────────────────────────────────────────────────────────────

    fn parse_insert(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Insert)?;
        let subject = self.parse_object_ref()?;
        self.eat(&Token::LBrace)?;
        let shape = self.parse_shape_body()?;
        self.eat(&Token::RBrace)?;

        let unless_conflict = if matches!(self.current(), Token::Unless) {
            self.advance();
            self.eat(&Token::Conflict)?;
            let on = if matches!(self.current(), Token::Ident(_) | Token::LParen) {
                // ON (expr)
                if matches!(self.current(), Token::Ident(s) if s.eq_ignore_ascii_case("ON")) {
                    self.advance();
                    Some(self.parse_expr()?)
                } else {
                    None
                }
            } else {
                None
            };
            let else_ = if matches!(self.current(), Token::Else) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            Some(UnlessConflict { on, else_ })
        } else {
            None
        };

        Ok(Stmt::Insert(InsertStmt {
            subject,
            shape,
            unless_conflict,
        }))
    }

    // ── UPDATE ──────────────────────────────────────────────────────────────────

    fn parse_update(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Update)?;
        let subject = self.parse_expr()?;

        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.eat(&Token::Set)?;
        self.eat(&Token::LBrace)?;
        let shape = self.parse_shape_body()?;
        self.eat(&Token::RBrace)?;

        Ok(Stmt::Update(UpdateStmt { subject, filter, shape }))
    }

    // ── DELETE ──────────────────────────────────────────────────────────────────

    fn parse_delete(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Delete)?;
        let subject = self.parse_expr()?;

        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Stmt::Delete(DeleteStmt { subject, filter }))
    }

    // ── WITH ────────────────────────────────────────────────────────────────────

    /// True when the next token opens a statement, so a comma just consumed
    /// ended the list instead of separating it.
    fn stmt_keyword_ahead(&self) -> bool {
        matches!(
            self.current(),
            Token::With | Token::For | Token::Select | Token::Insert | Token::Update | Token::Delete | Token::Group
        )
    }

    fn parse_with(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::With)?;
        let mut aliases = vec![];
        loop {
            let name = self.eat_ident()?;
            self.eat(&Token::ColonEq)?;
            // The binding value is any expression: a parenthesised stmt, a type cast, etc.
            let expr = self.parse_expr()?;
            aliases.push(CteDef { name, expr });
            if !matches!(self.current(), Token::Comma) {
                break;
            }
            self.advance();
            // A trailing comma closes the list rather than promising another
            // binding, the same as every other comma-separated list here and
            // in EdgeQL (`WithDeclList`, declared with
            // `allow_trailing_separator=True`).
            if self.stmt_keyword_ahead() {
                break;
            }
        }
        let stmt = self.parse_inner_stmt()?;
        Ok(Stmt::With(WithStmt {
            aliases,
            stmt: Box::new(stmt),
        }))
    }

    /// Parse a statement in positions where WITH/FOR are also allowed.
    fn parse_inner_stmt(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        match self.current() {
            Token::With => self.parse_with(),
            Token::For => self.parse_for(),
            Token::Select => self.parse_select(),
            Token::Insert => self.parse_insert(),
            Token::Update => self.parse_update(),
            Token::Delete => self.parse_delete(),
            Token::Group => self.parse_group(),
            _ => Err(self.err(&format!(
                "expected the start of a statement (with, for, select, insert, update, delete, or group), found {}",
                self.current()
            ))),
        }
    }

    fn parse_for(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::For)?;
        let optional = if matches!(self.current(), Token::Ident(s) if s.eq_ignore_ascii_case("OPTIONAL")) {
            self.advance();
            true
        } else {
            false
        };
        let var = self.eat_ident()?;
        self.eat(&Token::In)?;
        let iterator = self.parse_if_else()?;
        // Body: `union (stmt)` or `union stmt` or bare `stmt`.
        if matches!(self.current(), Token::Union) {
            self.advance();
        }
        // The body is a statement (`union (select ...)`, `union (insert ...)`)
        // or a plain expression (`union (x + 1)`, `union x.name`); the latter
        // means the same as selecting it.
        let body = if self.at_stmt_start() {
            self.parse_inner_stmt()?
        } else {
            match self.parse_expr()? {
                Expr::SubQuery(stmt) => *stmt,
                expr => Stmt::Select(SelectStmt {
                    result: expr,
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    limit: None,
                    lock: None,
                }),
            }
        };
        Ok(Stmt::For(ForStmt {
            var,
            optional,
            iterator,
            body: Box::new(body),
        }))
    }

    fn parse_group(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Group)?;
        // Subject: a type name (path) optionally followed by a shape.
        let subject_expr = self.parse_postfix()?;
        let (subject, shape) = if let Expr::Shape(sh) = subject_expr {
            let inner = sh.expr.unwrap_or(Expr::Path(crate::parse::ast::Path {
                steps: vec![],
                partial: false,
            }));
            (inner, Some(sh.elements))
        } else if matches!(self.current(), Token::LBrace) {
            let elements = self.parse_shape_body()?;
            (subject_expr, Some(elements))
        } else {
            (subject_expr, None)
        };

        // Optional USING clause.
        let mut using = vec![];
        if matches!(self.current(), Token::Using) {
            self.advance();
            loop {
                let alias = self.eat_ident()?;
                self.eat(&Token::ColonEq)?;
                let expr = self.parse_if_else()?;
                using.push((alias, expr));
                if matches!(self.current(), Token::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // BY clause (required).
        self.eat(&Token::By)?;
        let mut by = vec![];
        loop {
            by.push(self.parse_postfix()?);
            if matches!(self.current(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        // Trailing modifiers, in the same order a SELECT takes them. FILTER
        // picks which rows are grouped at all; ORDER BY/OFFSET/LIMIT apply
        // within each group, to its own elements.
        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let order_by = if matches!(self.current(), Token::Order) {
            self.advance();
            self.eat(&Token::By)?;
            self.parse_sort_list()?
        } else {
            vec![]
        };
        let offset = if matches!(self.current(), Token::Offset) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if matches!(self.current(), Token::Limit) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(Stmt::Group(crate::parse::ast::GroupStmt {
            subject,
            shape,
            using,
            by,
            filter,
            order_by,
            offset,
            limit,
        }))
    }

    // ── Expressions ─────────────────────────────────────────────────────────────

    // Precedence (lowest → highest):
    //   union/except → if/else → or → and → not → comparison → coalesce → add/concat → mul → pow → unary → postfix

    pub fn parse_expr(&mut self) -> Result<Expr, PyQLSyntaxError> {
        self.parse_union()
    }

    /// `parse_expr`, plus the trailing modifiers a standalone schema
    /// fragment is allowed to carry without writing `select` around them.
    /// With any of them present the result is the sub-select they imply, so
    /// everything downstream sees the parenthesised spelling.
    pub fn parse_pointer_expr(&mut self) -> Result<Expr, PyQLSyntaxError> {
        // A fragment may also *start* with a statement keyword, unbracketed:
        // `"select .orders limit 5"` means the same as `"(select .orders
        // limit 5)"`. Statements that can't stand in for a value still parse
        // here, so they fail with what they are rather than a syntax error.
        if matches!(
            self.current(),
            Token::Select | Token::With | Token::For | Token::Insert | Token::Update | Token::Delete
        ) {
            let stmt = self.parse_inner_stmt()?;
            return Ok(Expr::SubQuery(Box::new(stmt)));
        }
        let result = self.parse_union()?;
        if !matches!(
            self.current(),
            Token::Filter | Token::Order | Token::Offset | Token::Limit
        ) {
            return Ok(result);
        }
        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let order_by = if matches!(self.current(), Token::Order) {
            self.advance();
            self.eat(&Token::By)?;
            self.parse_sort_list()?
        } else {
            vec![]
        };
        let offset = if matches!(self.current(), Token::Offset) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if matches!(self.current(), Token::Limit) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Expr::SubQuery(Box::new(Stmt::Select(SelectStmt {
            result,
            filter,
            order_by,
            offset,
            limit,
            lock: None,
        }))))
    }

    fn parse_union(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_if_else()?;
        loop {
            if matches!(self.current(), Token::Union) {
                self.advance();
                let right = self.parse_if_else()?;
                left = Expr::Union(Box::new(left), Box::new(right));
            } else if matches!(self.current(), Token::Except) {
                self.advance();
                let right = self.parse_if_else()?;
                left = Expr::Except(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_if_else(&mut self) -> Result<Expr, PyQLSyntaxError> {
        // Prefix form: if condition then value else fallback
        if matches!(self.current(), Token::If) {
            self.advance();
            let condition = self.parse_or()?;
            self.eat(&Token::Then)?;
            let if_expr = self.parse_or()?;
            self.eat(&Token::Else)?;
            let else_expr = self.parse_if_else()?;
            return Ok(Expr::IfElse(Box::new(IfElse {
                if_expr,
                condition,
                else_expr,
            })));
        }

        // Python-style postfix: value if condition else fallback
        let expr = self.parse_or()?;
        if matches!(self.current(), Token::If) {
            self.advance();
            let condition = self.parse_or()?;
            self.eat(&Token::Else)?;
            // Recurse so that `a if x else b if y else c` chains correctly.
            let else_expr = self.parse_if_else()?;
            return Ok(Expr::IfElse(Box::new(IfElse {
                if_expr: expr,
                condition,
                else_expr,
            })));
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_and()?;
        while matches!(self.current(), Token::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::BinOp(Box::new(BinOp {
                left,
                op: BinOpKind::Or,
                right,
            }));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_not()?;
        while matches!(self.current(), Token::And) {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::BinOp(Box::new(BinOp {
                left,
                op: BinOpKind::And,
                right,
            }));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, PyQLSyntaxError> {
        if matches!(self.current(), Token::Not) {
            // Check for NOT LIKE / NOT ILIKE / NOT IN handled at comparison level.
            // Here we handle `NOT expr` as a unary operator.
            // But first peek to see if this is NOT IN / NOT LIKE at a higher position.
            // We handle that by falling through: parse_comparison will emit the op.
            self.advance();
            let operand = self.parse_not()?;
            return Ok(Expr::UnaryOp(Box::new(UnaryOp {
                op: UnaryOpKind::Not,
                operand,
            })));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let left = self.parse_coalesce()?;
        let op = match self.current() {
            Token::Eq => BinOpKind::Eq,
            Token::Ne => BinOpKind::Ne,
            Token::QEq => BinOpKind::CoalesceEq,
            Token::QNe => BinOpKind::CoalesceNe,
            Token::Lt => BinOpKind::Lt,
            Token::Le => BinOpKind::Le,
            Token::Gt => BinOpKind::Gt,
            Token::Ge => BinOpKind::Ge,
            Token::Like => BinOpKind::Like,
            Token::Ilike => BinOpKind::Ilike,
            Token::In => BinOpKind::In,
            Token::Is => {
                self.advance();
                let ty = self.parse_type_expr()?;
                return Ok(Expr::TypeIs {
                    expr: Box::new(left),
                    ty,
                });
            }
            Token::Not => {
                // NOT LIKE / NOT ILIKE / NOT IN
                match self.peek_ahead(1) {
                    Token::Like => {
                        self.advance();
                        self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp {
                            left,
                            op: BinOpKind::NotLike,
                            right,
                        })));
                    }
                    Token::Ilike => {
                        self.advance();
                        self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp {
                            left,
                            op: BinOpKind::NotIlike,
                            right,
                        })));
                    }
                    Token::In => {
                        self.advance();
                        self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp {
                            left,
                            op: BinOpKind::NotIn,
                            right,
                        })));
                    }
                    _ => return Ok(left),
                }
            }
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_coalesce()?;
        Ok(Expr::BinOp(Box::new(BinOp { left, op, right })))
    }

    fn parse_coalesce(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_add()?;
        while matches!(self.current(), Token::QQ) {
            self.advance();
            // `x ?? not exists .y` — `??` binds tighter than `not`, but a
            // *prefix* operator opening the right operand is unambiguous, and
            // EdgeQL takes it. Parsed at the arithmetic level alone, `not` has
            // nowhere to go.
            let right = if matches!(self.current(), Token::Not) {
                self.parse_not()?
            } else {
                self.parse_add()?
            };
            left = Expr::BinOp(Box::new(BinOp {
                left,
                op: BinOpKind::Coalesce,
                right,
            }));
        }
        Ok(left)
    }

    fn parse_add(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.current() {
                Token::Plus => BinOpKind::Add,
                Token::Minus => BinOpKind::Sub,
                Token::PlusPlus => BinOpKind::Concat,
                _ => break,
            };
            self.advance();
            let right = self.parse_mul()?;
            left = Expr::BinOp(Box::new(BinOp { left, op, right }));
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.current() {
                Token::Star => BinOpKind::Mul,
                Token::Slash => BinOpKind::Div,
                Token::SlashSlash => BinOpKind::FloorDiv,
                Token::Percent => BinOpKind::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_pow()?;
            left = Expr::BinOp(Box::new(BinOp { left, op, right }));
        }
        Ok(left)
    }

    fn parse_pow(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let base = self.parse_unary()?;
        if matches!(self.current(), Token::StarStar) {
            self.advance();
            // Right-associative
            let exp = self.parse_pow()?;
            return Ok(Expr::BinOp(Box::new(BinOp {
                left: base,
                op: BinOpKind::Pow,
                right: exp,
            })));
        }
        Ok(base)
    }

    fn parse_unary(&mut self) -> Result<Expr, PyQLSyntaxError> {
        match self.current() {
            Token::Minus => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp {
                    op: UnaryOpKind::Minus,
                    operand,
                })))
            }
            Token::Exists => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp {
                    op: UnaryOpKind::Exists,
                    operand,
                })))
            }
            Token::Distinct => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp {
                    op: UnaryOpKind::Distinct,
                    operand,
                })))
            }
            _ => self.parse_type_cast(),
        }
    }

    // Type cast: `<TypeName>expr`
    fn parse_type_cast(&mut self) -> Result<Expr, PyQLSyntaxError> {
        if matches!(self.current(), Token::Lt) {
            // Peek ahead to see if this looks like a type cast rather than a comparison.
            // A type cast is `<Ident>` or `<Module::Ident>` followed by `>`.
            if self.is_type_cast_ahead() {
                self.advance(); // consume `<`
                // `<optional str>$token` / `<required str>$token` — the
                // cardinality a parameter is declared with. A Postgres
                // parameter is nullable either way, which is what `optional`
                // already means; `required` is not enforced.
                if matches!(self.current(), Token::Optional | Token::Required) {
                    self.advance();
                }
                let ty = self.parse_type_expr()?;
                self.eat(&Token::Gt)?;
                let expr = self.parse_type_cast()?;
                return Ok(Expr::TypeCast(Box::new(TypeCast { expr, ty })));
            }
        }
        self.parse_postfix()
    }

    fn is_type_cast_ahead(&self) -> bool {
        // Look for the pattern: `<` Ident ((`::` Ident)?) `>`
        let mut i = self.pos + 1;
        let n = self.tokens.len();
        if i >= n {
            return false;
        }
        // A parameter's cardinality may lead the type: `<optional str>$token`.
        if matches!(self.tokens[i].token, Token::Optional | Token::Required) {
            i += 1;
            if i >= n {
                return false;
            }
        }
        // must start with an identifier
        if !matches!(self.tokens[i].token, Token::Ident(_)) {
            return false;
        }
        // `<tuple<...` / `<array<...` — a structural tuple or array cast. The
        // outer `<...>` isn't balance-checkable with this simple lookahead
        // (nesting can go arbitrarily deep), but a bare identifier "tuple"/
        // "array" immediately followed by `<` is never a legitimate comparison
        // operand, so treat it unconditionally as a cast.
        if let Token::Ident(name) = &self.tokens[i].token
            && (name == "tuple" || name == "array")
            && i + 1 < n
            && matches!(self.tokens[i + 1].token, Token::Lt)
        {
            return true;
        }
        i += 1;
        if i >= n {
            return false;
        }
        // optional `::` Name
        if matches!(self.tokens[i].token, Token::ColonColon) {
            i += 1;
            if i >= n {
                return false;
            }
            if !matches!(self.tokens[i].token, Token::Ident(_)) {
                return false;
            }
            i += 1;
        }
        // must be followed by `>`
        i < n && matches!(self.tokens[i].token, Token::Gt)
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr, PyQLSyntaxError> {
        // `tuple` is a contextual keyword: a bare identifier "tuple" immediately
        // followed by `<` starts a structural tuple type instead of a plain named
        // type reference — no real schema type would ever be named "tuple" and
        // written this way, so no reserved-word conflict.
        if matches!(self.current(), Token::Ident(s) if s == "tuple") && matches!(self.peek_ahead(1), Token::Lt) {
            self.advance(); // "tuple"
            self.advance(); // "<"
            let elements = self.parse_tuple_type_elements()?;
            self.eat(&Token::Gt)?;
            return Ok(TypeExpr::Tuple { elements });
        }

        // `array` is a contextual keyword the same way "tuple" is — a bare
        // identifier "array" immediately followed by `<` starts a
        // one-dimensional array type. Pylon arrays can't nest (any element
        // type is allowed except another array), rejected here at parse time
        // rather than left to a later compile pass.
        if matches!(self.current(), Token::Ident(s) if s == "array") && matches!(self.peek_ahead(1), Token::Lt) {
            self.advance(); // "array"
            self.advance(); // "<"
            let element = self.parse_type_expr()?;
            if matches!(element, TypeExpr::Array { .. }) {
                return Err(self.err("nested arrays are not supported; arrays must be one-dimensional"));
            }
            self.eat(&Token::Gt)?;
            return Ok(TypeExpr::Array {
                element: Box::new(element),
            });
        }

        let first = self.eat_ident()?;
        if matches!(self.current(), Token::ColonColon) {
            self.advance();
            let name = self.eat_ident()?;
            // See the matching check in parse_primary: PyQL module names
            // are a single segment, so a further `::` here is an error
            // worth naming rather than a confusing dangling token later.
            if matches!(self.current(), Token::ColonColon) {
                return Err(self.err(&format!(
                    "'{first}::{name}::...' has too many '::' segments; \
                     PyQL module names are a single segment"
                )));
            }
            Ok(TypeExpr::named(Some(first), name))
        } else {
            Ok(TypeExpr::named(None, first))
        }
    }

    /// Comma-separated `tuple<...>` elements — each either a bare `TypeExpr`
    /// (unnamed/positional element) or `name: TypeExpr` (named element). Nesting
    /// is handled for free since each element's own type is parsed via the same
    /// `parse_type_expr`. Rejects a mix of named and unnamed elements.
    fn parse_tuple_type_elements(&mut self) -> Result<Vec<TupleTypeElement>, PyQLSyntaxError> {
        let mut elements = vec![];
        loop {
            // A named element starts with `ident ':'` — checked via lookahead so a
            // bare type reference (which also starts with an identifier) isn't
            // mistaken for one; `::` (module separator) is a different token so
            // this can't collide with `module::Name`.
            let name = if matches!(self.current(), Token::Ident(_)) && matches!(self.peek_ahead(1), Token::Colon) {
                let n = self.eat_ident()?;
                self.eat(&Token::Colon)?;
                Some(n)
            } else {
                None
            };
            let ty = self.parse_type_expr()?;
            elements.push(TupleTypeElement { name, ty: Box::new(ty) });

            if matches!(self.current(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        let named_count = elements.iter().filter(|e| e.name.is_some()).count();
        if named_count != 0 && named_count != elements.len() {
            return Err(self.err("tuple elements must be all named or all unnamed, not mixed"));
        }
        Ok(elements)
    }

    // Postfix: shape `{...}`, dot traversal, type intersection `[is T]`, link prop `@prop`
    fn parse_postfix(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let root_offset = self.current_offset();
        let mut expr = self.parse_primary()?;

        loop {
            match self.current() {
                Token::LBrace => {
                    self.advance();
                    let elements = self.parse_shape_body()?;
                    self.eat(&Token::RBrace)?;
                    expr = Expr::Shape(Box::new(ShapeExpr {
                        expr: Some(expr),
                        elements,
                        marker_offset: Some(root_offset),
                    }));
                }
                Token::Dot => {
                    self.advance();
                    if matches!(self.current(), Token::Lt) {
                        // Backlink: .<link_name
                        self.advance();
                        let name = self.eat_ident()?;
                        expr = self.extend_path(expr, PathStep::Backlink(name))?;
                    } else if let Token::IntLit(n) = self.current().clone() {
                        // Positional tuple access: expr.0, expr.1, ...
                        self.advance();
                        expr = Expr::TupleIndex {
                            expr: Box::new(expr),
                            index: n as usize,
                        };
                    } else {
                        let name = self.eat_ident()?;
                        // Path expressions extend the path; everything else uses FieldAccess.
                        expr = match expr {
                            Expr::Path(_) => self.extend_path(expr, PathStep::Name(name))?,
                            other => Expr::FieldAccess {
                                expr: Box::new(other),
                                field: name,
                            },
                        };
                    }
                }
                Token::LBracket => {
                    // `[is TypeName]` type intersection
                    if matches!(self.peek_ahead(1), Token::Is) {
                        self.advance(); // [
                        self.advance(); // is
                        let type_ref = self.parse_object_ref()?;
                        self.eat(&Token::RBracket)?;
                        expr = self.extend_path(expr, PathStep::TypeIntersection(type_ref))?;
                    } else {
                        // Index `[i]` or slice `[lower:upper]`
                        self.advance(); // consume [
                        let lower = if matches!(self.current(), Token::Colon) {
                            None
                        } else {
                            Some(Box::new(self.parse_expr()?))
                        };
                        if matches!(self.current(), Token::Colon) {
                            self.advance(); // consume :
                            let upper = if matches!(self.current(), Token::RBracket) {
                                None
                            } else {
                                Some(Box::new(self.parse_expr()?))
                            };
                            self.eat(&Token::RBracket)?;
                            expr = Expr::Slice {
                                expr: Box::new(expr),
                                lower,
                                upper,
                            };
                        } else {
                            let index = lower.ok_or_else(|| self.err("expected index expression"))?;
                            self.eat(&Token::RBracket)?;
                            expr = Expr::Index {
                                expr: Box::new(expr),
                                index,
                            };
                        }
                    }
                }
                Token::At => {
                    self.advance();
                    let name = self.eat_ident()?;
                    expr = self.extend_path(expr, PathStep::LinkProp(name))?;
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    /// Extend an existing Path expr with a new step, or wrap a non-path expr into an error.
    fn extend_path(&self, expr: Expr, step: PathStep) -> Result<Expr, PyQLSyntaxError> {
        match expr {
            Expr::Path(mut p) => {
                p.steps.push(step);
                Ok(Expr::Path(p))
            }
            _other => {
                // For non-path expressions (e.g. `(SELECT ...).field`), we'd need
                // a Path with an Expr head — not supported in Phase 1.
                Err(PyQLSyntaxError {
                    message: "field access with '.' is only supported on paths (e.g. `.field`), \
                              not on a parenthesized sub-expression"
                        .to_string(),
                    position: self.current_pos(),
                })
            }
        }
    }

    // ── Primary expressions ─────────────────────────────────────────────────────

    fn parse_primary(&mut self) -> Result<Expr, PyQLSyntaxError> {
        match self.current().clone() {
            // Relative path starting with `.name` or `.<name` (backlink)
            Token::Dot => {
                self.advance();
                if matches!(self.current(), Token::Lt) {
                    self.advance(); // consume <
                    let name = self.eat_ident()?;
                    Ok(Expr::Path(Path {
                        steps: vec![PathStep::Backlink(name)],
                        partial: true,
                    }))
                } else {
                    let name = self.eat_ident()?;
                    Ok(Expr::Path(Path::relative(name)))
                }
            }

            // Parameter `$name` or positional `$0`, `$1`, …
            Token::Dollar => {
                self.advance();
                let name = if let Token::IntLit(n) = self.current().clone() {
                    self.advance();
                    n.to_string()
                } else {
                    self.eat_ident()?
                };
                Ok(Expr::Parameter(name))
            }

            // Literals
            Token::IntLit(n) => {
                self.advance();
                Ok(Expr::Literal(Literal::Int(n)))
            }
            Token::FloatLit(f) => {
                self.advance();
                Ok(Expr::Literal(Literal::Float(f)))
            }
            Token::DecimalLit(s) => {
                self.advance();
                // Emit as <decimal>str — compiles to 'value'::numeric
                Ok(Expr::TypeCast(Box::new(TypeCast {
                    ty: TypeExpr::named(Some("std".to_string()), "decimal"),
                    expr: Expr::Literal(Literal::Str(s)),
                })))
            }
            Token::StrLit(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::Str(s)))
            }
            Token::True => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(true)))
            }
            Token::False => {
                self.advance();
                Ok(Expr::Literal(Literal::Bool(false)))
            }

            // Parenthesised expression, anonymous tuple, or named tuple
            Token::LParen => self.parse_paren_expr(),

            // Set literal `{1, 2}` or free object `{ foo := 'bar' }`
            Token::LBrace => {
                self.advance(); // consume {
                if matches!(self.current(), Token::RBrace) {
                    self.advance();
                    return Ok(Expr::Set(vec![]));
                }
                // Free object: starts with `ident :=` or `.ident :=`
                let is_free_object = match self.current() {
                    Token::Ident(_) => matches!(self.peek_ahead(1), Token::ColonEq),
                    Token::Dot => {
                        matches!(self.peek_ahead(1), Token::Ident(_)) && matches!(self.peek_ahead(2), Token::ColonEq)
                    }
                    // `{ last := … }` — an unreserved keyword names a field as
                    // well as a bare identifier does. Read as a set literal
                    // instead, the `:=` has nowhere to go.
                    _ => self.unreserved_keyword_as_ident().is_some() && matches!(self.peek_ahead(1), Token::ColonEq),
                };
                if is_free_object {
                    let elements = self.parse_shape_body()?;
                    self.eat(&Token::RBrace)?;
                    return Ok(Expr::Shape(Box::new(ShapeExpr {
                        expr: None,
                        elements,
                        marker_offset: None,
                    })));
                }
                // Set literal: comma-separated value expressions
                let mut elems = vec![self.parse_expr()?];
                while matches!(self.current(), Token::Comma) {
                    self.advance();
                    if matches!(self.current(), Token::RBrace) {
                        break;
                    }
                    elems.push(self.parse_expr()?);
                }
                self.eat(&Token::RBrace)?;
                Ok(Expr::Set(elems))
            }

            // `[is Type]` in expression context — type intersection starting expression
            Token::LBracket if matches!(self.peek_ahead(1), Token::Is) => {
                self.advance(); // [
                self.advance(); // is
                let type_ref = self.parse_object_ref()?;
                self.eat(&Token::RBracket)?;
                Ok(Expr::Path(Path {
                    steps: vec![PathStep::TypeIntersection(type_ref)],
                    partial: true,
                }))
            }

            // Array literal
            Token::LBracket => {
                self.advance();
                if matches!(self.current(), Token::RBracket) {
                    self.advance();
                    return Ok(Expr::Array(vec![]));
                }
                let mut elems = vec![self.parse_expr()?];
                while matches!(self.current(), Token::Comma) {
                    self.advance();
                    if matches!(self.current(), Token::RBracket) {
                        break;
                    }
                    elems.push(self.parse_expr()?);
                }
                self.eat(&Token::RBracket)?;
                Ok(Expr::Array(elems))
            }

            // Identifier — either a type reference (absolute path) or a function call
            Token::Ident(name) => {
                let name = name.clone();
                self.advance();
                self.parse_name_expr(name)
            }

            Token::Detached => {
                self.advance();
                let inner = self.parse_expr()?;
                Ok(Expr::Detached(Box::new(inner)))
            }

            // A bare link property: `filter @primary = true` inside a
            // multi-link's own modifiers, where the link being filtered is
            // already what's in scope. (`x@prop` — a property read off a
            // named path — is the postfix form, parsed in `parse_postfix`.)
            Token::At => {
                self.advance();
                let name = self.eat_ident()?;
                Ok(Expr::Path(Path {
                    steps: vec![PathStep::LinkProp(name)],
                    partial: true,
                }))
            }

            // A bare sub-statement in expression position: `x := select .emails
            // filter .primary limit 1`, `x := with y := ... select ...`. The
            // parenthesised form is handled in `parse_paren_expr`; EdgeQL accepts
            // both, and the statement parsers stop on their own at the `,` or `}`
            // that ends the shape element.
            _ if self.at_stmt_start() => {
                let stmt = self.parse_inner_stmt()?;
                Ok(Expr::SubQuery(Box::new(stmt)))
            }

            // A keyword that can't begin an expression is a name here — the
            // lexer is case-insensitive, so a `with` binding (or a type)
            // called `order` arrives as `Token::Order` and would otherwise
            // read as "expected an expression, found 'order'". Both the
            // binding site (`eat_ident`) and this one go through
            // `keyword_as_ident`, so they agree on the spelling.
            _ if self.keyword_is_never_expression_start() => {
                let name = self.keyword_as_ident().expect("checked by the guard");
                self.advance();
                self.parse_name_expr(name)
            }

            other => Err(self.err(&format!("expected an expression, found {other}"))),
        }
    }

    /// Everything a bare name can turn into once consumed: a session global,
    /// a qualified reference, a function call, or a path rooted at it.
    fn parse_name_expr(&mut self, name: String) -> Result<Expr, PyQLSyntaxError> {
        {
            {
                // `global name` or `global module::name` → Expr::Global
                if name == "global" {
                    if let Token::Ident(gname) = self.current().clone() {
                        let gname = gname.clone();
                        self.advance();
                        if matches!(self.current(), Token::ColonColon) {
                            self.advance();
                            let member = self.eat_ident()?;
                            return Ok(Expr::Global(format!("{}::{}", gname, member)));
                        }
                        return Ok(Expr::Global(gname));
                    }
                    // `global` not followed by ident → fall through as bare path
                    return Ok(Expr::Path(Path::absolute(name)));
                }

                // `Module::Name` qualified reference or function call
                if matches!(self.current(), Token::ColonColon) {
                    self.advance();
                    let member = self.eat_ident()?;
                    // A third `::` here means the module path has more than
                    // one segment, which PyQL doesn't support — say so
                    // clearly instead of leaving the trailing `::` to
                    // surface as a confusing "unexpected token ColonColon"
                    // once this (wrongly 2-segment-terminated) path/call
                    // returns.
                    if matches!(self.current(), Token::ColonColon) {
                        return Err(self.err(&format!(
                            "'{name}::{member}::...' has too many '::' segments; \
                             PyQL module names are a single segment"
                        )));
                    }
                    // Function call with module prefix
                    if matches!(self.current(), Token::LParen) {
                        return self.parse_func_call_args(Some(name), member);
                    }
                    // Qualified path (e.g. `std::SomeType`)
                    let path = Path {
                        steps: vec![PathStep::Name(format!("{name}::{member}"))],
                        partial: false,
                    };
                    return Ok(Expr::Path(path));
                }

                // Unqualified function call
                if matches!(self.current(), Token::LParen) {
                    return self.parse_func_call_args(None, name);
                }

                // Bare identifier → absolute path (type name or let-binding)
                Ok(Expr::Path(Path::absolute(name)))
            }
        }
    }

    fn parse_paren_expr(&mut self) -> Result<Expr, PyQLSyntaxError> {
        self.eat(&Token::LParen)?;

        if matches!(self.current(), Token::RParen) {
            self.advance();
            return Ok(Expr::Tuple(vec![]));
        }

        // Parenthesised statement: (SELECT ...), (INSERT ...), (UPDATE ...), (DELETE ...), (WITH ...), (FOR ...)
        if matches!(
            self.current(),
            Token::With | Token::For | Token::Select | Token::Insert | Token::Update | Token::Delete
        ) {
            let stmt = self.parse_inner_stmt()?;
            self.eat(&Token::RParen)?;
            return Ok(Expr::SubQuery(Box::new(stmt)));
        }

        let first = self.parse_expr()?;

        // Named tuple: `(name := expr, ...)`
        if matches!(self.current(), Token::ColonEq)
            && let Expr::Path(ref p) = first
            && !p.partial
            && p.steps.len() == 1
            && let PathStep::Name(ref label) = p.steps[0]
        {
            let label = label.clone();
            self.advance(); // :=
            let val = self.parse_expr()?;
            let mut pairs = vec![(label, val)];
            while matches!(self.current(), Token::Comma) {
                self.advance();
                if matches!(self.current(), Token::RParen) {
                    break;
                }
                let key = self.eat_ident()?;
                self.eat(&Token::ColonEq)?;
                let val = self.parse_expr()?;
                pairs.push((key, val));
            }
            self.eat(&Token::RParen)?;
            return Ok(Expr::NamedTuple(pairs));
        }

        // Anonymous tuple or grouped expression
        if matches!(self.current(), Token::Comma) {
            let mut elems = vec![first];
            while matches!(self.current(), Token::Comma) {
                self.advance();
                if matches!(self.current(), Token::RParen) {
                    break;
                }
                elems.push(self.parse_expr()?);
            }
            self.eat(&Token::RParen)?;
            return Ok(Expr::Tuple(elems));
        }

        self.eat(&Token::RParen)?;
        Ok(first)
    }

    /// A function-call argument, with the `FILTER`/`ORDER BY` EdgeQL lets one
    /// carry: `count(.<account[is Step] filter .status != Done)`. The clauses
    /// belong to the set the argument names, so they become a select over it
    /// — which is what Gel's own grammar builds for this.
    fn parse_call_arg(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let expr = self.parse_expr()?;
        let filter = if matches!(self.current(), Token::Filter) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let order_by = if matches!(self.current(), Token::Order) {
            self.advance();
            self.eat(&Token::By)?;
            self.parse_sort_list()?
        } else {
            vec![]
        };
        if filter.is_none() && order_by.is_empty() {
            return Ok(expr);
        }
        Ok(Expr::SubQuery(Box::new(Stmt::Select(SelectStmt {
            result: expr,
            filter,
            order_by,
            offset: None,
            limit: None,
            lock: None,
        }))))
    }

    fn parse_func_call_args(&mut self, module: Option<String>, name: String) -> Result<Expr, PyQLSyntaxError> {
        self.eat(&Token::LParen)?;
        let mut args = vec![];
        let mut kwargs = vec![];
        if !matches!(self.current(), Token::RParen) {
            loop {
                // Named argument: `name := expr`
                if matches!(self.current(), Token::Ident(_)) && matches!(self.peek_ahead(1), Token::ColonEq) {
                    let key = self.eat_ident()?;
                    self.eat(&Token::ColonEq)?;
                    let val = self.parse_expr()?;
                    kwargs.push((key, val));
                } else {
                    args.push(self.parse_call_arg()?);
                }
                if !matches!(self.current(), Token::Comma) {
                    break;
                }
                self.advance();
                if matches!(self.current(), Token::RParen) {
                    break;
                }
            }
        }
        self.eat(&Token::RParen)?;
        Ok(Expr::FunctionCall(FunctionCall {
            module,
            name,
            args,
            kwargs,
        }))
    }

    fn parse_object_ref(&mut self) -> Result<ObjectRef, PyQLSyntaxError> {
        let first = self.eat_ident()?;
        if matches!(self.current(), Token::ColonColon) {
            self.advance();
            let name = self.eat_ident()?;
            Ok(ObjectRef::qualified(first, name))
        } else {
            Ok(ObjectRef::unqualified(first))
        }
    }

    // ── Shape body ──────────────────────────────────────────────────────────────

    /// Parse the contents of `{ ... }` — a comma-separated list of shape elements.
    fn parse_shape_body(&mut self) -> Result<Vec<ShapeElement>, PyQLSyntaxError> {
        let mut elements = vec![];
        while !matches!(self.current(), Token::RBrace | Token::Eof) {
            elements.push(self.parse_shape_element()?);
            if matches!(self.current(), Token::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(elements)
    }

    fn parse_shape_element(&mut self) -> Result<ShapeElement, PyQLSyntaxError> {
        let element_offset = Some(self.current_offset());
        // Type intersection shape element: `[is Type].*`, `[is Type].**`, or `[is Type].field`
        if matches!(self.current(), Token::LBracket) && matches!(self.peek_ahead(1), Token::Is) {
            self.advance(); // [
            self.advance(); // is
            let type_ref = self.parse_object_ref()?;
            self.eat(&Token::RBracket)?;

            // Must be followed by .
            self.eat(&Token::Dot)?;

            // .* or .** — type intersection splat
            if matches!(self.current(), Token::Star | Token::StarStar) {
                let splat = if matches!(self.current(), Token::StarStar) {
                    self.advance();
                    Splat::Deep
                } else {
                    self.advance();
                    Splat::Shallow
                };
                return Ok(ShapeElement {
                    path: Path {
                        steps: vec![PathStep::TypeIntersection(type_ref)],
                        partial: true,
                    },
                    splat: Some(splat),
                    nested: None,
                    compexpr: None,
                    op: ShapeOp::Assign,
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    marker_offset: element_offset,
                    limit: None,
                });
            }

            // .field_name — type intersection field access
            let field_name = self.eat_ident()?;
            let path = Path {
                steps: vec![PathStep::TypeIntersection(type_ref), PathStep::Name(field_name)],
                partial: true,
            };
            // Could be followed by := for computed alias
            if matches!(self.current(), Token::ColonEq) {
                self.advance();
                let compexpr = self.parse_expr()?;
                return Ok(ShapeElement {
                    path,
                    splat: None,
                    nested: None,
                    compexpr: Some(compexpr),
                    op: ShapeOp::Assign,
                    filter: None,
                    order_by: vec![],
                    offset: None,
                    marker_offset: element_offset,
                    limit: None,
                });
            }
            // `[is T].configs: { … }` — the narrowed pointer read with a shape
            // of its own, the same inclusion a plain `configs: { … }` is.
            let nested = if matches!(self.current(), Token::Colon) {
                self.advance();
                self.eat(&Token::LBrace)?;
                let elements = self.parse_shape_body()?;
                self.eat(&Token::RBrace)?;
                Some(elements)
            } else {
                None
            };
            return Ok(ShapeElement {
                path,
                splat: None,
                nested,
                compexpr: None,
                op: ShapeOp::Assign,
                filter: None,
                order_by: vec![],
                offset: None,
                marker_offset: element_offset,
                limit: None,
            });
        }

        // Wildcard splats: `*` (shallow) and `**` (deep)
        if matches!(self.current(), Token::StarStar) {
            self.advance();
            return Ok(ShapeElement::splat(Splat::Deep));
        }
        if matches!(self.current(), Token::Star) {
            self.advance();
            return Ok(ShapeElement::splat(Splat::Shallow));
        }

        // Link property in a nested shape: `@name` (read), or `@name := expr`
        // (write — only valid attached to a multi-link mutation's target
        // expression, e.g. `(select Tag filter ...) { @weight := <float64>$w }`).
        if matches!(self.current(), Token::At) {
            self.advance();
            let name = self.eat_ident()?;
            let path = Path {
                steps: vec![PathStep::LinkProp(name)],
                partial: true,
            };
            let compexpr = if matches!(self.current(), Token::ColonEq) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            return Ok(ShapeElement {
                path,
                splat: None,
                nested: None,
                compexpr,
                op: ShapeOp::Assign,
                filter: None,
                order_by: vec![],
                offset: None,
                marker_offset: element_offset,
                limit: None,
            });
        }

        // Shape elements are partial paths relative to the shaped object.
        // They may start with `.name` or bare `name`.
        let path = if matches!(self.current(), Token::Dot) {
            self.advance();
            let name = self.eat_ident()?;
            Path::relative(name)
        } else {
            let name = self.eat_ident()?;
            Path::relative(name)
        };

        // Assignment operators: `:=` (assign), `+=` (append), `-=` (remove)
        if matches!(self.current(), Token::ColonEq | Token::PlusEq | Token::MinusEq) {
            let op = match self.current() {
                Token::ColonEq => ShapeOp::Assign,
                Token::PlusEq => ShapeOp::Append,
                Token::MinusEq => ShapeOp::Remove,
                _ => unreachable!(),
            };
            self.advance();
            let compexpr = self.parse_expr()?;
            return Ok(ShapeElement {
                path,
                splat: None,
                nested: None,
                compexpr: Some(compexpr),
                op,
                filter: None,
                order_by: vec![],
                offset: None,
                marker_offset: element_offset,
                limit: None,
            });
        }

        // Nested shape: `.link { ... }` or `.link: { ... }` (colon optional)
        if matches!(self.current(), Token::Colon) {
            self.advance();
        }
        if matches!(self.current(), Token::LBrace) {
            self.advance();
            let nested_elements = self.parse_shape_body()?;
            self.eat(&Token::RBrace)?;

            // Optional per-link modifiers
            let filter = if matches!(self.current(), Token::Filter) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            let order_by = if matches!(self.current(), Token::Order) {
                self.advance();
                self.eat(&Token::By)?;
                self.parse_sort_list()?
            } else {
                vec![]
            };
            let offset = if matches!(self.current(), Token::Offset) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            let limit = if matches!(self.current(), Token::Limit) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };

            return Ok(ShapeElement {
                path,
                splat: None,
                nested: Some(nested_elements),
                compexpr: None,
                op: ShapeOp::Assign,
                filter,
                order_by,
                offset,
                marker_offset: element_offset,
                limit,
            });
        }

        // Bare inclusion: `.name`
        Ok(ShapeElement {
            path,
            splat: None,
            nested: None,
            compexpr: None,
            op: ShapeOp::Assign,
            filter: None,
            order_by: vec![],
            offset: None,
            marker_offset: element_offset,
            limit: None,
        })
    }
}
