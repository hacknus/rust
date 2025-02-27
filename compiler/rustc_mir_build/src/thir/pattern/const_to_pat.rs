use rustc_abi::{FieldIdx, VariantIdx};
use rustc_apfloat::Float;
use rustc_hir as hir;
use rustc_index::Idx;
use rustc_infer::infer::TyCtxtInferExt;
use rustc_infer::traits::Obligation;
use rustc_middle::mir::interpret::ErrorHandled;
use rustc_middle::thir::{FieldPat, Pat, PatKind};
use rustc_middle::ty::{self, Ty, TyCtxt, TypeVisitableExt, ValTree};
use rustc_middle::{mir, span_bug};
use rustc_span::Span;
use rustc_trait_selection::traits::ObligationCause;
use rustc_trait_selection::traits::query::evaluate_obligation::InferCtxtExt;
use tracing::{debug, instrument, trace};

use super::PatCtxt;
use crate::errors::{
    ConstPatternDependsOnGenericParameter, CouldNotEvalConstPattern, InvalidPattern, NaNPattern,
    PointerPattern, TypeNotPartialEq, TypeNotStructural, UnionPattern, UnsizedPattern,
};

impl<'a, 'tcx> PatCtxt<'a, 'tcx> {
    /// Converts a constant to a pattern (if possible).
    /// This means aggregate values (like structs and enums) are converted
    /// to a pattern that matches the value (as if you'd compared via structural equality).
    ///
    /// Only type system constants are supported, as we are using valtrees
    /// as an intermediate step. Unfortunately those don't carry a type
    /// so we have to carry one ourselves.
    #[instrument(level = "debug", skip(self), ret)]
    pub(super) fn const_to_pat(
        &self,
        c: ty::Const<'tcx>,
        ty: Ty<'tcx>,
        id: hir::HirId,
        span: Span,
    ) -> Box<Pat<'tcx>> {
        let mut convert = ConstToPat::new(self, id, span);

        match c.kind() {
            ty::ConstKind::Unevaluated(uv) => convert.unevaluated_to_pat(uv, ty),
            ty::ConstKind::Value(_, val) => convert.valtree_to_pat(val, ty),
            _ => span_bug!(span, "Invalid `ConstKind` for `const_to_pat`: {:?}", c),
        }
    }
}

struct ConstToPat<'tcx> {
    tcx: TyCtxt<'tcx>,
    typing_env: ty::TypingEnv<'tcx>,
    span: Span,

    treat_byte_string_as_slice: bool,
}

impl<'tcx> ConstToPat<'tcx> {
    fn new(pat_ctxt: &PatCtxt<'_, 'tcx>, id: hir::HirId, span: Span) -> Self {
        trace!(?pat_ctxt.typeck_results.hir_owner);
        ConstToPat {
            tcx: pat_ctxt.tcx,
            typing_env: pat_ctxt.typing_env,
            span,
            treat_byte_string_as_slice: pat_ctxt
                .typeck_results
                .treat_byte_string_as_slice
                .contains(&id.local_id),
        }
    }

    fn type_marked_structural(&self, ty: Ty<'tcx>) -> bool {
        ty.is_structural_eq_shallow(self.tcx)
    }

    fn unevaluated_to_pat(
        &mut self,
        uv: ty::UnevaluatedConst<'tcx>,
        ty: Ty<'tcx>,
    ) -> Box<Pat<'tcx>> {
        trace!(self.treat_byte_string_as_slice);
        let pat_from_kind = |kind| Box::new(Pat { span: self.span, ty, kind });

        // It's not *technically* correct to be revealing opaque types here as borrowcheck has
        // not run yet. However, CTFE itself uses `TypingMode::PostAnalysis` unconditionally even
        // during typeck and not doing so has a lot of (undesirable) fallout (#101478, #119821).
        // As a result we always use a revealed env when resolving the instance to evaluate.
        //
        // FIXME: `const_eval_resolve_for_typeck` should probably just modify the env itself
        // instead of having this logic here
        let typing_env =
            self.tcx.erase_regions(self.typing_env).with_post_analysis_normalized(self.tcx);
        let uv = self.tcx.erase_regions(uv);

        // try to resolve e.g. associated constants to their definition on an impl, and then
        // evaluate the const.
        let valtree = match self.tcx.const_eval_resolve_for_typeck(typing_env, uv, self.span) {
            Ok(Ok(c)) => c,
            Err(ErrorHandled::Reported(_, _)) => {
                // Let's tell the use where this failing const occurs.
                let e = self.tcx.dcx().emit_err(CouldNotEvalConstPattern { span: self.span });
                return pat_from_kind(PatKind::Error(e));
            }
            Err(ErrorHandled::TooGeneric(_)) => {
                let e = self
                    .tcx
                    .dcx()
                    .emit_err(ConstPatternDependsOnGenericParameter { span: self.span });
                return pat_from_kind(PatKind::Error(e));
            }
            Ok(Err(bad_ty)) => {
                // The pattern cannot be turned into a valtree.
                let e = match bad_ty.kind() {
                    ty::Adt(def, ..) => {
                        assert!(def.is_union());
                        self.tcx.dcx().emit_err(UnionPattern { span: self.span })
                    }
                    ty::FnPtr(..) | ty::RawPtr(..) => {
                        self.tcx.dcx().emit_err(PointerPattern { span: self.span })
                    }
                    _ => self
                        .tcx
                        .dcx()
                        .emit_err(InvalidPattern { span: self.span, non_sm_ty: bad_ty }),
                };
                return pat_from_kind(PatKind::Error(e));
            }
        };

        // Convert the valtree to a const.
        let inlined_const_as_pat = self.valtree_to_pat(valtree, ty);

        if !inlined_const_as_pat.references_error() {
            // Always check for `PartialEq` if we had no other errors yet.
            if !self.type_has_partial_eq_impl(ty) {
                let err = TypeNotPartialEq { span: self.span, non_peq_ty: ty };
                let e = self.tcx.dcx().emit_err(err);
                return pat_from_kind(PatKind::Error(e));
            }
        }

        inlined_const_as_pat
    }

    #[instrument(level = "trace", skip(self), ret)]
    fn type_has_partial_eq_impl(&self, ty: Ty<'tcx>) -> bool {
        let (infcx, param_env) = self.tcx.infer_ctxt().build_with_typing_env(self.typing_env);
        // double-check there even *is* a semantic `PartialEq` to dispatch to.
        //
        // (If there isn't, then we can safely issue a hard
        // error, because that's never worked, due to compiler
        // using `PartialEq::eq` in this scenario in the past.)
        let partial_eq_trait_id =
            self.tcx.require_lang_item(hir::LangItem::PartialEq, Some(self.span));
        let partial_eq_obligation = Obligation::new(
            self.tcx,
            ObligationCause::dummy(),
            param_env,
            ty::TraitRef::new(self.tcx, partial_eq_trait_id, [ty, ty]),
        );

        // This *could* accept a type that isn't actually `PartialEq`, because region bounds get
        // ignored. However that should be pretty much impossible since consts that do not depend on
        // generics can only mention the `'static` lifetime, and how would one have a type that's
        // `PartialEq` for some lifetime but *not* for `'static`? If this ever becomes a problem
        // we'll need to leave some sort of trace of this requirement in the MIR so that borrowck
        // can ensure that the type really implements `PartialEq`.
        infcx.predicate_must_hold_modulo_regions(&partial_eq_obligation)
    }

    fn field_pats(
        &self,
        vals: impl Iterator<Item = (ValTree<'tcx>, Ty<'tcx>)>,
    ) -> Vec<FieldPat<'tcx>> {
        vals.enumerate()
            .map(|(idx, (val, ty))| {
                let field = FieldIdx::new(idx);
                // Patterns can only use monomorphic types.
                let ty = self.tcx.normalize_erasing_regions(self.typing_env, ty);
                FieldPat { field, pattern: self.valtree_to_pat(val, ty) }
            })
            .collect()
    }

    // Recursive helper for `to_pat`; invoke that (instead of calling this directly).
    #[instrument(skip(self), level = "debug")]
    fn valtree_to_pat(&self, cv: ValTree<'tcx>, ty: Ty<'tcx>) -> Box<Pat<'tcx>> {
        let span = self.span;
        let tcx = self.tcx;
        let kind = match ty.kind() {
            ty::Adt(adt_def, _) if !self.type_marked_structural(ty) => {
                // Extremely important check for all ADTs! Make sure they opted-in to be used in
                // patterns.
                debug!("adt_def {:?} has !type_marked_structural for cv.ty: {:?}", adt_def, ty);
                let err = TypeNotStructural { span, non_sm_ty: ty };
                let e = tcx.dcx().emit_err(err);
                // We errored. Signal that in the pattern, so that follow up errors can be silenced.
                PatKind::Error(e)
            }
            ty::Adt(adt_def, args) if adt_def.is_enum() => {
                let (&variant_index, fields) = cv.unwrap_branch().split_first().unwrap();
                let variant_index = VariantIdx::from_u32(variant_index.unwrap_leaf().to_u32());
                PatKind::Variant {
                    adt_def: *adt_def,
                    args,
                    variant_index,
                    subpatterns: self.field_pats(
                        fields.iter().copied().zip(
                            adt_def.variants()[variant_index]
                                .fields
                                .iter()
                                .map(|field| field.ty(self.tcx, args)),
                        ),
                    ),
                }
            }
            ty::Adt(def, args) => {
                assert!(!def.is_union()); // Valtree construction would never succeed for unions.
                PatKind::Leaf {
                    subpatterns: self.field_pats(cv.unwrap_branch().iter().copied().zip(
                        def.non_enum_variant().fields.iter().map(|field| field.ty(self.tcx, args)),
                    )),
                }
            }
            ty::Tuple(fields) => PatKind::Leaf {
                subpatterns: self.field_pats(cv.unwrap_branch().iter().copied().zip(fields.iter())),
            },
            ty::Slice(elem_ty) => PatKind::Slice {
                prefix: cv
                    .unwrap_branch()
                    .iter()
                    .map(|val| self.valtree_to_pat(*val, *elem_ty))
                    .collect(),
                slice: None,
                suffix: Box::new([]),
            },
            ty::Array(elem_ty, _) => PatKind::Array {
                prefix: cv
                    .unwrap_branch()
                    .iter()
                    .map(|val| self.valtree_to_pat(*val, *elem_ty))
                    .collect(),
                slice: None,
                suffix: Box::new([]),
            },
            ty::Ref(_, pointee_ty, ..) => match *pointee_ty.kind() {
                // `&str` is represented as a valtree, let's keep using this
                // optimization for now.
                ty::Str => PatKind::Constant {
                    value: mir::Const::Ty(ty, ty::Const::new_value(tcx, cv, ty)),
                },
                // All other references are converted into deref patterns and then recursively
                // convert the dereferenced constant to a pattern that is the sub-pattern of the
                // deref pattern.
                _ => {
                    if !pointee_ty.is_sized(tcx, self.typing_env) && !pointee_ty.is_slice() {
                        let err = UnsizedPattern { span, non_sm_ty: *pointee_ty };
                        let e = tcx.dcx().emit_err(err);
                        // We errored. Signal that in the pattern, so that follow up errors can be silenced.
                        PatKind::Error(e)
                    } else {
                        // `b"foo"` produces a `&[u8; 3]`, but you can't use constants of array type when
                        // matching against references, you can only use byte string literals.
                        // The typechecker has a special case for byte string literals, by treating them
                        // as slices. This means we turn `&[T; N]` constants into slice patterns, which
                        // has no negative effects on pattern matching, even if we're actually matching on
                        // arrays.
                        let pointee_ty = match *pointee_ty.kind() {
                            ty::Array(elem_ty, _) if self.treat_byte_string_as_slice => {
                                Ty::new_slice(tcx, elem_ty)
                            }
                            _ => *pointee_ty,
                        };
                        // References have the same valtree representation as their pointee.
                        let subpattern = self.valtree_to_pat(cv, pointee_ty);
                        PatKind::Deref { subpattern }
                    }
                }
            },
            ty::Float(flt) => {
                let v = cv.unwrap_leaf();
                let is_nan = match flt {
                    ty::FloatTy::F16 => v.to_f16().is_nan(),
                    ty::FloatTy::F32 => v.to_f32().is_nan(),
                    ty::FloatTy::F64 => v.to_f64().is_nan(),
                    ty::FloatTy::F128 => v.to_f128().is_nan(),
                };
                if is_nan {
                    // NaNs are not ever equal to anything so they make no sense as patterns.
                    // Also see <https://github.com/rust-lang/rfcs/pull/3535>.
                    let e = tcx.dcx().emit_err(NaNPattern { span });
                    PatKind::Error(e)
                } else {
                    PatKind::Constant {
                        value: mir::Const::Ty(ty, ty::Const::new_value(tcx, cv, ty)),
                    }
                }
            }
            ty::Pat(..) | ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_) | ty::RawPtr(..) => {
                // The raw pointers we see here have been "vetted" by valtree construction to be
                // just integers, so we simply allow them.
                PatKind::Constant { value: mir::Const::Ty(ty, ty::Const::new_value(tcx, cv, ty)) }
            }
            ty::FnPtr(..) => {
                unreachable!(
                    "Valtree construction would never succeed for FnPtr, so this is unreachable."
                )
            }
            _ => {
                let err = InvalidPattern { span, non_sm_ty: ty };
                let e = tcx.dcx().emit_err(err);
                // We errored. Signal that in the pattern, so that follow up errors can be silenced.
                PatKind::Error(e)
            }
        };

        Box::new(Pat { span, ty, kind })
    }
}
