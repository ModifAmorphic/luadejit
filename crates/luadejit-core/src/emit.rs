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
//! old walk-based emitter; the new capabilities are `if/then` (Stage
//! 7), `if/else` (Stage 8), compound `and`/`or` conditions (Stage 9),
//! numeric-`for` loops (Stage 10), `while`/`repeat` loops (Stage 11),
//! regular function calls (Stage 12), field access + method
//! calls (Stage 13), table literals + table writes (Stage 14), and
//! function literals + length operator + global assignment (Stage 15).
//! Any CFG shape the recovery doesn't model surfaces as
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
use crate::structure::{recover, BinOpKind, Expr, Stmt, TableEntry};
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
    let main_idx = module.protos.len() - 1;
    let cfg = Cfg::build(&module.protos[main_idx]);
    let is_fr2 = module.header.is_fr2();
    let ast = recover(module, main_idx, &cfg, is_fr2)?;
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
/// indent their body by one level; an optional `else_body` is
/// rendered at the same indent as `if`/`end`, with its own
/// one-level-indented body.
fn format_stmt(stmt: &Stmt, indent: usize) -> Option<String> {
    let pad = " ".repeat(indent * INDENT_SPACES);
    match stmt {
        Stmt::LocalDecl { name, expr } => Some(format!(
            "{}local {} = {}",
            pad,
            name,
            format_expr(expr, indent)
        )),
        Stmt::Assign { name, expr } => {
            Some(format!("{}{} = {}", pad, name, format_expr(expr, indent)))
        }
        Stmt::Return(Some(expr)) => Some(format!("{}return {}", pad, format_expr(expr, indent))),
        Stmt::Return(None) => None,
        Stmt::Call { func, args } => Some(format!("{}{}", pad, format_call(func, args, indent))),
        Stmt::MethodCall { obj, method, args } => Some(format!(
            "{}{}",
            pad,
            format_method_call(obj, method, args, indent)
        )),
        Stmt::TableSet { target, value } => Some(format!(
            "{}{} = {}",
            pad,
            format_expr(target, indent),
            format_expr(value, indent)
        )),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            let mut out = format!("{}if {} then", pad, format_expr(cond, indent));
            for inner in then_body {
                if let Some(line) = format_stmt(inner, indent + 1) {
                    out.push('\n');
                    out.push_str(&line);
                }
            }
            if let Some(else_stmts) = else_body {
                out.push('\n');
                out.push_str(&pad);
                out.push_str("else");
                for inner in else_stmts {
                    if let Some(line) = format_stmt(inner, indent + 1) {
                        out.push('\n');
                        out.push_str(&line);
                    }
                }
            }
            out.push('\n');
            out.push_str(&pad);
            out.push_str("end");
            Some(out)
        }
        Stmt::For {
            var,
            start,
            stop,
            step,
            body,
        } => {
            // `for var = start, stop[, step] do` — omit the step
            // entirely when the recovery collapsed it to None
            // (default step of 1). Otherwise include it as a third
            // expression in the header.
            let header = match step {
                None => format!(
                    "{}for {} = {}, {} do",
                    pad,
                    var,
                    format_expr(start, indent),
                    format_expr(stop, indent)
                ),
                Some(s) => format!(
                    "{}for {} = {}, {}, {} do",
                    pad,
                    var,
                    format_expr(start, indent),
                    format_expr(stop, indent),
                    format_expr(s, indent)
                ),
            };
            let mut out = header;
            for inner in body {
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
        Stmt::While { cond, body } => {
            let mut out = format!("{}while {} do", pad, format_expr(cond, indent));
            for inner in body {
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
        Stmt::Repeat { cond, body } => {
            // `repeat\n    <body>\nuntil <cond>` — `repeat` has no
            // header expression; the condition trails the body.
            let mut out = format!("{}repeat", pad);
            for inner in body {
                if let Some(line) = format_stmt(inner, indent + 1) {
                    out.push('\n');
                    out.push_str(&line);
                }
            }
            out.push('\n');
            out.push_str(&pad);
            out.push_str("until ");
            out.push_str(&format_expr(cond, indent));
            Some(out)
        }
        Stmt::LocalDeclMulti { names, expr } => Some(format!(
            "{}local {} = {}",
            pad,
            names.join(", "),
            format_expr(expr, indent)
        )),
    }
}

/// Format an expression as Lua source.
///
/// `indent` is the current statement-level indent (in indent units,
/// each expanded to [`INDENT_SPACES`]). It is propagated to
/// sub-expressions unchanged except for [`Expr::Function`], whose
/// body statements render at `indent + 1` and whose closing `end`
/// renders at `indent` (matching `if`/`for`/`while` body
/// indentation).
///
/// Known limitation (carried over from Stage 4): nested arithmetic
/// is emitted without parenthesization. This works whenever Lua's
/// precedence matches the bytecode's evaluation order (the common
/// case) but produces incorrect output for cases where the bytecode
/// reorders against precedence, e.g. `(a + b) * c` would be emitted
/// as `a + b * c`. Correct parenthesization is deferred to a later
/// stage.
fn format_expr(expr: &Expr, indent: usize) -> String {
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
        Expr::BinOp { op, left, right } => format!(
            "{} {} {}",
            format_expr(left, indent),
            binop_symbol(*op),
            format_expr(right, indent)
        ),
        Expr::Not(inner) => format!("not {}", format_expr(inner, indent)),
        // Stage 9: logical connectives. No parenthesization — Lua's
        // precedence is comparisons > `and` > `or`, and the test
        // fixtures don't mix `and`/`or` in a single condition, so
        // naive concatenation round-trips the source. (Mixed
        // `and`/`or` chains will need a precedence-aware emitter
        // later.)
        Expr::And(left, right) => {
            format!(
                "{} and {}",
                format_expr(left, indent),
                format_expr(right, indent)
            )
        }
        Expr::Or(left, right) => {
            format!(
                "{} or {}",
                format_expr(left, indent),
                format_expr(right, indent)
            )
        }
        Expr::Call { func, args } => format_call(func, args, indent),
        Expr::Field { obj, name } => format!("{}.{}", format_expr(obj, indent), name),
        Expr::Index { obj, key } => {
            format!("{}[{}]", format_expr(obj, indent), format_expr(key, indent))
        }
        Expr::MethodCall { obj, method, args } => format_method_call(obj, method, args, indent),
        Expr::Table { entries } => format_table(entries, indent),
        Expr::Len(inner) => format!("#{}", format_expr(inner, indent)),
        Expr::Function { params, body } => format_function(params, body, indent),
        Expr::Vararg => "...".to_string(),
    }
}

/// Format a table literal's `{entry, entry, ...}` portion (without
/// leading indentation). Empty tables render as `{}`; non-empty
/// tables render as `{ a, b = 1, [k] = v }` with one comma-separated
/// entry per source position. Stage 14 does not pretty-print
/// multi-entry tables across multiple lines (one-line output matches
/// `luajit -bl`'s single-instruction expectation).
fn format_table(entries: &[TableEntry], indent: usize) -> String {
    if entries.is_empty() {
        return "{}".to_string();
    }
    let parts: Vec<String> = entries
        .iter()
        .map(|e| match e {
            TableEntry::Array(expr) => format_expr(expr, indent),
            TableEntry::HashStr(key, expr) => format!("{} = {}", key, format_expr(expr, indent)),
            TableEntry::HashExpr(key, expr) => {
                format!(
                    "[{}] = {}",
                    format_expr(key, indent),
                    format_expr(expr, indent)
                )
            }
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// Format a function call's `func(args...)` portion (without leading
/// indentation). Shared between [`Stmt::Call`] (a bare call
/// statement) and [`Expr::Call`] (a call nested inside another
/// expression).
///
/// `func` is formatted via [`format_expr`]; Stage 12 only emits
/// calls whose function is a bare global name, so no
/// parenthesization is needed. Future stages that allow calls on
/// non-atomic expressions (e.g. `(a or b).method()` — Stage 13's
/// method-call desugaring will cover this) will need to add
/// precedence-aware parenthesization here.
fn format_call(func: &Expr, args: &[Expr], indent: usize) -> String {
    let args_str = args
        .iter()
        .map(|a| format_expr(a, indent))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({})", format_expr(func, indent), args_str)
}

/// Format a method call's `obj:method(args...)` portion (without
/// leading indentation). Shared between [`Stmt::MethodCall`] (a
/// bare call statement) and [`Expr::MethodCall`] (a method call
/// nested inside another expression).
///
/// Like [`format_call`], no parenthesization of `obj` — Stage 13
/// fixtures use atomic receivers (globals / locals). Future stages
/// that allow non-atomic receivers (e.g. `(a or b):method()`) will
/// need precedence-aware parenthesization.
fn format_method_call(obj: &Expr, method: &str, args: &[Expr], indent: usize) -> String {
    let args_str = args
        .iter()
        .map(|a| format_expr(a, indent))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}:{}({})", format_expr(obj, indent), method, args_str)
}

/// Format a function literal as a multi-line expression. The
/// `function(params)` keyword line has no leading pad (the
/// surrounding statement supplies the prefix, e.g. `local f = `).
/// Each body statement renders at `indent + 1`; the closing `end`
/// renders at `indent`. An empty body collapses to
/// `function(params)\nend`.
///
/// Example at `indent = 0`:
/// ```text
/// function(x)
///     return x
/// end
/// ```
fn format_function(params: &[String], body: &[Stmt], indent: usize) -> String {
    let params_str = params.join(", ");
    let mut out = format!("function({})", params_str);
    for inner in body {
        if let Some(line) = format_stmt(inner, indent + 1) {
            out.push('\n');
            out.push_str(&line);
        }
    }
    out.push('\n');
    out.push_str(&" ".repeat(indent * INDENT_SPACES));
    out.push_str("end");
    out
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
        // Stage 9: comparison operators lowered from ISxx.
        BinOpKind::Equal => "==",
        BinOpKind::NotEqual => "~=",
        BinOpKind::LessThan => "<",
        BinOpKind::GreaterThan => ">",
        BinOpKind::LessEqual => "<=",
        BinOpKind::GreaterEqual => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure::{BinOpKind, Expr, Stmt, TableEntry};

    // ---- format_expr / format_stmt direct tests ------------------------
    //
    // These exercise the new emit-side formatting code in isolation
    // from the recovery pipeline (which has its own coverage in
    // `structure::tests`). Every variant of `Expr` / `Stmt` that
    // Stage 7 can produce gets at least one assertion here.

    #[test]
    fn formats_int() {
        assert_eq!(format_expr(&Expr::Int(5), 0), "5");
        assert_eq!(format_expr(&Expr::Int(-7), 0), "-7");
        assert_eq!(format_expr(&Expr::Int(0), 0), "0");
    }

    #[test]
    fn formats_float_through_lua_formatter() {
        // The pipeline routes Float through format_lua_number; the
        // 10.0/3.0 case would round differently under Rust's `{}`.
        assert_eq!(format_expr(&Expr::Float(10.0 / 3.0), 0), "3.3333333333333");
        // The fixture value 3.14 trips clippy::approx_constant (PI);
        // we intentionally use it here to mirror the `return_float`
        // integration fixture exactly.
        #[allow(clippy::approx_constant)]
        let pi_approx = 3.14_f64;
        assert_eq!(format_expr(&Expr::Float(pi_approx), 0), "3.14");
        // Integer-valued floats print without `.0`.
        assert_eq!(format_expr(&Expr::Float(3.0), 0), "3");
    }

    #[test]
    fn formats_str_with_rust_debug_escaping() {
        assert_eq!(format_expr(&Expr::Str(b"foo".to_vec()), 0), "\"foo\"");
        // Bytes are lossy-converted to UTF-8 before `{:?}`. Invalid
        // UTF-8 becomes the replacement char; emit never panics.
        assert_eq!(
            format_expr(&Expr::Str(vec![0xff, 0xfe]), 0),
            "\"\u{fffd}\u{fffd}\""
        );
    }

    #[test]
    fn formats_nil_true_false() {
        assert_eq!(format_expr(&Expr::Nil, 0), "nil");
        assert_eq!(format_expr(&Expr::True, 0), "true");
        assert_eq!(format_expr(&Expr::False, 0), "false");
    }

    #[test]
    fn formats_var_and_global_identically() {
        // At the Lua source level both surface as bare names.
        assert_eq!(format_expr(&Expr::Var("x".to_string()), 0), "x");
        assert_eq!(format_expr(&Expr::Global("x".to_string()), 0), "x");
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
            // Stage 9 comparison operators.
            (BinOpKind::Equal, "=="),
            (BinOpKind::NotEqual, "~="),
            (BinOpKind::LessThan, "<"),
            (BinOpKind::GreaterThan, ">"),
            (BinOpKind::LessEqual, "<="),
            (BinOpKind::GreaterEqual, ">="),
        ] {
            let expr = Expr::BinOp {
                op,
                left: left.clone(),
                right: right.clone(),
            };
            assert_eq!(format_expr(&expr, 0), format!("a {} 3", sym));
        }
    }

    #[test]
    fn formats_not() {
        assert_eq!(
            format_expr(&Expr::Not(Box::new(Expr::Global("x".to_string()))), 0),
            "not x"
        );
    }

    #[test]
    fn formats_and_chain() {
        // Stage 9: `a and b` — naive concatenation, no parens.
        let expr = Expr::And(
            Box::new(Expr::Global("a".to_string())),
            Box::new(Expr::Global("b".to_string())),
        );
        assert_eq!(format_expr(&expr, 0), "a and b");
    }

    #[test]
    fn formats_or_chain() {
        // Stage 9: `a or b` — naive concatenation, no parens.
        let expr = Expr::Or(
            Box::new(Expr::Global("a".to_string())),
            Box::new(Expr::Global("b".to_string())),
        );
        assert_eq!(format_expr(&expr, 0), "a or b");
    }

    // ---- Stage 12: function-call formatting ----------------------------

    #[test]
    fn formats_call_no_args() {
        // `f()` — bare global, empty arg list.
        let expr = Expr::Call {
            func: Box::new(Expr::Global("f".to_string())),
            args: vec![],
        };
        assert_eq!(format_expr(&expr, 0), "f()");
    }

    #[test]
    fn formats_call_one_arg() {
        // `print("hello")` — single string arg.
        let expr = Expr::Call {
            func: Box::new(Expr::Global("print".to_string())),
            args: vec![Expr::Str(b"hello".to_vec())],
        };
        assert_eq!(format_expr(&expr, 0), "print(\"hello\")");
    }

    #[test]
    fn formats_call_multiple_args() {
        // `print("a", "b", "c")` — comma-separated args.
        let expr = Expr::Call {
            func: Box::new(Expr::Global("print".to_string())),
            args: vec![
                Expr::Str(b"a".to_vec()),
                Expr::Str(b"b".to_vec()),
                Expr::Str(b"c".to_vec()),
            ],
        };
        assert_eq!(format_expr(&expr, 0), "print(\"a\", \"b\", \"c\")");
    }

    #[test]
    fn formats_call_with_mixed_arg_types() {
        // `f(1, "x", nil, true)` — int / str / nil / bool args.
        let expr = Expr::Call {
            func: Box::new(Expr::Global("f".to_string())),
            args: vec![
                Expr::Int(1),
                Expr::Str(b"x".to_vec()),
                Expr::Nil,
                Expr::True,
            ],
        };
        assert_eq!(format_expr(&expr, 0), "f(1, \"x\", nil, true)");
    }

    #[test]
    fn formats_call_with_var_arg() {
        // `f(x)` — Var arg (the local/global reads back through
        // format_expr's Var arm).
        let expr = Expr::Call {
            func: Box::new(Expr::Global("f".to_string())),
            args: vec![Expr::Var("x".to_string())],
        };
        assert_eq!(format_expr(&expr, 0), "f(x)");
    }

    #[test]
    fn formats_call_statement_at_zero_indent() {
        // Bare call statement: `print("hello")` at the top level.
        let stmt = Stmt::Call {
            func: Expr::Global("print".to_string()),
            args: vec![Expr::Str(b"hello".to_vec())],
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "print(\"hello\")");
    }

    #[test]
    fn formats_call_statement_at_nonzero_indent() {
        // Inside a loop body (which Stage 12 doesn't decompile, but
        // the formatter must still handle the AST shape), a bare call
        // indents with the surrounding block.
        let stmt = Stmt::Call {
            func: Expr::Global("print".to_string()),
            args: vec![Expr::Int(42)],
        };
        assert_eq!(format_stmt(&stmt, 1).unwrap(), "    print(42)");
    }

    #[test]
    fn formats_local_decl_with_call_rhs() {
        // `local x = f(1)` — Call as the RHS of a LocalDecl.
        let stmt = Stmt::LocalDecl {
            name: "x".to_string(),
            expr: Expr::Call {
                func: Box::new(Expr::Global("f".to_string())),
                args: vec![Expr::Int(1)],
            },
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local x = f(1)");
    }

    #[test]
    fn formats_return_with_call_expr() {
        // `return f(1)` — Call as the returned expression.
        let stmt = Stmt::Return(Some(Expr::Call {
            func: Box::new(Expr::Global("f".to_string())),
            args: vec![Expr::Int(1)],
        }));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return f(1)");
    }

    // ---- Stage 13: field-access / index / method-call formatting -----

    #[test]
    fn formats_field_access() {
        // `obj.field` — Field expr.
        let expr = Expr::Field {
            obj: Box::new(Expr::Global("obj".to_string())),
            name: "field".to_string(),
        };
        assert_eq!(format_expr(&expr, 0), "obj.field");
    }

    #[test]
    fn formats_field_access_on_var() {
        // `x.field` — Field on a local variable.
        let expr = Expr::Field {
            obj: Box::new(Expr::Var("x".to_string())),
            name: "field".to_string(),
        };
        assert_eq!(format_expr(&expr, 0), "x.field");
    }

    #[test]
    fn formats_index_with_int_key() {
        // `obj[5]` — Index with integer key.
        let expr = Expr::Index {
            obj: Box::new(Expr::Global("obj".to_string())),
            key: Box::new(Expr::Int(5)),
        };
        assert_eq!(format_expr(&expr, 0), "obj[5]");
    }

    #[test]
    fn formats_index_with_var_key() {
        // `t[k]` — Index with variable key.
        let expr = Expr::Index {
            obj: Box::new(Expr::Var("t".to_string())),
            key: Box::new(Expr::Var("k".to_string())),
        };
        assert_eq!(format_expr(&expr, 0), "t[k]");
    }

    #[test]
    fn formats_method_call_no_args() {
        // `obj:method()` — method call with empty arg list.
        let expr = Expr::MethodCall {
            obj: Box::new(Expr::Global("obj".to_string())),
            method: "method".to_string(),
            args: vec![],
        };
        assert_eq!(format_expr(&expr, 0), "obj:method()");
    }

    #[test]
    fn formats_method_call_with_args() {
        // `obj:method(1, "x")` — method call with mixed-type args.
        let expr = Expr::MethodCall {
            obj: Box::new(Expr::Global("obj".to_string())),
            method: "method".to_string(),
            args: vec![Expr::Int(1), Expr::Str(b"x".to_vec())],
        };
        assert_eq!(format_expr(&expr, 0), "obj:method(1, \"x\")");
    }

    #[test]
    fn formats_method_call_statement_at_zero_indent() {
        // `obj:method(1)` — bare method call at top level.
        let stmt = Stmt::MethodCall {
            obj: Expr::Global("obj".to_string()),
            method: "method".to_string(),
            args: vec![Expr::Int(1)],
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "obj:method(1)");
    }

    #[test]
    fn formats_method_call_statement_at_nonzero_indent() {
        // Bare method call inside an indented block.
        let stmt = Stmt::MethodCall {
            obj: Expr::Var("self".to_string()),
            method: "update".to_string(),
            args: vec![Expr::Float(1.5)],
        };
        assert_eq!(format_stmt(&stmt, 1).unwrap(), "    self:update(1.5)");
    }

    #[test]
    fn formats_local_decl_with_method_call_rhs() {
        // `local x = obj:method()` — MethodCall as RHS of LocalDecl.
        let stmt = Stmt::LocalDecl {
            name: "x".to_string(),
            expr: Expr::MethodCall {
                obj: Box::new(Expr::Global("obj".to_string())),
                method: "method".to_string(),
                args: vec![],
            },
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local x = obj:method()");
    }

    #[test]
    fn formats_return_with_method_call_expr() {
        // `return obj:method()` — MethodCall as returned expr.
        let stmt = Stmt::Return(Some(Expr::MethodCall {
            obj: Box::new(Expr::Global("obj".to_string())),
            method: "method".to_string(),
            args: vec![Expr::Int(1)],
        }));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return obj:method(1)");
    }

    #[test]
    fn formats_nested_field_access_no_parens() {
        // `a.b.c` — chained Field access (Stage 13 doesn't yet
        // recover this, but the formatter handles the AST shape).
        let expr = Expr::Field {
            obj: Box::new(Expr::Field {
                obj: Box::new(Expr::Global("a".to_string())),
                name: "b".to_string(),
            }),
            name: "c".to_string(),
        };
        assert_eq!(format_expr(&expr, 0), "a.b.c");
    }

    #[test]
    fn formats_nested_and_or_no_parens() {
        // Stage 9 limitation: nested `and`/`or` emits without
        // parenthesization. `(a and b) or c` would round-trip
        // incorrectly as `a and b or c`, which Lua parses as
        // `a and b or c` (= `(a and b) or c` by precedence —
        // happens to match here, but the formatter doesn't track
        // precedence). The emit-side test just pins the current
        // behavior; precedence-aware formatting is a later stage.
        let expr = Expr::Or(
            Box::new(Expr::And(
                Box::new(Expr::Global("a".to_string())),
                Box::new(Expr::Global("b".to_string())),
            )),
            Box::new(Expr::Global("c".to_string())),
        );
        assert_eq!(format_expr(&expr, 0), "a and b or c");
    }

    // ---- Stage 14: table literal / table-write formatting ----------

    #[test]
    fn formats_empty_table() {
        // `{}` — TNEW with no entries.
        let expr = Expr::Table { entries: vec![] };
        assert_eq!(format_expr(&expr, 0), "{}");
    }

    #[test]
    fn formats_array_only_table() {
        // `{1, 2, 3}` — positional array entries.
        let expr = Expr::Table {
            entries: vec![
                TableEntry::Array(Expr::Int(1)),
                TableEntry::Array(Expr::Int(2)),
                TableEntry::Array(Expr::Int(3)),
            ],
        };
        assert_eq!(format_expr(&expr, 0), "{1, 2, 3}");
    }

    #[test]
    fn formats_hash_only_table() {
        // `{a = 1, b = 2}` — string-keyed hash entries.
        let expr = Expr::Table {
            entries: vec![
                TableEntry::HashStr("a".to_string(), Expr::Int(1)),
                TableEntry::HashStr("b".to_string(), Expr::Int(2)),
            ],
        };
        assert_eq!(format_expr(&expr, 0), "{a = 1, b = 2}");
    }

    #[test]
    fn formats_mixed_table() {
        // `{1, 2, x = 3}` — array entries first, hash entries after.
        let expr = Expr::Table {
            entries: vec![
                TableEntry::Array(Expr::Int(1)),
                TableEntry::Array(Expr::Int(2)),
                TableEntry::HashStr("x".to_string(), Expr::Int(3)),
            ],
        };
        assert_eq!(format_expr(&expr, 0), "{1, 2, x = 3}");
    }

    #[test]
    fn formats_hash_expr_table() {
        // `{[1] = "v"}` — non-string key needs bracket syntax.
        let expr = Expr::Table {
            entries: vec![TableEntry::HashExpr(Expr::Int(1), Expr::Str(b"v".to_vec()))],
        };
        assert_eq!(format_expr(&expr, 0), "{[1] = \"v\"}");
    }

    #[test]
    fn formats_table_with_mixed_value_types() {
        // `{true, nil, 1.5, "s"}` — bool/nil/num/str entries.
        let expr = Expr::Table {
            entries: vec![
                TableEntry::Array(Expr::True),
                TableEntry::Array(Expr::Nil),
                TableEntry::Array(Expr::Float(1.5)),
                TableEntry::Array(Expr::Str(b"s".to_vec())),
            ],
        };
        assert_eq!(format_expr(&expr, 0), "{true, nil, 1.5, \"s\"}");
    }

    #[test]
    fn formats_table_set_field_target() {
        // `t.x = 1` — TableSet with a Field target (TSETS).
        let stmt = Stmt::TableSet {
            target: Expr::Field {
                obj: Box::new(Expr::Var("t".to_string())),
                name: "x".to_string(),
            },
            value: Expr::Int(1),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "t.x = 1");
    }

    #[test]
    fn formats_table_set_index_int_key_target() {
        // `t[0] = 42` — TableSet with an Index target (TSETB).
        let stmt = Stmt::TableSet {
            target: Expr::Index {
                obj: Box::new(Expr::Var("t".to_string())),
                key: Box::new(Expr::Int(0)),
            },
            value: Expr::Int(42),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "t[0] = 42");
    }

    #[test]
    fn formats_table_set_index_var_key_target() {
        // `t[k] = v` — TableSet with an Index target (TSETV).
        let stmt = Stmt::TableSet {
            target: Expr::Index {
                obj: Box::new(Expr::Var("t".to_string())),
                key: Box::new(Expr::Var("k".to_string())),
            },
            value: Expr::Var("v".to_string()),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "t[k] = v");
    }

    #[test]
    fn formats_table_set_at_nonzero_indent() {
        // Stage 14 doesn't recover table writes inside an indented
        // block (would need a loop body), but the formatter must
        // still handle the AST shape.
        let stmt = Stmt::TableSet {
            target: Expr::Field {
                obj: Box::new(Expr::Var("t".to_string())),
                name: "x".to_string(),
            },
            value: Expr::Int(1),
        };
        assert_eq!(format_stmt(&stmt, 1).unwrap(), "    t.x = 1");
    }

    #[test]
    fn formats_local_decl_with_table_rhs() {
        // `local t = {}` — Table as RHS of a LocalDecl.
        let stmt = Stmt::LocalDecl {
            name: "t".to_string(),
            expr: Expr::Table { entries: vec![] },
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local t = {}");
    }

    #[test]
    fn formats_return_with_table_expr() {
        // `return {1, 2}` — Table as the returned expression.
        let stmt = Stmt::Return(Some(Expr::Table {
            entries: vec![
                TableEntry::Array(Expr::Int(1)),
                TableEntry::Array(Expr::Int(2)),
            ],
        }));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return {1, 2}");
    }

    // ---- Stage 15: function-literal / length-op formatting ----------

    #[test]
    fn formats_len_on_global() {
        // `#t` — Len on a global.
        let expr = Expr::Len(Box::new(Expr::Global("t".to_string())));
        assert_eq!(format_expr(&expr, 0), "#t");
    }

    #[test]
    fn formats_len_on_var() {
        // `#x` — Len on a local variable.
        let expr = Expr::Len(Box::new(Expr::Var("x".to_string())));
        assert_eq!(format_expr(&expr, 0), "#x");
    }

    #[test]
    fn formats_len_on_field() {
        // `#t.n` — Len on a field access (nested expr).
        let expr = Expr::Len(Box::new(Expr::Field {
            obj: Box::new(Expr::Global("t".to_string())),
            name: "n".to_string(),
        }));
        assert_eq!(format_expr(&expr, 0), "#t.n");
    }

    #[test]
    fn formats_local_decl_with_len_rhs() {
        // `local x = #t` — Len as RHS of a LocalDecl.
        let stmt = Stmt::LocalDecl {
            name: "x".to_string(),
            expr: Expr::Len(Box::new(Expr::Global("t".to_string()))),
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local x = #t");
    }

    #[test]
    fn formats_function_no_params_empty_body() {
        // `function()` with empty body — `function()\nend`.
        let expr = Expr::Function {
            params: vec![],
            body: vec![],
        };
        assert_eq!(format_expr(&expr, 0), "function()\nend");
    }

    #[test]
    fn formats_function_with_params_and_body() {
        // `function(x, y)\n    return x + y\nend` at indent 0.
        let expr = Expr::Function {
            params: vec!["x".to_string(), "y".to_string()],
            body: vec![Stmt::Return(Some(Expr::BinOp {
                op: BinOpKind::Add,
                left: Box::new(Expr::Var("x".to_string())),
                right: Box::new(Expr::Var("y".to_string())),
            }))],
        };
        assert_eq!(
            format_expr(&expr, 0),
            "function(x, y)\n    return x + y\nend"
        );
    }

    #[test]
    fn formats_function_at_nonzero_indent() {
        // Inside an indented block (e.g. a loop body), the function's
        // body tracks the surrounding indent and `end` lines up.
        let expr = Expr::Function {
            params: vec!["n".to_string()],
            body: vec![Stmt::Return(Some(Expr::Var("n".to_string())))],
        };
        assert_eq!(
            format_expr(&expr, 1),
            "function(n)\n        return n\n    end"
        );
    }

    #[test]
    fn formats_local_decl_with_function_rhs() {
        // `local f = function()\n    return 1\nend` — the complete
        // FNEW fixture shape.
        let stmt = Stmt::LocalDecl {
            name: "f".to_string(),
            expr: Expr::Function {
                params: vec![],
                body: vec![Stmt::Return(Some(Expr::Int(1)))],
            },
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "local f = function()\n    return 1\nend"
        );
    }

    #[test]
    fn formats_assign_with_function_rhs() {
        // `g = function(x)\n    return x\nend` — Assign variant
        // (GSET doesn't produce this directly, but a reassigned
        // global/local could).
        let stmt = Stmt::Assign {
            name: "g".to_string(),
            expr: Expr::Function {
                params: vec!["x".to_string()],
                body: vec![Stmt::Return(Some(Expr::Var("x".to_string())))],
            },
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "g = function(x)\n    return x\nend"
        );
    }

    #[test]
    fn formats_return_with_function_expr() {
        // `return function() end` — Function as the returned expr.
        let stmt = Stmt::Return(Some(Expr::Function {
            params: vec![],
            body: vec![],
        }));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return function()\nend");
    }

    // ---- Stage 16: multres (LocalDeclMulti, Vararg) formatting ------

    #[test]
    fn formats_vararg_expr() {
        // `...` — bare Vararg expression.
        assert_eq!(format_expr(&Expr::Vararg, 0), "...");
    }

    #[test]
    fn formats_local_decl_multi() {
        // `local a, b, c = f()` — multi-name local declaration.
        let stmt = Stmt::LocalDeclMulti {
            names: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            expr: Expr::Call {
                func: Box::new(Expr::Global("f".to_string())),
                args: vec![],
            },
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local a, b, c = f()");
    }

    #[test]
    fn formats_local_decl_multi_two_names() {
        // `local x, y = f()` — two-name variant.
        let stmt = Stmt::LocalDeclMulti {
            names: vec!["x".to_string(), "y".to_string()],
            expr: Expr::Call {
                func: Box::new(Expr::Global("g".to_string())),
                args: vec![Expr::Int(1)],
            },
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "local x, y = g(1)");
    }

    #[test]
    fn formats_local_decl_multi_at_nonzero_indent() {
        // Inside an indented block (Stage 16 doesn't recover this
        // shape nested, but the formatter must still handle it).
        let stmt = Stmt::LocalDeclMulti {
            names: vec!["a".to_string(), "b".to_string()],
            expr: Expr::Vararg,
        };
        assert_eq!(format_stmt(&stmt, 1).unwrap(), "    local a, b = ...");
    }

    #[test]
    fn formats_return_with_vararg_expr() {
        // `return ...` — Vararg as the returned expr (the shape
        // Stage 16's RETM handler emits).
        let stmt = Stmt::Return(Some(Expr::Vararg));
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "return ...");
    }

    #[test]
    fn formats_call_with_vararg_arg() {
        // `f(...)` — Vararg as a call argument (CALLM with VARG).
        let expr = Expr::Call {
            func: Box::new(Expr::Global("f".to_string())),
            args: vec![Expr::Vararg],
        };
        assert_eq!(format_expr(&expr, 0), "f(...)");
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
    fn formats_if_else_with_both_bodies_indented() {
        // Stage 8: populated else_body renders at the same indent as
        // `if`/`end`, with the else-body indented one level (matching
        // the then-body).
        let stmt = Stmt::If {
            cond: Expr::Global("x".to_string()),
            then_body: vec![Stmt::Return(Some(Expr::Int(1)))],
            else_body: Some(vec![Stmt::Return(Some(Expr::Int(2)))]),
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "if x then\n    return 1\nelse\n    return 2\nend"
        );
    }

    #[test]
    fn formats_if_else_at_nonzero_indent() {
        // Inside a nested construct (which Stage 8 doesn't recover,
        // but the formatter must still handle the AST shape), the
        // `else` line tracks the outer indent.
        let stmt = Stmt::If {
            cond: Expr::Global("y".to_string()),
            then_body: vec![Stmt::Return(Some(Expr::Int(1)))],
            else_body: Some(vec![Stmt::Return(Some(Expr::Int(2)))]),
        };
        assert_eq!(
            format_stmt(&stmt, 1).unwrap(),
            "    if y then\n        return 1\n    else\n        return 2\n    end"
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

    // ---- Stage 10: numeric-for formatting ------------------------------

    #[test]
    fn formats_for_without_step() {
        // step == None → omit the third expression in the header.
        let stmt = Stmt::For {
            var: "i".to_string(),
            start: Expr::Int(1),
            stop: Expr::Int(10),
            step: None,
            body: vec![Stmt::LocalDecl {
                name: "x".to_string(),
                expr: Expr::Var("i".to_string()),
            }],
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "for i = 1, 10 do\n    local x = i\nend"
        );
    }

    #[test]
    fn formats_for_with_step() {
        // step == Some(Int(2)) → include as third header expression.
        let stmt = Stmt::For {
            var: "i".to_string(),
            start: Expr::Int(1),
            stop: Expr::Int(10),
            step: Some(Expr::Int(2)),
            body: vec![Stmt::LocalDecl {
                name: "x".to_string(),
                expr: Expr::Var("i".to_string()),
            }],
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "for i = 1, 10, 2 do\n    local x = i\nend"
        );
    }

    #[test]
    fn formats_for_at_nonzero_indent() {
        // Stage 10 doesn't recover nested loops, but the formatter
        // must still handle the AST shape if it ever appears.
        let stmt = Stmt::For {
            var: "i".to_string(),
            start: Expr::Int(1),
            stop: Expr::Int(3),
            step: None,
            body: vec![Stmt::Return(Some(Expr::Var("i".to_string())))],
        };
        assert_eq!(
            format_stmt(&stmt, 1).unwrap(),
            "    for i = 1, 3 do\n        return i\n    end"
        );
    }

    // ---- Stage 11: while/repeat formatting ----------------------------

    #[test]
    fn formats_while_with_indented_body() {
        let stmt = Stmt::While {
            cond: Expr::BinOp {
                op: BinOpKind::LessThan,
                left: Box::new(Expr::Var("i".to_string())),
                right: Box::new(Expr::Int(10)),
            },
            body: vec![Stmt::Assign {
                name: "i".to_string(),
                expr: Expr::BinOp {
                    op: BinOpKind::Add,
                    left: Box::new(Expr::Var("i".to_string())),
                    right: Box::new(Expr::Int(1)),
                },
            }],
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "while i < 10 do\n    i = i + 1\nend"
        );
    }

    #[test]
    fn formats_while_empty_body() {
        // `while cond do end` — empty body produces just the
        // header + `end`.
        let stmt = Stmt::While {
            cond: Expr::Global("x".to_string()),
            body: vec![],
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "while x do\nend");
    }

    #[test]
    fn formats_while_at_nonzero_indent() {
        // Stage 11 doesn't recover nested loops, but the formatter
        // must still handle the AST shape if it ever appears.
        let stmt = Stmt::While {
            cond: Expr::Global("x".to_string()),
            body: vec![Stmt::Return(Some(Expr::Int(1)))],
        };
        assert_eq!(
            format_stmt(&stmt, 1).unwrap(),
            "    while x do\n        return 1\n    end"
        );
    }

    #[test]
    fn formats_repeat_with_indented_body() {
        let stmt = Stmt::Repeat {
            // Canonicalized form of `i >= 3` — matches what the
            // recovery actually produces.
            cond: Expr::BinOp {
                op: BinOpKind::LessEqual,
                left: Box::new(Expr::Int(3)),
                right: Box::new(Expr::Var("i".to_string())),
            },
            body: vec![Stmt::Assign {
                name: "i".to_string(),
                expr: Expr::BinOp {
                    op: BinOpKind::Add,
                    left: Box::new(Expr::Var("i".to_string())),
                    right: Box::new(Expr::Int(1)),
                },
            }],
        };
        assert_eq!(
            format_stmt(&stmt, 0).unwrap(),
            "repeat\n    i = i + 1\nuntil 3 <= i"
        );
    }

    #[test]
    fn formats_repeat_empty_body() {
        // `repeat until cond` — empty body produces just `repeat`
        // + `until cond`.
        let stmt = Stmt::Repeat {
            cond: Expr::Global("x".to_string()),
            body: vec![],
        };
        assert_eq!(format_stmt(&stmt, 0).unwrap(), "repeat\nuntil x");
    }

    #[test]
    fn formats_repeat_at_nonzero_indent() {
        let stmt = Stmt::Repeat {
            cond: Expr::Global("x".to_string()),
            body: vec![Stmt::Return(Some(Expr::Int(1)))],
        };
        assert_eq!(
            format_stmt(&stmt, 1).unwrap(),
            "    repeat\n        return 1\n    until x"
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
