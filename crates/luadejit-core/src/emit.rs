//! Source emission from a parsed [`Module`].
//!
//! Stage 7 rewrites this module to walk the structured AST produced
//! by [`crate::structure::recover`] rather than raw bytecode. The
//! public entry point [`emit_module`] is unchanged: callers still
//! hand it a parsed [`Module`] and get back a source string. The new
//! pipeline runs *inside* `emit_module`:
//!
//! 1. Build the [`Cfg`](crate::cfg::Cfg) for the main proto.
//! 2. [`structure::recover`] the AST (`Vec<Stmt>`).
//! 3. [`emit_ast`] formats the AST to a source string.
//!
//! Linear code (Stages 1-6 shapes) round-trips identically to the
//! old walk-based emitter; the new capability is `if/then`. Any
//! CFG shape the recovery doesn't model surfaces as
//! [`DecompilerError::NotImplemented`].
//!
//! ## Formatting invariants
//!
//! - Statements join with `\n`; no trailing newline.
//! - `Return(None)` (implicit return) emits no line — the source
//!   chunk just ends.
//! - `if/then` bodies indent 4 spaces per level.
//! - Number literals go through [`crate::number::format_lua_number`]
//!   so floats round-trip LuaJIT's `%.14g` formatting.
//! - String literals use Rust's `{:?}` (close enough to Lua's
//!   escaping for the common cases; full escape parity is a later
//!   stage).

use crate::cfg::Cfg;
use crate::ir::Module;
use crate::number::format_lua_number;
use crate::structure::{recover, BinOpKind, Expr, Stmt};
use crate::DecompilerError;

/// Number of spaces per indentation level. Matches LuaJIT's
/// `luajit -b` pretty-printer and the canonical Lua style.
const INDENT_SPACES: usize = 4;

/// Emit Lua source from a parsed module.
///
/// Builds the CFG for the main proto, recovers the AST, then
/// formats the AST to a source string. Returns
/// [`DecompilerError::NotImplemented`] for any input the recovery
/// doesn't model (if/else, loops, function calls, nested `if`,
/// compound conditions, etc.).
pub fn emit_module(module: &Module) -> Result<String, DecompilerError> {
    let main = module.main_proto();
    let cfg = Cfg::build(main);
    let ast = recover(main, &cfg)?;
    Ok(emit_ast(&ast))
}

/// Format a sequence of statements as Lua source. Statements are
/// joined by `\n`; `Return(None)` emits nothing. No trailing newline.
fn emit_ast(stmts: &[Stmt]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        if let Some(line) = format_stmt(stmt, 0) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Format one statement at the given indent level. Returns `None`
/// for `Return(None)` (implicit return — emits no line). `If` nodes
/// indent their body by one level; the `else_body` slot exists on
/// the variant for forward-compatibility but Stage 7 recovery never
/// produces one, so reaching a populated `else_body` here is a
/// logic bug.
fn format_stmt(stmt: &Stmt, indent: usize) -> Option<String> {
    let pad = " ".repeat(indent * INDENT_SPACES);
    match stmt {
        Stmt::LocalDecl { name, expr } => {
            Some(format!("{}local {} = {}", pad, name, format_expr(expr)))
        }
        Stmt::Assign { name, expr } => Some(format!("{}{} = {}", pad, name, format_expr(expr))),
        Stmt::Return(Some(expr)) => Some(format!("{}return {}", pad, format_expr(expr))),
        Stmt::Return(None) => None,
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            if else_body.is_some() {
                unreachable!("Stage 7 recovery never emits an else_body; if/else is Stage 8");
            }
            let mut out = format!("{}if {} then", pad, format_expr(cond));
            for inner in then_body {
                if let Some(line) = format_stmt(inner, indent + 1) {
                    out.push('\n');
                    out.push_str(&line);
                }
            }
            out.push('\n');
            out.push_str(&pad);
            out.push_str("end");
            Some(out)
        }
    }
}

/// Format an expression as Lua source.
///
/// Known limitation (carried over from Stage 4): nested arithmetic
/// is emitted without parenthesization. This works whenever Lua's
/// precedence matches the bytecode's evaluation order (the common
/// case) but produces incorrect output for cases where the bytecode
/// reorders against precedence, e.g. `(a + b) * c` would be emitted
/// as `a + b * c`. Correct parenthesization is deferred to a later
/// stage.
fn format_expr(expr: &Expr) -> String {
    match expr {
        // `Var` and `Global` both surface as bare names at the Lua
        // source level — a global is just an unqualified identifier
        // at the top of the chunk.
        Expr::Var(name) | Expr::Global(name) => name.clone(),
        Expr::Int(i) => format!("{}", i),
        Expr::Float(f) => format_lua_number(*f),
        Expr::Str(bytes) => {
            // Lossy UTF-8 conversion + Rust's `{:?}` — same Stage 2
            // behavior the walk-based emitter used for KSTR.
            let s = String::from_utf8_lossy(bytes);
            format!("{:?}", s)
        }
        Expr::Nil => "nil".to_string(),
        Expr::True => "true".to_string(),
        Expr::False => "false".to_string(),
        Expr::BinOp { op, left, right } => {
            format!(
                "{} {} {}",
                format_expr(left),
                binop_symbol(*op),
                format_expr(right)
            )
        }
        Expr::Not(inner) => format!("not {}", format_expr(inner)),
    }
}

/// Map a [`BinOpKind`] to its Lua source operator.
fn binop_symbol(op: BinOpKind) -> &'static str {
    match op {
        BinOpKind::Add => "+",
        BinOpKind::Sub => "-",
        BinOpKind::Mul => "*",
        BinOpKind::Div => "/",
        BinOpKind::Mod => "%",
        BinOpKind::Pow => "^",
        BinOpKind::Concat => "..",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure::{BinOpKind, Expr, Stmt};

    // ---- format_expr / format_stmt direct tests ------------------------
    //
    // These exercise the new emit-side formatting code in isolation
    // from the recovery pipeline (which has its own coverage in
    // `structure::tests`). Every variant of `Expr` / `Stmt` that
    // Stage 7 can produce gets at least one assertion here.

    #[test]
    fn formats_int() {
        assert_eq!(format_expr(&Expr::Int(5)), "5");
        assert_eq!(format_expr(&Expr::Int(-7)), "-7");
        assert_eq!(format_expr(&Expr::Int(0)), "0");
    }

    #[test]
    fn formats_float_through_lua_formatter() {
        // The pipeline routes Float through format_lua_number; the
        // 10.0/3.0 case would round differently under Rust's `{}`.
        assert_eq!(format_expr(&Expr::Float(10.0 / 3.0)), "3.3333333333333");
        // The fixture value 3.14 trips clippy::approx_constant (PI);
        // we intentionally use it here to mirror the `return_float`
        // integration fixture exactly.
        #[allow(clippy::approx_constant)]
        let pi_approx = 3.14_f64;
        assert_eq!(format_expr(&Expr::Float(pi_approx)), "3.14");
        // Integer-valued floats print without `.0`.
        assert_eq!(format_expr(&Expr::Float(3.0)), "3");
    }

    #[test]
    fn formats_str_with_rust_debug_escaping() {
        assert_eq!(format_expr(&Expr::Str(b"foo".to_vec())), "\"foo\"");
        // Bytes are lossy-converted to UTF-8 before `{:?}`. Invalid
        // UTF-8 becomes the replacement char; emit never panics.
        assert_eq!(
            format_expr(&Expr::Str(vec![0xff, 0xfe])),
            "\"\u{fffd}\u{fffd}\""
        );
    }

    #[test]
    fn formats_nil_true_false() {
        assert_eq!(format_expr(&Expr::Nil), "nil");
        assert_eq!(format_expr(&Expr::True), "true");
        assert_eq!(format_expr(&Expr::False), "false");
    }

    #[test]
    fn formats_var_and_global_identically() {
        // At the Lua source level both surface as bare names.
        assert_eq!(format_expr(&Expr::Var("x".to_string())), "x");
        assert_eq!(format_expr(&Expr::Global("x".to_string())), "x");
    }

    #[test]
    fn formats_binop_with_correct_symbol() {
        let left = Box::new(Expr::Var("a".to_string()));
        let right = Box::new(Expr::Int(3));
        for (op, sym) in [
            (BinOpKind::Add, "+"),
            (BinOpKind::Sub, "-"),
            (BinOpKind::Mul, "*"),
            (BinOpKind::Div, "/"),
            (BinOpKind::Mod, "%"),
            (BinOpKind::Pow, "^"),
            (BinOpKind::Concat, ".."),
        ] {
            let expr = Expr::BinOp {
                op,
                left: left.clone(),
                right: right.clone(),
            };
            assert_eq!(format_expr(&expr), format!("a {} 3", sym));
        }
    }

    #[test]
    fn formats_not() {
        assert_eq!(
            format_expr(&Expr::Not(Box::new(Expr::Global("x".to_string())))),
            "not x"
        );
    }

    #[test]
    fn formats_local_decl() {
        let stmt = Stmt::LocalDecl {
            name: "x".to_string(),
            expr: Expr::Int(5),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local x = 5");
    }

    #[test]
    fn formats_assign() {
        let stmt = Stmt::Assign {
            name: "x".to_string(),
            expr: Expr::Int(2),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "x = 2");
    }

    #[test]
    fn formats_return_some() {
        let stmt = Stmt::Return(Some(Expr::Int(1)));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return 1");
    }

    #[test]
    fn formats_return_none_emits_nothing() {
        let stmt = Stmt::Return(None);
        assert!(format_stmt(&stmt, 0).is_none());
    }

    #[test]
    fn formats_if_then_with_indented_body() {
        let stmt = Stmt::If {
            cond: Expr::Global("x".to_string()),
            then_body: vec![Stmt::Return(Some(Expr::Int(1)))],
            else_body: None,
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "if x then\n    return 1\nend"
        );
    }

    #[test]
    fn formats_if_then_at_nonzero_indent() {
        // Inside a then-body that itself contains an `if` (which
        // Stage 7 doesn't recover, but the formatter must still
        // handle the AST shape), the inner `if` indents further.
        let inner = Stmt::If {
            cond: Expr::Global("y".to_string()),
            then_body: vec![Stmt::Return(Some(Expr::Int(2)))],
            else_body: None,
        };
        assert_eq!(
            format_stmt(&inner, 1).unwrap(),
            "    if y then\n        return 2\n    end"
        );
    }

    #[test]
    fn emit_ast_joins_with_newline_no_trailing() {
        let stmts = vec![
            Stmt::LocalDecl {
                name: "x".to_string(),
                expr: Expr::Int(1),
            },
            Stmt::Return(Some(Expr::Var("x".to_string()))),
        ];
        assert_eq!(emit_ast(&stmts), "local x = 1\nreturn x");
    }

    #[test]
    fn emit_ast_skips_implicit_return() {
        // Return(None) emits no line, so a chunk that ends in an
        // implicit return joins to just the prior statements.
        let stmts = vec![
            Stmt::LocalDecl {
                name: "x".to_string(),
                expr: Expr::Int(5),
            },
            Stmt::Return(None),
        ];
        assert_eq!(emit_ast(&stmts), "local x = 5");
    }

    #[test]
    fn emit_ast_empty_returns_empty() {
        // A chunk that's just `return` (RET0 only) round-trips to
        // empty source — both the implicit Return(None) and the
        // empty AST produce the same output.
        assert_eq!(emit_ast(&[]), "");
        assert_eq!(emit_ast(&[Stmt::Return(None)]), "");
    }
}
