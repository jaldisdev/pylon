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

pub mod ast;
mod lexer;
mod parser;

use crate::error::PyQLSyntaxError;

pub use ast::{Expr, Stmt};

pub fn parse(input: &str) -> Result<Stmt, PyQLSyntaxError> {
    let tokens = lexer::Lexer::new(input).tokenize()?;
    parser::Parser::new(tokens).parse_stmt()
}

/// Parse a single PyQL expression (used for rewrite handlers, computed-field
/// bodies, and constraint expressions stored as strings in the schema).
pub fn parse_expr(input: &str) -> Result<Expr, PyQLSyntaxError> {
    let tokens = lexer::Lexer::new(input).tokenize()?;
    parser::Parser::new(tokens).parse_expr()
}

/// Parse the body of a computed pointer, default, or any other schema-level
/// PyQL fragment that stands alone rather than sitting inside a statement.
///
/// Identical to `parse_expr`, except that trailing `FILTER`/`ORDER BY`/
/// `OFFSET`/`LIMIT` are allowed without an enclosing `select`: a fragment
/// has no statement around it to hang them off, so
/// `".orders order by .created_at desc limit 5"` used to be a bare syntax
/// error ("expected an expression, found 'order'") and had to be written as
/// `"(select .orders order by …)"`. Both spellings now parse, to the same
/// sub-select.
pub fn parse_pointer_expr(input: &str) -> Result<Expr, PyQLSyntaxError> {
    let tokens = lexer::Lexer::new(input).tokenize()?;
    parser::Parser::new(tokens).parse_pointer_expr()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ast::*;

    #[test]
    fn test_select_bare_type() {
        let stmt = parse("SELECT Person").unwrap();
        assert!(matches!(
            stmt,
            Stmt::Select(SelectStmt {
                result: Expr::Path(_),
                ..
            })
        ));
    }

    #[test]
    fn test_select_with_shape() {
        let stmt = parse("SELECT Person { name, age }").unwrap();
        let Stmt::Select(sel) = stmt else {
            panic!("not a select")
        };
        let Expr::Shape(shape) = sel.result else {
            panic!("not a shape")
        };
        assert_eq!(shape.elements.len(), 2);
        assert_eq!(shape.elements[0].path, Path::relative("name"));
        assert_eq!(shape.elements[1].path, Path::relative("age"));
    }

    #[test]
    fn test_select_filter_param() {
        let stmt = parse("SELECT Person { name, age } FILTER .name = $name").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert!(sel.filter.is_some());
        let filter = sel.filter.unwrap();
        let Expr::BinOp(binop) = filter else {
            panic!("not a binop")
        };
        assert_eq!(binop.op, BinOpKind::Eq);
        assert!(matches!(binop.left, Expr::Path(Path { partial: true, .. })));
        assert!(matches!(binop.right, Expr::Parameter(_)));
    }

    #[test]
    fn test_shape_element_bare_select_without_parens() {
        let stmt = parse("SELECT Person { primary := SELECT .emails FILTER .primary = true LIMIT 1 }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(shape) = sel.result else {
            panic!("not a shape")
        };
        assert_eq!(shape.elements.len(), 1);
        let Some(Expr::SubQuery(inner)) = &shape.elements[0].compexpr else {
            panic!("not a subquery")
        };
        let Stmt::Select(inner) = inner.as_ref() else { panic!() };
        assert!(inner.filter.is_some());
        assert!(inner.limit.is_some());
    }

    #[test]
    fn test_bare_select_stops_at_shape_separator() {
        let stmt = parse("SELECT Person { a := SELECT .emails LIMIT 1, b := .name }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(shape) = sel.result else {
            panic!("not a shape")
        };
        assert_eq!(shape.elements.len(), 2);
        assert!(matches!(shape.elements[1].compexpr, Some(Expr::Path(_))));
    }

    #[test]
    fn test_bare_with_in_expression_position() {
        let expr = parse_expr("WITH n := 1 SELECT n").unwrap();
        assert!(matches!(expr, Expr::SubQuery(_)));
    }

    #[test]
    fn test_bare_for_union_in_expression_position() {
        let expr = parse_expr("FOR x IN {1, 2} UNION (x + 1)").unwrap();
        assert!(matches!(expr, Expr::SubQuery(_)));
    }

    #[test]
    fn test_cast_may_declare_a_parameter_cardinality() {
        for query in ["<optional std::str>$token", "<required std::str>$token"] {
            let expr = parse_expr(query).unwrap_or_else(|e| panic!("{query}: {e}"));
            assert!(matches!(expr, Expr::TypeCast(_)), "{query}");
        }
        // Still a comparison, not a cast.
        let expr = parse_expr("1 < 2").unwrap();
        assert!(matches!(expr, Expr::BinOp(_)));
    }

    #[test]
    fn test_keyword_used_as_a_name_keeps_its_written_casing() {
        let expr = parse_expr(".<order[is OrderAttribute]").unwrap();
        let Expr::Path(path) = expr else { panic!("not a path") };
        assert_eq!(path.steps[0], PathStep::Backlink("order".into()));
        let expr = parse_expr(".<Order[is OrderAttribute]").unwrap();
        let Expr::Path(path) = expr else { panic!("not a path") };
        assert_eq!(path.steps[0], PathStep::Backlink("Order".into()));
    }

    #[test]
    fn test_string_literal_preserves_multibyte_utf8() {
        // Regression: the lexer scans raw bytes; a naive `byte as char` cast
        // on non-ASCII bytes previously corrupted multi-byte UTF-8 sequences
        // (each byte became its own Latin-1-style codepoint instead of being
        // reassembled into the real scalar value).
        let expr = parse_expr("'I ❤️ Pylon!'").unwrap();
        assert!(matches!(expr, Expr::Literal(Literal::Str(s)) if s == "I ❤️ Pylon!"));
    }

    #[test]
    fn test_positional_param_parsed() {
        let expr = parse_expr("$0").unwrap();
        assert!(matches!(expr, Expr::Parameter(n) if n == "0"));
    }

    #[test]
    fn test_multiple_positional_params_parsed() {
        let stmt = parse("SELECT Person FILTER .name = $0 AND .age > $1").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let filter = sel.filter.unwrap();
        let Expr::BinOp(outer) = filter else {
            panic!("not a binop")
        };
        let Expr::BinOp(left) = outer.left else {
            panic!("left not binop")
        };
        assert!(matches!(left.right, Expr::Parameter(n) if n == "0"));
        let Expr::BinOp(right) = outer.right else {
            panic!("right not binop")
        };
        assert!(matches!(right.right, Expr::Parameter(n) if n == "1"));
    }

    #[test]
    fn test_select_nested_shape() {
        let stmt = parse("SELECT Person { name, posts { title, body } }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(shape) = sel.result else { panic!() };
        assert_eq!(shape.elements.len(), 2);
        let posts = &shape.elements[1];
        assert_eq!(posts.path, Path::relative("posts"));
        assert!(posts.nested.is_some());
        assert_eq!(posts.nested.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_select_set_literal() {
        let stmt = parse("SELECT {1, 2, 3}").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Set(elems) = sel.result else {
            panic!("expected Set")
        };
        assert_eq!(elems.len(), 3);
        assert!(matches!(elems[0], Expr::Literal(Literal::Int(1))));
    }

    #[test]
    fn test_select_set_literal_single() {
        let stmt = parse("SELECT {42}").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Set(elems) = sel.result else {
            panic!("expected Set")
        };
        assert_eq!(elems.len(), 1);
    }

    #[test]
    fn test_analyze_wraps_inner_stmt() {
        let stmt = parse("analyze select Person { name }").unwrap();
        let Stmt::Analyze(inner) = stmt else {
            panic!("expected Analyze")
        };
        assert!(matches!(*inner, Stmt::Select(_)));
    }

    #[test]
    fn test_analyze_is_case_insensitive_and_not_reserved_elsewhere() {
        assert!(matches!(parse("ANALYZE select Person").unwrap(), Stmt::Analyze(_)));
        // "analyze" stays a legal identifier everywhere except as the
        // leading token of a statement — it's a soft keyword, not reserved.
        let stmt = parse("SELECT Person { name }").unwrap();
        assert!(matches!(stmt, Stmt::Select(_)));
    }

    #[test]
    fn test_analyze_wraps_insert_and_update_too() {
        assert!(matches!(
            parse("analyze insert Person { name := 'a' }").unwrap(),
            Stmt::Analyze(inner) if matches!(*inner, Stmt::Insert(_))
        ));
        assert!(matches!(
            parse("analyze update Person set { name := 'a' }").unwrap(),
            Stmt::Analyze(inner) if matches!(*inner, Stmt::Update(_))
        ));
    }

    #[test]
    fn test_analyze_marker_offsets_mark_root_and_nested_shape_elements() {
        let query = "analyze select Person { name, posts { title } }";
        let stmt = parse(query).unwrap();
        let Stmt::Analyze(inner) = stmt else {
            panic!("expected Analyze")
        };
        let Stmt::Select(sel) = *inner else { panic!() };
        let Expr::Shape(shape) = sel.result else { panic!() };

        // Root marker sits right at "Person", after "analyze select ".
        let root_offset = shape.marker_offset.expect("root shape should carry an offset");
        assert_eq!(&query[root_offset..root_offset + "Person".len()], "Person");

        let name_offset = shape.elements[0]
            .marker_offset
            .expect("name element should carry an offset");
        assert_eq!(&query[name_offset..name_offset + "name".len()], "name");

        let posts = &shape.elements[1];
        let posts_offset = posts.marker_offset.expect("posts element should carry an offset");
        assert_eq!(&query[posts_offset..posts_offset + "posts".len()], "posts");

        let title_offset = posts.nested.as_ref().unwrap()[0]
            .marker_offset
            .expect("nested element should carry an offset");
        assert_eq!(&query[title_offset..title_offset + "title".len()], "title");
    }

    #[test]
    fn test_select_free_object() {
        let stmt = parse("SELECT { foo := 'bar', n := 42 }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(sh) = sel.result else {
            panic!("expected Shape")
        };
        assert!(sh.expr.is_none());
        assert_eq!(sh.elements.len(), 2);
        assert_eq!(sh.elements[0].path, Path::relative("foo"));
        assert!(sh.elements[0].compexpr.is_some());
    }

    #[test]
    fn test_select_tuple_expr() {
        let stmt = parse("SELECT (1, 'hello')").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert!(matches!(sel.result, Expr::Tuple(_)));
    }

    #[test]
    fn test_select_scalar_function() {
        let stmt = parse("SELECT str_lower('HELLO')").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert!(matches!(sel.result, Expr::FunctionCall(_)));
    }

    #[test]
    fn test_shape_splat_shallow() {
        let stmt = parse("SELECT Person { * }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(sh) = &sel.result else { panic!() };
        assert_eq!(sh.elements.len(), 1);
        assert!(matches!(sh.elements[0].splat, Some(ast::Splat::Shallow)));
    }

    #[test]
    fn test_shape_splat_deep() {
        let stmt = parse("SELECT Person { ** }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(sh) = &sel.result else { panic!() };
        assert_eq!(sh.elements.len(), 1);
        assert!(matches!(sh.elements[0].splat, Some(ast::Splat::Deep)));
    }

    #[test]
    fn test_select_order_by_limit_offset() {
        let stmt = parse("SELECT Person { name } ORDER BY .name ASC OFFSET 10 LIMIT 5").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(sel.order_by.len(), 1);
        assert_eq!(sel.order_by[0].direction, SortDirection::Asc);
        assert!(matches!(sel.offset, Some(Expr::Literal(Literal::Int(10)))));
        assert!(matches!(sel.limit, Some(Expr::Literal(Literal::Int(5)))));
    }

    #[test]
    fn test_select_for_update_defaults_to_blocking() {
        let stmt = parse("SELECT Person FOR UPDATE").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::Update,
                wait: LockWait::Block
            })
        );
    }

    #[test]
    fn test_select_for_update_skip_locked() {
        let stmt = parse("SELECT Person FOR UPDATE SKIP LOCKED").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::Update,
                wait: LockWait::SkipLocked
            })
        );
    }

    #[test]
    fn test_select_for_update_nowait() {
        let stmt = parse("SELECT Person FOR UPDATE NOWAIT").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::Update,
                wait: LockWait::NoWait
            })
        );
    }

    #[test]
    fn test_select_for_share_skip_locked_is_case_insensitive() {
        let stmt = parse("select Person for share skip locked").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::Share,
                wait: LockWait::SkipLocked
            })
        );
    }

    #[test]
    fn test_select_for_no_key_update() {
        let stmt = parse("SELECT Person FOR NO KEY UPDATE").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::NoKeyUpdate,
                wait: LockWait::Block
            })
        );
    }

    #[test]
    fn test_select_for_key_share() {
        let stmt = parse("SELECT Person FOR KEY SHARE").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::KeyShare,
                wait: LockWait::Block
            })
        );
    }

    #[test]
    fn test_select_for_update_comes_after_order_by_limit_offset() {
        let stmt = parse("SELECT Person { name } ORDER BY .name OFFSET 1 LIMIT 5 FOR UPDATE SKIP LOCKED").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(sel.order_by.len(), 1);
        assert!(sel.offset.is_some());
        assert!(sel.limit.is_some());
        assert_eq!(
            sel.lock,
            Some(LockClause {
                strength: LockStrength::Update,
                wait: LockWait::SkipLocked
            })
        );
    }

    #[test]
    fn test_select_with_no_lock_clause_defaults_to_none() {
        let stmt = parse("SELECT Person").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert_eq!(sel.lock, None);
    }

    #[test]
    fn test_select_for_garbage_strength_is_a_clear_error() {
        let err = parse("SELECT Person FOR BOGUS").unwrap_err();
        assert!(err.message.contains("UPDATE"), "unexpected: {}", err.message);
    }

    #[test]
    fn test_select_for_no_without_key_is_a_clear_error() {
        let err = parse("SELECT Person FOR NO UPDATE").unwrap_err();
        assert!(err.message.contains("KEY"), "unexpected: {}", err.message);
    }

    #[test]
    fn test_select_for_skip_without_locked_is_a_clear_error() {
        let err = parse("SELECT Person FOR UPDATE SKIP").unwrap_err();
        assert!(err.message.contains("LOCKED"), "unexpected: {}", err.message);
    }

    #[test]
    fn test_boolean_operators() {
        let stmt = parse("SELECT Person FILTER .active = true AND .age >= 18 OR .admin = true").unwrap();
        assert!(matches!(stmt, Stmt::Select(_)));
    }

    #[test]
    fn test_function_call() {
        let stmt = parse("SELECT count(Person)").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert!(matches!(sel.result, Expr::FunctionCall(_)));
    }

    #[test]
    fn test_type_cast() {
        let stmt = parse("SELECT <str>$value").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        assert!(matches!(sel.result, Expr::TypeCast(_)));
    }

    #[test]
    fn test_computed_shape_element() {
        let stmt = parse("SELECT Person { full_name := .first ++ ' ' ++ .last }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::Shape(shape) = sel.result else { panic!() };
        let el = &shape.elements[0];
        assert!(el.compexpr.is_some());
    }

    #[test]
    fn test_insert() {
        let stmt = parse("INSERT Person { name := 'Alice', age := 30 }").unwrap();
        let Stmt::Insert(ins) = stmt else { panic!() };
        assert_eq!(ins.subject.name, "Person");
        assert_eq!(ins.shape.len(), 2);
    }

    #[test]
    fn test_update() {
        let stmt = parse("UPDATE Person FILTER .name = 'Alice' SET { age := 31 }").unwrap();
        assert!(matches!(stmt, Stmt::Update(_)));
    }

    #[test]
    fn test_delete() {
        let stmt = parse("DELETE Person FILTER .name = 'Alice'").unwrap();
        assert!(matches!(stmt, Stmt::Delete(_)));
    }

    #[test]
    fn test_if_else_postfix() {
        let stmt = parse("SELECT 'yes' IF 1 = 1 ELSE 'no'").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::IfElse(ie) = sel.result else {
            panic!("expected IfElse")
        };
        assert!(matches!(ie.if_expr, Expr::Literal(Literal::Str(_))));
        assert!(matches!(ie.condition, Expr::BinOp(_)));
        assert!(matches!(ie.else_expr, Expr::Literal(Literal::Str(_))));
    }

    #[test]
    fn test_if_then_else_prefix() {
        let stmt = parse("SELECT IF 1 = 1 THEN 'yes' ELSE 'no'").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::IfElse(ie) = sel.result else {
            panic!("expected IfElse")
        };
        assert!(matches!(ie.if_expr, Expr::Literal(Literal::Str(_))));
        assert!(matches!(ie.condition, Expr::BinOp(_)));
        assert!(matches!(ie.else_expr, Expr::Literal(Literal::Str(_))));
    }

    #[test]
    fn test_if_then_else_chained() {
        let stmt = parse("SELECT IF 1 = 1 THEN 'a' ELSE IF 2 = 2 THEN 'b' ELSE 'c'").unwrap();
        let Stmt::Select(sel) = stmt else { panic!() };
        let Expr::IfElse(outer) = sel.result else {
            panic!("expected IfElse")
        };
        assert!(matches!(outer.else_expr, Expr::IfElse(_)));
    }

    #[test]
    fn test_nested_module_path_in_function_call_is_a_clear_error() {
        // Regression: `ext::pgcrypto::digest(...)` (a 3-segment module path
        // — PyQL module names are a single segment, e.g. `crypto::digest`)
        // used to silently stop consuming after the first `::`, leaving the
        // second `::` dangling to surface as a confusing "unexpected token
        // ColonColon" once the (wrongly 2-segment-terminated) call returned.
        let err = parse("select ext::pgcrypto::digest('encrypt this', 'sha1')").unwrap_err();
        assert!(err.to_string().contains("too many '::' segments"), "got: {err}");
    }

    #[test]
    fn test_nested_module_path_in_type_expr_is_a_clear_error() {
        let err = parse_expr("x is ext::pgcrypto::SomeType").unwrap_err();
        assert!(err.to_string().contains("too many '::' segments"), "got: {err}");
    }

    // ── Error message wording ───────────────────────────────────────────────
    //
    // Regression: `eat`/`eat_ident` used to Debug-format the `Token` enum
    // directly (`"expected RParen, got Eof"`, `"unexpected token LBrace"`),
    // leaking internal lexer variant names instead of the surface syntax a
    // user actually typed.

    #[test]
    fn test_unclosed_paren_names_the_missing_character_not_the_token_variant() {
        let err = parse("select (1 + 2").unwrap_err();
        assert_eq!(err.message, "expected ')', found end of input");
    }

    #[test]
    fn test_unclosed_brace_names_the_missing_character_not_the_token_variant() {
        let err = parse("select { 1").unwrap_err();
        assert_eq!(err.message, "expected '}', found end of input");
    }

    #[test]
    fn test_trailing_garbage_after_a_complete_statement_is_a_clear_error() {
        let err = parse("select 1 select 2").unwrap_err();
        assert_eq!(err.message, "unexpected 'select' after the end of the query");
    }

    #[test]
    fn test_missing_expression_names_the_offending_token_not_its_debug_form() {
        let err = parse_expr("1 +").unwrap_err();
        assert_eq!(err.message, "expected an expression, found end of input");
    }

    #[test]
    fn test_bad_statement_start_lists_keywords_in_surface_form() {
        let err = parse("123").unwrap_err();
        assert_eq!(
            err.message,
            "expected the start of a statement (with, for, select, insert, update, delete, or group), \
             found integer literal '123'"
        );
    }
}
