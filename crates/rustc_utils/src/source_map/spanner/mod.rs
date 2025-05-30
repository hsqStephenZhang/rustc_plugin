//! Mapping source ranges to/from the HIR and MIR.

use either::Either;
use log::trace;
use rustc_hir::{self as hir, BodyId, ExprKind, MatchSource, Node};
use rustc_middle::{
  mir::{
    self, visit::Visitor as MirVisitor, Body, StatementKind, TerminatorKind, RETURN_PLACE,
  },
  ty::TyCtxt,
};
use rustc_span::{source_map::Spanned, Span, SpanData};

pub use self::hir_span::EnclosingHirSpans;
use self::{
  mir_span::{MirSpanCollector, MirSpannedPlace},
  span_tree::SpanTree,
};
use crate::{mir::location_or_arg::LocationOrArg, BodyExt, SpanDataExt, SpanExt};

mod hir_span;
mod mir_span;
mod span_tree;

/// Converts MIR locations to source spans using HIR information.
pub struct Spanner<'tcx> {
  pub(super) tcx: TyCtxt<'tcx>,
  pub(super) mir_spans: Vec<MirSpannedPlace<'tcx>>,
  pub mir_span_tree: SpanTree<MirSpannedPlace<'tcx>>,
  pub body_span: Span,
  pub item_span: Span,
  pub ret_span: Span,
}

impl<'tcx> Spanner<'tcx> {
  pub fn new(tcx: TyCtxt<'tcx>, body_id: BodyId, body: &Body<'tcx>) -> Self {
    let hir = tcx.hir();
    let hir_body = tcx.hir_body(body_id);
    let owner = tcx.hir_body_owner(body_id);
    let item_span = hir.span_with_body(owner);
    let ret_span = tcx.hir_fn_decl_by_hir_id(owner).unwrap().output.span();

    let mut spanner = Spanner {
      mir_spans: Vec::new(),
      mir_span_tree: SpanTree::new([]),
      body_span: hir_body.value.span,
      item_span,
      ret_span,
      tcx,
    };
    trace!(
      "Body span: {:?}, item span: {:?}",
      spanner.body_span,
      spanner.item_span
    );

    let mut mir_collector = MirSpanCollector(&mut spanner, body);
    mir_collector.visit_body(body);

    spanner.mir_span_tree =
      SpanTree::new(spanner.mir_spans.drain(..).map(|node| Spanned {
        span: node.span.span(),
        node,
      }));

    spanner
  }

  pub(super) fn invalid_span(&self, span: Span) -> bool {
    span.is_dummy()
      || span.source_equal(self.body_span)
      || span.source_equal(self.item_span)
  }

  fn find_matching<T>(
    predicate: impl Fn(SpanData) -> bool,
    query: SpanData,
    spans: &'_ SpanTree<T>,
  ) -> impl ExactSizeIterator<Item = &'_ T> + '_ {
    let mut matching = spans
      .overlapping(query)
      .filter(|(span, _)| predicate(*span))
      .collect::<Vec<_>>();
    matching.sort_by_key(|(span, _)| span.size());
    matching.into_iter().map(|(_, t)| t)
  }

  pub fn location_to_spans(
    &self,
    location: LocationOrArg,
    body: &Body,
    _span_type: EnclosingHirSpans,
  ) -> Vec<Span> {
    let (source_info, stmt) = match location {
      LocationOrArg::Arg(local) => (&body.local_decls[local].source_info, None),
      LocationOrArg::Location(location) => {
        (body.source_info(location), Some(body.stmt_at(location)))
      }
    };

    let hir_id = body.source_info_to_hir_id(source_info);

    let mir_span = match source_info.span.as_local(self.item_span) {
      Some(span) if !self.invalid_span(span) => span,
      _ => {
        return vec![];
      }
    };

    let mut hir_spans = Vec::new();

    // Include the MIR span, skipping spans that get mapped to the end brace of a body
    if mir_span != body.span.shrink_to_hi() {
      hir_spans.push(mir_span);
    }

    // Include the span for the immediately enclosing HIR node
    if let Some(spans) = self.hir_spans(hir_id, EnclosingHirSpans::OuterOnly) {
      hir_spans.extend(spans);
    }

    let enclosing_hir = self.tcx.hir_parent_iter(hir_id).collect::<Vec<_>>();
    macro_rules! add_first_matching {
      ($p:pat) => {
        if let Some((id, _)) = enclosing_hir.iter().find(|(_, node)| matches!(node, $p)) {
          if let Some(spans) = self.hir_spans(*id, EnclosingHirSpans::OuterOnly) {
            hir_spans.extend(spans);
          }
        }
      };
    }

    // Add the spans of the first enclosing statement
    add_first_matching!(Node::Stmt(..));

    // Include `return` keyword if the location is an expression under a return.
    add_first_matching!(Node::Expr(hir::Expr {
      kind: hir::ExprKind::Ret(..),
      ..
    }));

    if let Some(Either::Right(mir::Terminator {
      kind: TerminatorKind::SwitchInt { .. },
      ..
    })) = stmt
    {
      // If the location is a switch, then include the closest enclosing if or match
      add_first_matching!(Node::Expr(hir::Expr {
        kind: ExprKind::If(..) | ExprKind::Match(.., MatchSource::Normal),
        ..
      }));

      // Also include enclosing loops
      add_first_matching!(Node::Expr(hir::Expr {
        kind: ExprKind::Loop(..),
        ..
      }));
    }

    if let Some(Either::Left(mir::Statement {
      kind: StatementKind::Assign(box (lhs, _)),
      ..
    })) = stmt
    {
      if lhs.local == RETURN_PLACE {
        hir_spans.push(self.ret_span);
      }
    }

    let format_spans = |spans: &[Span]| -> String {
      spans
        .iter()
        .map(|span| span.to_string(self.tcx))
        .collect::<Vec<_>>()
        .join(" -- ")
    };

    trace!(
      "Location {location:?} ({})\n  has loc span:\n  {}\n  and HIR spans:\n  {}",
      location.to_string(body),
      format_spans(&[mir_span]),
      format_spans(&hir_spans)
    );

    hir_spans
  }

  pub fn span_to_places<'this>(
    &'this self,
    span: Span,
  ) -> Vec<&'this MirSpannedPlace<'tcx>> {
    // Note that MIR does not have granular source maps around projections.
    // So in the expression `let x = z.0`, the MIR Body only contains the place
    // z.0 with a span for the string "z.0". If the user selects only "z", there
    // is no way to determine map that selection back to a subset of the projection.
    //
    // At least, we can conservatively include the containing span "z.0" and slice on that.

    let span_data = span.data();

    let mut contained = Self::find_matching(
      move |mir_span| span_data.contains(mir_span),
      span_data,
      &self.mir_span_tree,
    );
    let mut vec = if let Some(first) = contained.next() {
      contained
        .take_while(|other| other.span.size() == first.span.size())
        .chain([first])
        .collect()
    } else {
      let mut containing = Self::find_matching(
        move |mir_span| mir_span.contains(span_data),
        span_data,
        &self.mir_span_tree,
      );
      if let Some(first) = containing.next() {
        containing
          .take_while(|other| other.span.size() == first.span.size())
          .chain([first])
          .collect()
      } else {
        Vec::new()
      }
    };

    vec.dedup();
    vec
  }
}

#[cfg(test)]
mod test {
  use rustc_data_structures::fx::FxHashSet as HashSet;
  use rustc_middle::mir::BasicBlock;
  use test_log::test;

  use super::*;
  use crate::{mir::borrowck_facts, source_map::range::ToSpan, test_utils};

  fn harness(
    src: &str,
    f: impl for<'tcx> FnOnce(TyCtxt<'tcx>, BodyId, &Body<'tcx>, Vec<Span>) + Send,
  ) {
    let (input, _) = test_utils::parse_ranges(src, [("`(", ")`")]).unwrap();
    test_utils::compile_body(input, move |tcx, body_id, body_with_facts| {
      let (_, mut ranges) = test_utils::parse_ranges(src, [("`(", ")`")]).unwrap();
      let spans = ranges
        .remove("`(")
        .unwrap()
        .into_iter()
        .map(|range| range.to_span(tcx).unwrap())
        .collect::<Vec<_>>();
      f(tcx, body_id, &body_with_facts.body, spans);
    });
  }

  #[test]
  fn test_span_to_places() {
    let src = r#"fn foo(`(z)`: i32){
      let `(x)` = 1;
      let y = 1;
      `(x + y)`;
      `(x)` + y;
      `(x + )`y;
      print!("{} {}", x, `(y)`);
      let w = (0, 0);
      `(w)`.0;
      `(w.0)`;
      `(w.)`0;
    }"#;
    harness(src, |tcx, body_id, body, spans| {
      let source_map = tcx.sess.source_map();
      let spanner = Spanner::new(tcx, body_id, body);
      let expected: &[&[_]] = &[
        &["z"],
        &["x"],
        &["x", "y"],
        &["x"],
        &["x"],
        &["y"],
        &["w.0"],
        &["w.0"],
        &["w.0"],
      ];
      for (input_span, desired) in spans.into_iter().zip(expected) {
        let outputs = spanner.span_to_places(input_span);
        let snippets = outputs
          .into_iter()
          .map(|spanned| source_map.span_to_snippet(spanned.span.span()).unwrap())
          .collect::<HashSet<_>>();

        println!("input_span={input_span:?}");
        compare_sets(&desired.iter().collect::<HashSet<_>>(), &snippets);
      }
    });
  }

  fn compare_sets(desired: &HashSet<impl AsRef<str>>, actual: &HashSet<impl AsRef<str>>) {
    let desired = desired.iter().map(AsRef::as_ref).collect::<HashSet<_>>();
    let actual = actual.iter().map(AsRef::as_ref).collect::<HashSet<_>>();
    let missing_desired = &desired - &actual;
    let missing_actual = &actual - &desired;

    let check = |key: &str, set: HashSet<&str>| {
      if let Some(el) = set.iter().next() {
        panic!("Missing {key}: {el}. Actual = {actual:?}. Desired = {desired:?}",);
      }
    };

    check("desired", missing_desired);
    check("actual", missing_actual);
  }

  #[test]
  fn test_location_to_spans() {
    let src = r"fn foo() {
  let mut x: i32 = 1;
  let y = x + 2;
  let w = if true {
    let z = 0;
    z
  } else {
    3
  };
  let z = &mut x;
  *z = 4;
  let q = x
    .leading_ones()
    .trailing_zeros();
}";

    // This affects source mapping, and this feature is primarily used by Flowistry, so
    // we enable MIR simplification for consistency with Flowistry.
    borrowck_facts::enable_mir_simplification();

    let (input, _ranges) = test_utils::parse_ranges(src, [("`(", ")`")]).unwrap();
    test_utils::compile_body(input, move |tcx, body_id, body_with_facts| {
      let body = &body_with_facts.body;
      let source_map = tcx.sess.source_map();

      let spanner = Spanner::new(tcx, body_id, body);

      // These locations are just selected by inspecting the actual body, so this test might break
      // if the compiler is updated. Run with RUST_LOG=debug to see the body.
      let pairs: &[(_, &[&str])] = &[
        // Variable assignment
        ((0, 0), &["let mut x: i32 = ", "1", ";"]),
        // Expression RHS
        ((0, 3), &["let y = ", "x", ";"]),
        ((0, 4), &["let y = ", " + ", "x + 2", ";"]),
        // If expression
        ((1, 2), &["let w = ", "true", ";"]),
        ((1, 3), &[
          "let w = ",
          "if ",
          "true",
          " {\n    ",
          "\n    ",
          "\n  } else {\n    ",
          "\n  }",
          ";",
        ]),
        // Reference
        ((4, 1), &["let z = ", "&mut ", "&mut x", ";"]),
        // Reference assignment
        ((4, 3), &[" = ", ";", "*z = 4"]),
        // Method chain
        ((4, 4), &["let q = ", "x", ";"]),
        ((4, 5), &[
          "let q = ",
          "x\n    .leading_ones()",
          "\n    .leading_ones()",
          ";",
        ]),
        ((5, 0), &[
          "let q = ",
          "x\n    .leading_ones()\n    .trailing_zeros()",
          "\n    .trailing_zeros()",
          ";",
        ]),
      ];

      for ((i, j), outp) in pairs {
        let loc = mir::Location {
          block: BasicBlock::from_usize(*i),
          statement_index: *j,
        };
        let spans = spanner.location_to_spans(
          LocationOrArg::Location(loc),
          body,
          EnclosingHirSpans::OuterOnly,
        );
        let desired = outp.iter().collect::<HashSet<_>>();
        let actual = spans
          .into_iter()
          .map(|s| source_map.span_to_snippet(s).unwrap())
          .collect::<HashSet<_>>();
        compare_sets(&desired, &actual);
      }
    });
  }
}
