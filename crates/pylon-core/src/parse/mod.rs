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

#[cfg(test)]
mod tests {
    use super::*;
    use ast::*;

    #[test]
    fn test_select_bare_type() {
        let stmt = parse("SELECT Person").unwrap();
        assert!(matches!(
            stmt,
            Stmt::Select(SelectStmt { result: Expr::Path(_), .. })
        ));
    }

    #[test]
    fn test_select_with_shape() {
        let stmt = parse("SELECT Person { name, age }").unwrap();
        let Stmt::Select(sel) = stmt else { panic!("not a select") };
        let Expr::Shape(shape) = sel.result else { panic!("not a shape") };
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
        let Expr::BinOp(binop) = filter else { panic!("not a binop") };
        assert_eq!(binop.op, BinOpKind::Eq);
        assert!(matches!(binop.left, Expr::Path(Path { partial: true, .. })));
        assert!(matches!(binop.right, Expr::Parameter(_)));
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
    fn test_boolean_operators() {
        let stmt = parse(
            "SELECT Person FILTER .active = true AND .age >= 18 OR .admin = true",
        )
        .unwrap();
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
        let stmt =
            parse("INSERT Person { name := 'Alice', age := 30 }").unwrap();
        let Stmt::Insert(ins) = stmt else { panic!() };
        assert_eq!(ins.subject.name, "Person");
        assert_eq!(ins.shape.len(), 2);
    }

    #[test]
    fn test_update() {
        let stmt =
            parse("UPDATE Person FILTER .name = 'Alice' SET { age := 31 }").unwrap();
        assert!(matches!(stmt, Stmt::Update(_)));
    }

    #[test]
    fn test_delete() {
        let stmt = parse("DELETE Person FILTER .name = 'Alice'").unwrap();
        assert!(matches!(stmt, Stmt::Delete(_)));
    }
}
