#![feature(rustc_private)]
#![warn(unused_extern_crates)]

extern crate rustc_ast;
extern crate rustc_errors;
extern crate rustc_span;

use rustc_ast::ast::{self, ItemKind};
use rustc_ast::visit::{self, Visitor};
use rustc_errors::{Diag, DiagCtxtHandle};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};

dylint_linting::declare_early_lint! {
    /// ### What it does
    /// Warns when code uses long inline `crate::`, `self::`, or `super::` paths
    /// instead of importing the item with `use`.
    ///
    /// ### Configuration
    /// In `dylint.toml` at the workspace root:
    /// ```toml
    /// max_inline_segments = 3   # default; paths with more segments warn
    /// ```
    ///
    /// A path like `crate::a::b::c` has 4 segments (including `crate`).
    /// With `max_inline_segments = 3`, that path triggers because 4 > 3.
    ///
    /// ### Example
    /// ```rust,ignore
    /// let x = crate::providers::types::CorrelationKey::Branch(b);
    /// ```
    /// Use instead:
    /// ```rust,ignore
    /// use crate::providers::types::CorrelationKey;
    /// let x = CorrelationKey::Branch(b);
    /// ```
    pub LONG_INLINE_PATHS,
    Warn,
    "long inline crate::/self::/super:: paths should use imports"
}

const DEFAULT_MAX_SEGMENTS: usize = 3;

/// Read `max_inline_segments` from `dylint.toml`, defaulting to 3.
/// Paths with more total segments (including the prefix keyword) warn.
fn max_segments() -> usize {
    let v = dylint_linting::config_or_default::<usize>("max_inline_segments");
    if v == 0 { DEFAULT_MAX_SEGMENTS } else { v }
}

/// Diagnostic emitted by the lint.
struct LongPathWarning {
    message: String,
}

impl<'a> rustc_errors::Diagnostic<'a, ()> for LongPathWarning {
    #[track_caller]
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: rustc_errors::Level) -> Diag<'a, ()> {
        Diag::new(dcx, level, self.message)
    }
}

impl EarlyLintPass for LongInlinePaths {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &ast::Item) {
        if matches!(item.kind, ItemKind::Use(..)) {
            return;
        }
        let mut finder = PathFinder { cx, max: max_segments() };
        visit::walk_item(&mut finder, item);
    }
}

struct PathFinder<'a, 'b> {
    cx: &'a EarlyContext<'b>,
    max: usize,
}

impl PathFinder<'_, '_> {
    fn check_path(&self, path: &ast::Path) {
        if path.segments.len() <= self.max {
            return;
        }

        let first = &path.segments[0].ident.name;
        let is_qualifying = *first == rustc_span::symbol::kw::Crate
            || *first == rustc_span::symbol::kw::SelfLower
            || *first == rustc_span::symbol::kw::Super;

        if !is_qualifying {
            return;
        }

        // Skip paths from macro expansions.
        if path.span.from_expansion() || path.span.in_derive_expansion() {
            return;
        }

        let path_str: String = path.segments.iter().map(|s| s.ident.to_string()).collect::<Vec<_>>().join("::");

        // Skip proc-macro-generated paths whose span text doesn't match the AST path
        // (e.g. serde derive generates `mod::serialize` with span on `"mod"`).
        // Also skip if snippet lookup fails (synthetic/dummy spans).
        match self.cx.sess().source_map().span_to_snippet(path.span) {
            Ok(snippet) if snippet == path_str => {} // real source path; proceed
            _ => return,                             // synthetic, dummy, or mismatch; skip
        }

        self.cx.emit_span_lint(
            LONG_INLINE_PATHS,
            path.span,
            LongPathWarning {
                message: format!(
                    "long inline path `{path_str}` ({} segments, max {}); prefer a `use` import",
                    path.segments.len(),
                    self.max,
                ),
            },
        );
    }
}

impl<'ast> Visitor<'ast> for PathFinder<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::ExprKind::Path(_, ref path) = expr.kind {
            self.check_path(path);
        }
        visit::walk_expr(self, expr);
    }

    fn visit_ty(&mut self, ty: &'ast ast::Ty) {
        if let ast::TyKind::Path(_, ref path) = ty.kind {
            self.check_path(path);
        }
        visit::walk_ty(self, ty);
    }

    fn visit_pat(&mut self, pat: &'ast ast::Pat) {
        if let ast::PatKind::Path(_, ref path)
        | ast::PatKind::TupleStruct(_, ref path, _)
        | ast::PatKind::Struct(_, ref path, _, _) = pat.kind
        {
            self.check_path(path);
        }
        visit::walk_pat(self, pat);
    }

    fn visit_item(&mut self, item: &'ast ast::Item) {
        if matches!(item.kind, ItemKind::Use(..)) {
            return;
        }
        visit::walk_item(self, item);
    }
}

#[test]
fn ui() {
    dylint_testing::ui_test(env!("CARGO_PKG_NAME"), "ui");
}
