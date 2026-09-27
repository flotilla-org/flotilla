#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_ast;
extern crate rustc_errors;

use rustc_ast::{ast, token};
use rustc_errors::{Diag, DiagCtxtHandle};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};

dylint_linting::declare_early_lint! {
    /// Rejects direct Git process invocations outside the VCS implementation.
    /// Callers should use the checkout-scoped `Vcs` trait instead.
    pub RAW_GIT_OUTSIDE_VCS,
    Deny,
    "Git commands outside the VCS implementation must use the Vcs trait"
}

struct RawGitWarning;

impl<'a> rustc_errors::Diagnostic<'a, ()> for RawGitWarning {
    #[track_caller]
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: rustc_errors::Level) -> Diag<'a, ()> {
        Diag::new(dcx, level, "invoke Git through the checkout-scoped Vcs trait instead of a raw command")
    }
}

impl EarlyLintPass for RawGitOutsideVcs {
    fn check_expr(&mut self, cx: &EarlyContext<'_>, expr: &ast::Expr) {
        // Test fixtures create real repositories. Build scripts run before a
        // checkout-scoped command runner can exist.
        if cx.sess().opts.test || cx.sess().opts.crate_name.as_deref() == Some("build_script_build") {
            return;
        }

        let source = cx.sess().source_map();
        if expr.span.from_expansion() {
            if let ast::ExprKind::MethodCall(call) = &expr.kind {
                if matches!(call.seg.ident.name.as_str(), "run" | "run_output") {
                    let callsite = expr.span.source_callsite();
                    let file = source.span_to_filename(callsite).prefer_local_unconditionally().to_string();
                    if !is_exempt_source(&file) && source.span_to_snippet(callsite).is_ok_and(|snippet| is_raw_git_call(&snippet)) {
                        cx.emit_span_lint(RAW_GIT_OUTSIDE_VCS, callsite, RawGitWarning);
                    }
                }
            }
            return;
        }
        let file = source.span_to_filename(expr.span).prefer_local_unconditionally().to_string();
        if is_exempt_source(&file) {
            return;
        }

        if !is_raw_git_expr(expr) {
            return;
        }

        cx.emit_span_lint(RAW_GIT_OUTSIDE_VCS, expr.span, RawGitWarning);
    }
}

fn is_exempt_source(file: &str) -> bool {
    // `test_support` is also compiled in non-test builds when its feature is
    // enabled, but only creates fixture repositories.
    file.contains("/flotilla-core/src/providers/vcs/")
        || file.starts_with("crates/flotilla-core/src/providers/vcs/")
        || file.ends_with("/flotilla-core/src/vcs.rs")
        || file == "crates/flotilla-core/src/vcs.rs"
        || file.ends_with("/flotilla-core/src/providers/discovery/test_support.rs")
}

fn is_raw_git_expr(expr: &ast::Expr) -> bool {
    match &expr.kind {
        ast::ExprKind::MethodCall(call) => {
            matches!(call.seg.ident.name.as_str(), "run" | "run_output" | "run_with_input")
                && call.args.first().is_some_and(|arg| is_git_literal(arg))
        }
        ast::ExprKind::Call(callee, args) => {
            let ast::ExprKind::Path(_, path) = &callee.kind else {
                return false;
            };
            let segments = &path.segments;
            // Match an associated `new("git")` call regardless of how
            // `std::process::Command` was imported or renamed.
            segments.last().is_some_and(|segment| segment.ident.name.as_str() == "new")
                && segments.len() > 1
                && args.first().is_some_and(|arg| is_git_literal(arg))
        }
        _ => false,
    }
}

fn is_git_literal(expr: &ast::Expr) -> bool {
    matches!(&expr.kind, ast::ExprKind::Lit(lit) if matches!(lit.kind, token::LitKind::Str | token::LitKind::StrRaw(_)) && lit.symbol.as_str() == "git")
}

fn is_raw_git_call(snippet: &str) -> bool {
    // The macro has expanded before this lint runs. Inspect the original
    // invocation at the call site of its generated CommandRunner method call.
    let compact: String = snippet.chars().filter(|c| !c.is_whitespace()).collect();
    ["run!(", "run_output!("].iter().any(|marker| {
        complete_call_args(&compact, marker)
            .is_some_and(|args| args.split_once(',').is_some_and(|(_, command)| command.starts_with("\"git\",")))
    })
}

fn complete_call_args<'a>(snippet: &'a str, marker: &str) -> Option<&'a str> {
    let start = snippet.rfind(marker)? + marker.len();
    let mut depth = 1;
    let mut quoted = false;
    let mut escaped = false;
    for (offset, character) in snippet[start..].char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }
        match character {
            '"' => quoted = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (start + offset + 1 == snippet.len()).then_some(&snippet[start..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod raw_git_tests {
    use super::is_raw_git_call;

    #[test]
    fn recognizes_run_macro_invocations() {
        assert!(is_raw_git_call("run!(runner, \"git\", &args, cwd)"));
        assert!(is_raw_git_call("run!(runner,\n \"git\", &args, cwd)"));
        assert!(is_raw_git_call("run_output!(runner, \"git\", &args, cwd)"));
        assert!(!is_raw_git_call("run!(runner, \"gh\", &args, cwd)"));
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
