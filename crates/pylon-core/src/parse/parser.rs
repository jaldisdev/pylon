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
            Err(self.err(&format!("expected {expected:?}, got {:?}", self.current())))
        }
    }

    fn eat_ident(&mut self) -> Result<String, PyQLSyntaxError> {
        let name = self.keyword_as_ident();
        if let Some(s) = name {
            self.advance();
            return Ok(s);
        }
        match self.current().clone() {
            Token::Ident(s) => { self.advance(); Ok(s) }
            other => Err(self.err(&format!("expected identifier, got {other:?}"))),
        }
    }

    /// If the current token is a keyword that is also a legal identifier,
    /// return its string form without consuming it. Returns `None` for tokens
    /// that are never valid identifiers (punctuation, literals, EOF).
    fn keyword_as_ident(&self) -> Option<String> {
        match self.current() {
            Token::Select    => Some("select".into()),
            Token::Insert    => Some("insert".into()),
            Token::Update    => Some("update".into()),
            Token::Delete    => Some("delete".into()),
            Token::Filter    => Some("filter".into()),
            Token::Order     => Some("Order".into()),
            Token::By        => Some("by".into()),
            Token::Asc       => Some("asc".into()),
            Token::Desc      => Some("desc".into()),
            Token::First     => Some("first".into()),
            Token::Last      => Some("last".into()),
            Token::Limit     => Some("limit".into()),
            Token::Offset    => Some("offset".into()),
            Token::With      => Some("with".into()),
            Token::For       => Some("for".into()),
            Token::In        => Some("in".into()),
            Token::Union     => Some("union".into()),
            Token::Except    => Some("except".into()),
            Token::Intersect => Some("intersect".into()),
            Token::Not       => Some("not".into()),
            Token::And       => Some("and".into()),
            Token::Or        => Some("or".into()),
            Token::Exists    => Some("exists".into()),
            Token::Distinct  => Some("distinct".into()),
            Token::If        => Some("if".into()),
            Token::Then      => Some("then".into()),
            Token::Else      => Some("else".into()),
            Token::Set       => Some("set".into()),
            Token::Is        => Some("is".into()),
            Token::Optional  => Some("optional".into()),
            Token::Required  => Some("required".into()),
            Token::Unless    => Some("unless".into()),
            Token::Conflict  => Some("conflict".into()),
            Token::Detached  => Some("detached".into()),
            Token::Group     => Some("group".into()),
            Token::Using     => Some("using".into()),
            Token::Like      => Some("like".into()),
            Token::Ilike     => Some("ilike".into()),
            Token::True      => Some("true".into()),
            Token::False     => Some("false".into()),
            _ => None,
        }
    }

    fn err(&self, msg: &str) -> PyQLSyntaxError {
        PyQLSyntaxError { message: msg.to_string(), position: self.current_pos() }
    }

    fn at_end(&self) -> bool {
        matches!(self.current(), Token::Eof)
    }

    // ── Top-level ───────────────────────────────────────────────────────────────

    pub fn parse_stmt(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        let stmt = match self.current() {
            Token::With => self.parse_with(),
            Token::For => self.parse_for(),
            Token::Select => self.parse_select(),
            Token::Insert => self.parse_insert(),
            Token::Update => self.parse_update(),
            Token::Delete => self.parse_delete(),
            Token::Group => self.parse_group(),
            _ => Err(self.err(&format!(
                "expected WITH, FOR, SELECT, INSERT, UPDATE, DELETE, or GROUP, got {:?}",
                self.current()
            ))),
        }?;
        // Optional trailing semicolon
        if matches!(self.current(), Token::Semicolon) {
            self.advance();
        }
        if !self.at_end() {
            return Err(self.err(&format!("unexpected token {:?}", self.current())));
        }
        Ok(stmt)
    }

    // ── SELECT ──────────────────────────────────────────────────────────────────

    fn parse_select(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Select)?;
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

        Ok(Stmt::Select(SelectStmt { result, filter, order_by, offset, limit }))
    }

    fn parse_sort_list(&mut self) -> Result<Vec<SortExpr>, PyQLSyntaxError> {
        let mut list = vec![self.parse_sort_expr()?];
        while matches!(self.current(), Token::Ident(s) if s.eq_ignore_ascii_case("then")) {
            self.advance();
            list.push(self.parse_sort_expr()?);
        }
        Ok(list)
    }

    fn parse_sort_expr(&mut self) -> Result<SortExpr, PyQLSyntaxError> {
        let expr = self.parse_expr()?;
        let direction = match self.current() {
            Token::Asc => { self.advance(); SortDirection::Asc }
            Token::Desc => { self.advance(); SortDirection::Desc }
            _ => SortDirection::Asc,
        };
        let nones = match self.current() {
            Token::Ident(s) if s.eq_ignore_ascii_case("EMPTY") => {
                self.advance();
                match self.current() {
                    Token::First => { self.advance(); NonesOrder::First }
                    Token::Last => { self.advance(); NonesOrder::Last }
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

        Ok(Stmt::Insert(InsertStmt { subject, shape, unless_conflict }))
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
        }
        let stmt = self.parse_inner_stmt()?;
        Ok(Stmt::With(WithStmt { aliases, stmt: Box::new(stmt) }))
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
                "expected WITH, FOR, SELECT, INSERT, UPDATE, DELETE, or GROUP, got {:?}",
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
        let body = if matches!(self.current(), Token::LParen) {
            // Parenthesised body: `(select ...)`, `(insert ...)`, etc.
            let expr = self.parse_paren_expr()?;
            match expr {
                Expr::SubQuery(stmt) => *stmt,
                _ => return Err(self.err("for loop body must be a statement")),
            }
        } else {
            self.parse_inner_stmt()?
        };
        Ok(Stmt::For(ForStmt { var, optional, iterator, body: Box::new(body) }))
    }

    fn parse_group(&mut self) -> Result<Stmt, PyQLSyntaxError> {
        self.eat(&Token::Group)?;
        // Subject: a type name (path) optionally followed by a shape.
        let subject_expr = self.parse_postfix()?;
        let (subject, shape) = if let Expr::Shape(sh) = subject_expr {
            let inner = sh.expr.map(|e| e).unwrap_or(Expr::Path(crate::parse::ast::Path {
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

        Ok(Stmt::Group(crate::parse::ast::GroupStmt { subject, shape, using, by }))
    }

    // ── Expressions ─────────────────────────────────────────────────────────────

    // Precedence (lowest → highest):
    //   union → if/else → or → and → not → comparison → coalesce → add/concat → mul → pow → unary → postfix

    pub fn parse_expr(&mut self) -> Result<Expr, PyQLSyntaxError> {
        self.parse_union()
    }

    fn parse_union(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_if_else()?;
        while matches!(self.current(), Token::Union) {
            self.advance();
            let right = self.parse_if_else()?;
            left = Expr::Union(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_if_else(&mut self) -> Result<Expr, PyQLSyntaxError> {
        // Gel-style prefix: if condition then value else fallback
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
            left = Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::Or, right }));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut left = self.parse_not()?;
        while matches!(self.current(), Token::And) {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::And, right }));
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
            return Ok(Expr::UnaryOp(Box::new(UnaryOp { op: UnaryOpKind::Not, operand })));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let left = self.parse_coalesce()?;
        let op = match self.current() {
            Token::Eq => BinOpKind::Eq,
            Token::Ne => BinOpKind::Ne,
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
                return Ok(Expr::TypeIs { expr: Box::new(left), ty });
            }
            Token::Not => {
                // NOT LIKE / NOT ILIKE / NOT IN
                match self.peek_ahead(1) {
                    Token::Like => {
                        self.advance(); self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::NotLike, right })));
                    }
                    Token::Ilike => {
                        self.advance(); self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::NotIlike, right })));
                    }
                    Token::In => {
                        self.advance(); self.advance();
                        let right = self.parse_coalesce()?;
                        return Ok(Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::NotIn, right })));
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
            let right = self.parse_add()?;
            left = Expr::BinOp(Box::new(BinOp { left, op: BinOpKind::Coalesce, right }));
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
            return Ok(Expr::BinOp(Box::new(BinOp { left: base, op: BinOpKind::Pow, right: exp })));
        }
        Ok(base)
    }

    fn parse_unary(&mut self) -> Result<Expr, PyQLSyntaxError> {
        match self.current() {
            Token::Minus => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp { op: UnaryOpKind::Minus, operand })))
            }
            Token::Exists => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp { op: UnaryOpKind::Exists, operand })))
            }
            Token::Distinct => {
                self.advance();
                let operand = self.parse_unary()?;
                Ok(Expr::UnaryOp(Box::new(UnaryOp { op: UnaryOpKind::Distinct, operand })))
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
        if i >= n { return false; }
        // must start with an identifier
        if !matches!(self.tokens[i].token, Token::Ident(_)) { return false; }
        i += 1;
        if i >= n { return false; }
        // optional `::` Name
        if matches!(self.tokens[i].token, Token::ColonColon) {
            i += 1;
            if i >= n { return false; }
            if !matches!(self.tokens[i].token, Token::Ident(_)) { return false; }
            i += 1;
        }
        // must be followed by `>`
        i < n && matches!(self.tokens[i].token, Token::Gt)
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr, PyQLSyntaxError> {
        let first = self.eat_ident()?;
        if matches!(self.current(), Token::ColonColon) {
            self.advance();
            let name = self.eat_ident()?;
            Ok(TypeExpr { module: Some(first), name })
        } else {
            Ok(TypeExpr { module: None, name: first })
        }
    }

    // Postfix: shape `{...}`, dot traversal, type intersection `[is T]`, link prop `@prop`
    fn parse_postfix(&mut self) -> Result<Expr, PyQLSyntaxError> {
        let mut expr = self.parse_primary()?;

        loop {
            match self.current() {
                Token::LBrace => {
                    self.advance();
                    let elements = self.parse_shape_body()?;
                    self.eat(&Token::RBrace)?;
                    expr = Expr::Shape(Box::new(ShapeExpr { expr: Some(expr), elements }));
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
                        expr = Expr::TupleIndex { expr: Box::new(expr), index: n as usize };
                    } else {
                        let name = self.eat_ident()?;
                        // Path expressions extend the path; everything else uses FieldAccess.
                        expr = match expr {
                            Expr::Path(_) => self.extend_path(expr, PathStep::Name(name))?,
                            other => Expr::FieldAccess { expr: Box::new(other), field: name },
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
                            expr = Expr::Slice { expr: Box::new(expr), lower, upper };
                        } else {
                            let index = lower.ok_or_else(|| self.err("expected index expression"))?;
                            self.eat(&Token::RBracket)?;
                            expr = Expr::Index { expr: Box::new(expr), index };
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
            other => {
                // For non-path expressions (e.g. `(SELECT ...).field`), we'd need
                // a Path with an Expr head — not supported in Phase 1.
                Err(PyQLSyntaxError {
                    message: format!("path traversal on non-path expression: {other:?}"),
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

            // Parameter `$name`
            Token::Dollar => {
                self.advance();
                let name = self.eat_ident()?;
                Ok(Expr::Parameter(name))
            }

            // Literals
            Token::IntLit(n) => { self.advance(); Ok(Expr::Literal(Literal::Int(n))) }
            Token::FloatLit(f) => { self.advance(); Ok(Expr::Literal(Literal::Float(f))) }
            Token::DecimalLit(s) => {
                self.advance();
                // Emit as <decimal>str — compiles to 'value'::numeric
                Ok(Expr::TypeCast(Box::new(TypeCast {
                    ty: TypeExpr { module: Some("std".to_string()), name: "decimal".to_string() },
                    expr: Expr::Literal(Literal::Str(s)),
                })))
            }
            Token::StrLit(s) => { self.advance(); Ok(Expr::Literal(Literal::Str(s))) }
            Token::True => { self.advance(); Ok(Expr::Literal(Literal::Bool(true))) }
            Token::False => { self.advance(); Ok(Expr::Literal(Literal::Bool(false))) }

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
                        matches!(self.peek_ahead(1), Token::Ident(_))
                            && matches!(self.peek_ahead(2), Token::ColonEq)
                    }
                    _ => false,
                };
                if is_free_object {
                    let elements = self.parse_shape_body()?;
                    self.eat(&Token::RBrace)?;
                    return Ok(Expr::Shape(Box::new(ShapeExpr { expr: None, elements })));
                }
                // Set literal: comma-separated value expressions
                let mut elems = vec![self.parse_expr()?];
                while matches!(self.current(), Token::Comma) {
                    self.advance();
                    if matches!(self.current(), Token::RBrace) { break; }
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
                    if matches!(self.current(), Token::RBracket) { break; }
                    elems.push(self.parse_expr()?);
                }
                self.eat(&Token::RBracket)?;
                Ok(Expr::Array(elems))
            }

            // Identifier — either a type reference (absolute path) or a function call
            Token::Ident(name) => {
                let name = name.clone();
                self.advance();

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

            Token::Detached => {
                self.advance();
                let inner = self.parse_expr()?;
                Ok(Expr::Detached(Box::new(inner)))
            }

            other => Err(self.err(&format!("unexpected token {other:?}"))),
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
        if matches!(self.current(), Token::ColonEq) {
            if let Expr::Path(ref p) = first {
                if !p.partial && p.steps.len() == 1 {
                    if let PathStep::Name(ref label) = p.steps[0] {
                        let label = label.clone();
                        self.advance(); // :=
                        let val = self.parse_expr()?;
                        let mut pairs = vec![(label, val)];
                        while matches!(self.current(), Token::Comma) {
                            self.advance();
                            if matches!(self.current(), Token::RParen) { break; }
                            let key = self.eat_ident()?;
                            self.eat(&Token::ColonEq)?;
                            let val = self.parse_expr()?;
                            pairs.push((key, val));
                        }
                        self.eat(&Token::RParen)?;
                        return Ok(Expr::NamedTuple(pairs));
                    }
                }
            }
        }

        // Anonymous tuple or grouped expression
        if matches!(self.current(), Token::Comma) {
            let mut elems = vec![first];
            while matches!(self.current(), Token::Comma) {
                self.advance();
                if matches!(self.current(), Token::RParen) { break; }
                elems.push(self.parse_expr()?);
            }
            self.eat(&Token::RParen)?;
            return Ok(Expr::Tuple(elems));
        }

        self.eat(&Token::RParen)?;
        Ok(first)
    }

    fn parse_func_call_args(
        &mut self,
        module: Option<String>,
        name: String,
    ) -> Result<Expr, PyQLSyntaxError> {
        self.eat(&Token::LParen)?;
        let mut args = vec![];
        let mut kwargs = vec![];
        if !matches!(self.current(), Token::RParen) {
            loop {
                // Named argument: `name := expr`
                if matches!(self.current(), Token::Ident(_))
                    && matches!(self.peek_ahead(1), Token::ColonEq)
                {
                    let key = self.eat_ident()?;
                    self.eat(&Token::ColonEq)?;
                    let val = self.parse_expr()?;
                    kwargs.push((key, val));
                } else {
                    args.push(self.parse_expr()?);
                }
                if !matches!(self.current(), Token::Comma) {
                    break;
                }
                self.advance();
                if matches!(self.current(), Token::RParen) { break; }
            }
        }
        self.eat(&Token::RParen)?;
        Ok(Expr::FunctionCall(FunctionCall { module, name, args, kwargs }))
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
                    path: Path { steps: vec![PathStep::TypeIntersection(type_ref)], partial: true },
                    splat: Some(splat),
                    nested: None,
                    compexpr: None,
                    op: ShapeOp::Assign,
                    filter: None,
                    order_by: vec![],
                    offset: None,
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
                    limit: None,
                });
            }
            return Ok(ShapeElement {
                path,
                splat: None,
                nested: None,
                compexpr: None,
                op: ShapeOp::Assign,
                filter: None,
                order_by: vec![],
                offset: None,
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

        // Link property in a nested shape: `@name`
        if matches!(self.current(), Token::At) {
            self.advance();
            let name = self.eat_ident()?;
            let path = Path {
                steps: vec![PathStep::LinkProp(name)],
                partial: true,
            };
            return Ok(ShapeElement {
                path,
                splat: None,
                nested: None,
                compexpr: None,
                op: ShapeOp::Assign,
                filter: None,
                order_by: vec![],
                offset: None,
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
                limit: None,
            });
        }

        // Nested shape: `.link { ... }` or `.link: { ... }` (Gel-style colon optional)
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
            limit: None,
        })
    }
}
