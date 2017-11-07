// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! This pass type-checks the MIR to ensure it is not broken.
#![allow(unreachable_code)]

use rustc::infer::{InferCtxt, InferOk, InferResult, UnitResult};
use rustc::traits::{self, FulfillmentContext};
use rustc::ty::error::TypeError;
use rustc::ty::fold::TypeFoldable;
use rustc::ty::{self, Ty, TyCtxt, TypeVariants};
use rustc::middle::const_val::ConstVal;
use rustc::mir::*;
use rustc::mir::tcx::LvalueTy;
use rustc::mir::transform::{MirPass, MirSource};
use rustc::mir::visit::Visitor;
use std::fmt;
use syntax::ast;
use syntax_pos::{Span, DUMMY_SP};

use rustc_data_structures::fx::FxHashSet;
use rustc_data_structures::indexed_vec::Idx;

fn mirbug(tcx: TyCtxt, span: Span, msg: &str) {
    tcx.sess.diagnostic().span_bug(span, msg);
}

macro_rules! span_mirbug {
    ($context:expr, $elem:expr, $($message:tt)*) => ({
        mirbug($context.tcx(), $context.last_span,
               &format!("broken MIR in {:?} ({:?}): {}",
                        $context.body_id,
                        $elem,
                        format_args!($($message)*)))
    })
}

macro_rules! span_mirbug_and_err {
    ($context:expr, $elem:expr, $($message:tt)*) => ({
        {
            span_mirbug!($context, $elem, $($message)*);
            $context.error()
        }
    })
}

enum FieldAccessError {
    OutOfRange { field_count: usize },
}

/// Verifies that MIR types are sane to not crash further checks.
///
/// The sanitize_XYZ methods here take an MIR object and compute its
/// type, calling `span_mirbug` and returning an error type if there
/// is a problem.
struct TypeVerifier<'a, 'b: 'a, 'gcx: 'b + 'tcx, 'tcx: 'b> {
    cx: &'a mut TypeChecker<'b, 'gcx, 'tcx>,
    mir: &'a Mir<'tcx>,
    last_span: Span,
    body_id: ast::NodeId,
    errors_reported: bool,
}

impl<'a, 'b, 'gcx, 'tcx> Visitor<'tcx> for TypeVerifier<'a, 'b, 'gcx, 'tcx> {
    fn visit_span(&mut self, span: &Span) {
        if *span != DUMMY_SP {
            self.last_span = *span;
        }
    }

    fn visit_lvalue(
        &mut self,
        lvalue: &Lvalue<'tcx>,
        _context: visit::LvalueContext,
        location: Location,
    ) {
        self.sanitize_lvalue(lvalue, location);
    }

    fn visit_constant(&mut self, constant: &Constant<'tcx>, location: Location) {
        self.super_constant(constant, location);
        self.sanitize_type(constant, constant.ty);
    }

    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
        self.super_rvalue(rvalue, location);
        let rval_ty = rvalue.ty(self.mir, self.tcx());
        self.sanitize_type(rvalue, rval_ty);
    }

    fn visit_local_decl(&mut self, local: Local, local_decl: &LocalDecl<'tcx>) {
        self.super_local_decl(local, local_decl);
        self.sanitize_type(local_decl, local_decl.ty);
    }

    fn visit_mir(&mut self, mir: &Mir<'tcx>) {
        self.sanitize_type(&"return type", mir.return_ty);
        for local_decl in &mir.local_decls {
            self.sanitize_type(local_decl, local_decl.ty);
        }
        if self.errors_reported {
            return;
        }
        self.super_mir(mir);
    }
}

impl<'a, 'b, 'gcx, 'tcx> TypeVerifier<'a, 'b, 'gcx, 'tcx> {
    fn new(cx: &'a mut TypeChecker<'b, 'gcx, 'tcx>, mir: &'a Mir<'tcx>) -> Self {
        TypeVerifier {
            mir,
            body_id: cx.body_id,
            cx,
            last_span: mir.span,
            errors_reported: false,
        }
    }

    fn tcx(&self) -> TyCtxt<'a, 'gcx, 'tcx> {
        self.cx.infcx.tcx
    }

    fn sanitize_type(&mut self, parent: &fmt::Debug, ty: Ty<'tcx>) -> Ty<'tcx> {
        if ty.needs_infer() || ty.has_escaping_regions() || ty.references_error() {
            span_mirbug_and_err!(self, parent, "bad type {:?}", ty)
        } else {
            ty
        }
    }

    fn sanitize_lvalue(&mut self, lvalue: &Lvalue<'tcx>, location: Location) -> LvalueTy<'tcx> {
        debug!("sanitize_lvalue: {:?}", lvalue);
        match *lvalue {
            Lvalue::Local(index) => LvalueTy::Ty {
                ty: self.mir.local_decls[index].ty,
            },
            Lvalue::Static(box Static { def_id, ty: sty }) => {
                let sty = self.sanitize_type(lvalue, sty);
                let ty = self.tcx().type_of(def_id);
                let ty = self.cx.normalize(&ty, location);
                if let Err(terr) = self.cx.eq_types(self.last_span, ty, sty, location) {
                    span_mirbug!(
                        self,
                        lvalue,
                        "bad static type ({:?}: {:?}): {:?}",
                        ty,
                        sty,
                        terr
                    );
                }
                LvalueTy::Ty { ty: sty }
            }
            Lvalue::Projection(ref proj) => {
                let base_ty = self.sanitize_lvalue(&proj.base, location);
                if let LvalueTy::Ty { ty } = base_ty {
                    if ty.references_error() {
                        assert!(self.errors_reported);
                        return LvalueTy::Ty {
                            ty: self.tcx().types.err,
                        };
                    }
                }
                self.sanitize_projection(base_ty, &proj.elem, lvalue, location)
            }
        }
    }

    fn sanitize_projection(
        &mut self,
        base: LvalueTy<'tcx>,
        pi: &LvalueElem<'tcx>,
        lvalue: &Lvalue<'tcx>,
        location: Location,
    ) -> LvalueTy<'tcx> {
        debug!("sanitize_projection: {:?} {:?} {:?}", base, pi, lvalue);
        let tcx = self.tcx();
        let base_ty = base.to_ty(tcx);
        let span = self.last_span;
        match *pi {
            ProjectionElem::Deref => {
                let deref_ty = base_ty.builtin_deref(true, ty::LvaluePreference::NoPreference);
                LvalueTy::Ty {
                    ty: deref_ty.map(|t| t.ty).unwrap_or_else(|| {
                        span_mirbug_and_err!(self, lvalue, "deref of non-pointer {:?}", base_ty)
                    }),
                }
            }
            ProjectionElem::Index(i) => {
                let index_ty = Lvalue::Local(i).ty(self.mir, tcx).to_ty(tcx);
                if index_ty != tcx.types.usize {
                    LvalueTy::Ty {
                        ty: span_mirbug_and_err!(self, i, "index by non-usize {:?}", i),
                    }
                } else {
                    LvalueTy::Ty {
                        ty: base_ty.builtin_index().unwrap_or_else(|| {
                            span_mirbug_and_err!(self, lvalue, "index of non-array {:?}", base_ty)
                        }),
                    }
                }
            }
            ProjectionElem::ConstantIndex { .. } => {
                // consider verifying in-bounds
                LvalueTy::Ty {
                    ty: base_ty.builtin_index().unwrap_or_else(|| {
                        span_mirbug_and_err!(self, lvalue, "index of non-array {:?}", base_ty)
                    }),
                }
            }
            ProjectionElem::Subslice { from, to } => LvalueTy::Ty {
                ty: match base_ty.sty {
                    ty::TyArray(inner, size) => {
                        let size = size.val.to_const_int().unwrap().to_u64().unwrap();
                        let min_size = (from as u64) + (to as u64);
                        if let Some(rest_size) = size.checked_sub(min_size) {
                            tcx.mk_array(inner, rest_size)
                        } else {
                            span_mirbug_and_err!(
                                self,
                                lvalue,
                                "taking too-small slice of {:?}",
                                base_ty
                            )
                        }
                    }
                    ty::TySlice(..) => base_ty,
                    _ => span_mirbug_and_err!(self, lvalue, "slice of non-array {:?}", base_ty),
                },
            },
            ProjectionElem::Downcast(adt_def1, index) => match base_ty.sty {
                ty::TyAdt(adt_def, substs) if adt_def.is_enum() && adt_def == adt_def1 => {
                    if index >= adt_def.variants.len() {
                        LvalueTy::Ty {
                            ty: span_mirbug_and_err!(
                                self,
                                lvalue,
                                "cast to variant #{:?} but enum only has {:?}",
                                index,
                                adt_def.variants.len()
                            ),
                        }
                    } else {
                        LvalueTy::Downcast {
                            adt_def,
                            substs,
                            variant_index: index,
                        }
                    }
                }
                _ => LvalueTy::Ty {
                    ty: span_mirbug_and_err!(
                        self,
                        lvalue,
                        "can't downcast {:?} as {:?}",
                        base_ty,
                        adt_def1
                    ),
                },
            },
            ProjectionElem::Field(field, fty) => {
                let fty = self.sanitize_type(lvalue, fty);
                match self.field_ty(lvalue, base, field, location) {
                    Ok(ty) => if let Err(terr) = self.cx.eq_types(span, ty, fty, location) {
                        span_mirbug!(
                            self,
                            lvalue,
                            "bad field access ({:?}: {:?}): {:?}",
                            ty,
                            fty,
                            terr
                        );
                    },
                    Err(FieldAccessError::OutOfRange { field_count }) => span_mirbug!(
                        self,
                        lvalue,
                        "accessed field #{} but variant only has {}",
                        field.index(),
                        field_count
                    ),
                }
                LvalueTy::Ty { ty: fty }
            }
        }
    }

    fn error(&mut self) -> Ty<'tcx> {
        self.errors_reported = true;
        self.tcx().types.err
    }

    fn field_ty(
        &mut self,
        parent: &fmt::Debug,
        base_ty: LvalueTy<'tcx>,
        field: Field,
        location: Location,
    ) -> Result<Ty<'tcx>, FieldAccessError> {
        let tcx = self.tcx();

        let (variant, substs) = match base_ty {
            LvalueTy::Downcast {
                adt_def,
                substs,
                variant_index,
            } => (&adt_def.variants[variant_index], substs),
            LvalueTy::Ty { ty } => match ty.sty {
                ty::TyAdt(adt_def, substs) if adt_def.is_univariant() => {
                    (&adt_def.variants[0], substs)
                }
                ty::TyClosure(def_id, substs) => {
                    return match substs.upvar_tys(def_id, tcx).nth(field.index()) {
                        Some(ty) => Ok(ty),
                        None => Err(FieldAccessError::OutOfRange {
                            field_count: substs.upvar_tys(def_id, tcx).count(),
                        }),
                    }
                }
                ty::TyGenerator(def_id, substs, _) => {
                    // Try upvars first. `field_tys` requires final optimized MIR.
                    if let Some(ty) = substs.upvar_tys(def_id, tcx).nth(field.index()) {
                        return Ok(ty);
                    }

                    return match substs.field_tys(def_id, tcx).nth(field.index()) {
                        Some(ty) => Ok(ty),
                        None => Err(FieldAccessError::OutOfRange {
                            field_count: substs.field_tys(def_id, tcx).count() + 1,
                        }),
                    };
                }
                ty::TyTuple(tys, _) => {
                    return match tys.get(field.index()) {
                        Some(&ty) => Ok(ty),
                        None => Err(FieldAccessError::OutOfRange {
                            field_count: tys.len(),
                        }),
                    }
                }
                _ => {
                    return Ok(span_mirbug_and_err!(
                        self,
                        parent,
                        "can't project out of {:?}",
                        base_ty
                    ))
                }
            },
        };

        if let Some(field) = variant.fields.get(field.index()) {
            Ok(self.cx.normalize(&field.ty(tcx, substs), location))
        } else {
            Err(FieldAccessError::OutOfRange {
                field_count: variant.fields.len(),
            })
        }
    }
}

pub struct TypeChecker<'a, 'gcx: 'a + 'tcx, 'tcx: 'a> {
    infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
    param_env: ty::ParamEnv<'gcx>,
    last_span: Span,
    body_id: ast::NodeId,
    reported_errors: FxHashSet<(Ty<'tcx>, Span)>,
}

impl<'a, 'gcx, 'tcx> TypeChecker<'a, 'gcx, 'tcx> {
    fn new(
        infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
        body_id: ast::NodeId,
        param_env: ty::ParamEnv<'gcx>,
    ) -> Self {
        TypeChecker {
            infcx,
            last_span: DUMMY_SP,
            body_id,
            param_env,
            reported_errors: FxHashSet(),
        }
    }

    fn misc(&self, span: Span) -> traits::ObligationCause<'tcx> {
        traits::ObligationCause::misc(span, self.body_id)
    }

    fn fully_perform_op<OP, R>(&self, op: OP) -> Result<R, TypeError<'tcx>>
    where
        OP: FnOnce() -> InferResult<'tcx, R>,
    {
        let mut fulfill_cx = FulfillmentContext::new();
        let InferOk { value, obligations } = self.infcx.commit_if_ok(|_| op())?;
        fulfill_cx.register_predicate_obligations(self.infcx, obligations);
        if let Err(e) = fulfill_cx.select_all_or_error(self.infcx) {
            span_mirbug!(self, "", "errors selecting obligation: {:?}", e);
        } // FIXME propagate
        Ok(value)
    }

    fn sub_types(&self, sub: Ty<'tcx>, sup: Ty<'tcx>, _at_location: Location) -> UnitResult<'tcx> {
        self.fully_perform_op(|| {
            self.infcx
                .at(&self.misc(self.last_span), self.param_env)
                .sup(sup, sub)
        })
    }

    fn eq_types(
        &self,
        _span: Span,
        a: Ty<'tcx>,
        b: Ty<'tcx>,
        _at_location: Location,
    ) -> UnitResult<'tcx> {
        self.fully_perform_op(|| {
            self.infcx
                .at(&self.misc(self.last_span), self.param_env)
                .eq(b, a)
        })
    }

    fn tcx(&self) -> TyCtxt<'a, 'gcx, 'tcx> {
        self.infcx.tcx
    }

    fn check_stmt(&mut self, mir: &Mir<'tcx>, stmt: &Statement<'tcx>, location: Location) {
        debug!("check_stmt: {:?}", stmt);
        let tcx = self.tcx();
        match stmt.kind {
            StatementKind::Assign(ref lv, ref rv) => {
                let lv_ty = lv.ty(mir, tcx).to_ty(tcx);
                let rv_ty = rv.ty(mir, tcx);
                if let Err(terr) = self.sub_types(rv_ty, lv_ty, location.successor_within_block()) {
                    span_mirbug!(
                        self,
                        stmt,
                        "bad assignment ({:?} = {:?}): {:?}",
                        lv_ty,
                        rv_ty,
                        terr
                    );
                }
            }
            StatementKind::SetDiscriminant {
                ref lvalue,
                variant_index,
            } => {
                let lvalue_type = lvalue.ty(mir, tcx).to_ty(tcx);
                let adt = match lvalue_type.sty {
                    TypeVariants::TyAdt(adt, _) if adt.is_enum() => adt,
                    _ => {
                        span_bug!(
                            stmt.source_info.span,
                            "bad set discriminant ({:?} = {:?}): lhs is not an enum",
                            lvalue,
                            variant_index
                        );
                    }
                };
                if variant_index >= adt.variants.len() {
                    span_bug!(
                        stmt.source_info.span,
                        "bad set discriminant ({:?} = {:?}): value of of range",
                        lvalue,
                        variant_index
                    );
                };
            }
            StatementKind::StorageLive(_) |
            StatementKind::StorageDead(_) |
            StatementKind::InlineAsm { .. } |
            StatementKind::EndRegion(_) |
            StatementKind::Validate(..) |
            StatementKind::Nop => {}
        }
    }

    fn check_terminator(&mut self, mir: &Mir<'tcx>, term: &Terminator<'tcx>, location: Location) {
        debug!("check_terminator: {:?}", term);
        let tcx = self.tcx();
        match term.kind {
            TerminatorKind::Goto { .. } |
            TerminatorKind::Resume |
            TerminatorKind::Return |
            TerminatorKind::GeneratorDrop |
            TerminatorKind::Unreachable |
            TerminatorKind::Drop { .. } |
            TerminatorKind::FalseEdges { .. } => {
                // no checks needed for these
            }

            TerminatorKind::DropAndReplace {
                ref location,
                ref value,
                target,
                unwind,
            } => {
                let lv_ty = location.ty(mir, tcx).to_ty(tcx);
                let rv_ty = value.ty(mir, tcx);

                if let Err(terr) = self.sub_types(rv_ty, lv_ty, target.start_location()) {
                    span_mirbug!(
                        self,
                        term,
                        "bad DropAndReplace ({:?} = {:?}): {:?}",
                        lv_ty,
                        rv_ty,
                        terr
                    );
                }

                // Subtle: this assignment occurs at the start of
                // *both* blocks, so we need to ensure that it holds
                // at both locations.
                if let Some(unwind) = unwind {
                    if let Err(terr) = self.sub_types(rv_ty, lv_ty, unwind.start_location()) {
                        span_mirbug!(
                            self,
                            term,
                            "bad DropAndReplace ({:?} = {:?}): {:?}",
                            lv_ty,
                            rv_ty,
                            terr
                        );
                    }
                }
            }
            TerminatorKind::SwitchInt {
                ref discr,
                switch_ty,
                ..
            } => {
                let discr_ty = discr.ty(mir, tcx);
                if let Err(terr) = self.sub_types(discr_ty, switch_ty, location) {
                    span_mirbug!(
                        self,
                        term,
                        "bad SwitchInt ({:?} on {:?}): {:?}",
                        switch_ty,
                        discr_ty,
                        terr
                    );
                }
                if !switch_ty.is_integral() && !switch_ty.is_char() && !switch_ty.is_bool() {
                    span_mirbug!(self, term, "bad SwitchInt discr ty {:?}", switch_ty);
                }
                // FIXME: check the values
            }
            TerminatorKind::Call {
                ref func,
                ref args,
                ref destination,
                ..
            } => {
                let func_ty = func.ty(mir, tcx);
                debug!("check_terminator: call, func_ty={:?}", func_ty);
                let sig = match func_ty.sty {
                    ty::TyFnDef(..) | ty::TyFnPtr(_) => func_ty.fn_sig(tcx),
                    _ => {
                        span_mirbug!(self, term, "call to non-function {:?}", func_ty);
                        return;
                    }
                };
                let sig = tcx.erase_late_bound_regions(&sig);
                let sig = self.normalize(&sig, location);
                self.check_call_dest(mir, term, &sig, destination);

                if self.is_box_free(func) {
                    self.check_box_free_inputs(mir, term, &sig, args, location);
                } else {
                    self.check_call_inputs(mir, term, &sig, args, location);
                }
            }
            TerminatorKind::Assert {
                ref cond, ref msg, ..
            } => {
                let cond_ty = cond.ty(mir, tcx);
                if cond_ty != tcx.types.bool {
                    span_mirbug!(self, term, "bad Assert ({:?}, not bool", cond_ty);
                }

                if let AssertMessage::BoundsCheck { ref len, ref index } = *msg {
                    if len.ty(mir, tcx) != tcx.types.usize {
                        span_mirbug!(self, len, "bounds-check length non-usize {:?}", len)
                    }
                    if index.ty(mir, tcx) != tcx.types.usize {
                        span_mirbug!(self, index, "bounds-check index non-usize {:?}", index)
                    }
                }
            }
            TerminatorKind::Yield { ref value, .. } => {
                let value_ty = value.ty(mir, tcx);
                match mir.yield_ty {
                    None => span_mirbug!(self, term, "yield in non-generator"),
                    Some(ty) => if let Err(terr) = self.sub_types(value_ty, ty, location) {
                        span_mirbug!(
                            self,
                            term,
                            "type of yield value is {:?}, but the yield type is {:?}: {:?}",
                            value_ty,
                            ty,
                            terr
                        );
                    },
                }
            }
        }
    }

    fn check_call_dest(
        &mut self,
        mir: &Mir<'tcx>,
        term: &Terminator<'tcx>,
        sig: &ty::FnSig<'tcx>,
        destination: &Option<(Lvalue<'tcx>, BasicBlock)>,
    ) {
        let tcx = self.tcx();
        match *destination {
            Some((ref dest, target_block)) => {
                let dest_ty = dest.ty(mir, tcx).to_ty(tcx);
                if let Err(terr) =
                    self.sub_types(sig.output(), dest_ty, target_block.start_location())
                {
                    span_mirbug!(
                        self,
                        term,
                        "call dest mismatch ({:?} <- {:?}): {:?}",
                        dest_ty,
                        sig.output(),
                        terr
                    );
                }
            }
            None => {
                // FIXME(canndrew): This is_never should probably be an is_uninhabited
                if !sig.output().is_never() {
                    span_mirbug!(self, term, "call to converging function {:?} w/o dest", sig);
                }
            }
        }
    }

    fn check_call_inputs(
        &mut self,
        mir: &Mir<'tcx>,
        term: &Terminator<'tcx>,
        sig: &ty::FnSig<'tcx>,
        args: &[Operand<'tcx>],
        location: Location,
    ) {
        debug!("check_call_inputs({:?}, {:?})", sig, args);
        if args.len() < sig.inputs().len() || (args.len() > sig.inputs().len() && !sig.variadic) {
            span_mirbug!(self, term, "call to {:?} with wrong # of args", sig);
        }
        for (n, (fn_arg, op_arg)) in sig.inputs().iter().zip(args).enumerate() {
            let op_arg_ty = op_arg.ty(mir, self.tcx());
            if let Err(terr) = self.sub_types(op_arg_ty, fn_arg, location) {
                span_mirbug!(
                    self,
                    term,
                    "bad arg #{:?} ({:?} <- {:?}): {:?}",
                    n,
                    fn_arg,
                    op_arg_ty,
                    terr
                );
            }
        }
    }

    fn is_box_free(&self, operand: &Operand<'tcx>) -> bool {
        match operand {
            &Operand::Constant(box Constant {
                literal:
                    Literal::Value {
                        value:
                            &ty::Const {
                                val: ConstVal::Function(def_id, _),
                                ..
                            },
                        ..
                    },
                ..
            }) => Some(def_id) == self.tcx().lang_items().box_free_fn(),
            _ => false,
        }
    }

    fn check_box_free_inputs(
        &mut self,
        mir: &Mir<'tcx>,
        term: &Terminator<'tcx>,
        sig: &ty::FnSig<'tcx>,
        args: &[Operand<'tcx>],
        location: Location,
    ) {
        debug!("check_box_free_inputs");

        // box_free takes a Box as a pointer. Allow for that.

        if sig.inputs().len() != 1 {
            span_mirbug!(self, term, "box_free should take 1 argument");
            return;
        }

        let pointee_ty = match sig.inputs()[0].sty {
            ty::TyRawPtr(mt) => mt.ty,
            _ => {
                span_mirbug!(self, term, "box_free should take a raw ptr");
                return;
            }
        };

        if args.len() != 1 {
            span_mirbug!(self, term, "box_free called with wrong # of args");
            return;
        }

        let ty = args[0].ty(mir, self.tcx());
        let arg_ty = match ty.sty {
            ty::TyRawPtr(mt) => mt.ty,
            ty::TyAdt(def, _) if def.is_box() => ty.boxed_ty(),
            _ => {
                span_mirbug!(self, term, "box_free called with bad arg ty");
                return;
            }
        };

        if let Err(terr) = self.sub_types(arg_ty, pointee_ty, location) {
            span_mirbug!(
                self,
                term,
                "bad box_free arg ({:?} <- {:?}): {:?}",
                pointee_ty,
                arg_ty,
                terr
            );
        }
    }

    fn check_iscleanup(&mut self, mir: &Mir<'tcx>, block_data: &BasicBlockData<'tcx>) {
        let is_cleanup = block_data.is_cleanup;
        self.last_span = block_data.terminator().source_info.span;
        match block_data.terminator().kind {
            TerminatorKind::Goto { target } => {
                self.assert_iscleanup(mir, block_data, target, is_cleanup)
            }
            TerminatorKind::SwitchInt { ref targets, .. } => for target in targets {
                self.assert_iscleanup(mir, block_data, *target, is_cleanup);
            },
            TerminatorKind::Resume => if !is_cleanup {
                span_mirbug!(self, block_data, "resume on non-cleanup block!")
            },
            TerminatorKind::Return => if is_cleanup {
                span_mirbug!(self, block_data, "return on cleanup block")
            },
            TerminatorKind::GeneratorDrop { .. } => if is_cleanup {
                span_mirbug!(self, block_data, "generator_drop in cleanup block")
            },
            TerminatorKind::Yield { resume, drop, .. } => {
                if is_cleanup {
                    span_mirbug!(self, block_data, "yield in cleanup block")
                }
                self.assert_iscleanup(mir, block_data, resume, is_cleanup);
                if let Some(drop) = drop {
                    self.assert_iscleanup(mir, block_data, drop, is_cleanup);
                }
            }
            TerminatorKind::Unreachable => {}
            TerminatorKind::Drop { target, unwind, .. } |
            TerminatorKind::DropAndReplace { target, unwind, .. } |
            TerminatorKind::Assert {
                target,
                cleanup: unwind,
                ..
            } => {
                self.assert_iscleanup(mir, block_data, target, is_cleanup);
                if let Some(unwind) = unwind {
                    if is_cleanup {
                        span_mirbug!(self, block_data, "unwind on cleanup block")
                    }
                    self.assert_iscleanup(mir, block_data, unwind, true);
                }
            }
            TerminatorKind::Call {
                ref destination,
                cleanup,
                ..
            } => {
                if let &Some((_, target)) = destination {
                    self.assert_iscleanup(mir, block_data, target, is_cleanup);
                }
                if let Some(cleanup) = cleanup {
                    if is_cleanup {
                        span_mirbug!(self, block_data, "cleanup on cleanup block")
                    }
                    self.assert_iscleanup(mir, block_data, cleanup, true);
                }
            }
            TerminatorKind::FalseEdges { real_target, ref imaginary_targets } => {
                self.assert_iscleanup(mir, block, real_target, is_cleanup);
                for target in imaginary_targets {
                    self.assert_iscleanup(mir, block, *target, is_cleanup);
                }
            }
        }
    }

    fn assert_iscleanup(
        &mut self,
        mir: &Mir<'tcx>,
        ctxt: &fmt::Debug,
        bb: BasicBlock,
        iscleanuppad: bool,
    ) {
        if mir[bb].is_cleanup != iscleanuppad {
            span_mirbug!(
                self,
                ctxt,
                "cleanuppad mismatch: {:?} should be {:?}",
                bb,
                iscleanuppad
            );
        }
    }

    fn check_local(&mut self, mir: &Mir<'gcx>, local: Local, local_decl: &LocalDecl<'gcx>) {
        match mir.local_kind(local) {
            LocalKind::ReturnPointer | LocalKind::Arg => {
                // return values of normal functions are required to be
                // sized by typeck, but return values of ADT constructors are
                // not because we don't include a `Self: Sized` bounds on them.
                //
                // Unbound parts of arguments were never required to be Sized
                // - maybe we should make that a warning.
                return;
            }
            LocalKind::Var | LocalKind::Temp => {}
        }

        let span = local_decl.source_info.span;
        let ty = local_decl.ty;
        if !ty.is_sized(self.tcx().global_tcx(), self.param_env, span) {
            // in current MIR construction, all non-control-flow rvalue
            // expressions evaluate through `as_temp` or `into` a return
            // slot or local, so to find all unsized rvalues it is enough
            // to check all temps, return slots and locals.
            if let None = self.reported_errors.replace((ty, span)) {
                span_err!(
                    self.tcx().sess,
                    span,
                    E0161,
                    "cannot move a value of type {0}: the size of {0} \
                     cannot be statically determined",
                    ty
                );
            }
        }
    }

    fn typeck_mir(&mut self, mir: &Mir<'gcx>) {
        self.last_span = mir.span;
        debug!("run_on_mir: {:?}", mir.span);

        for (local, local_decl) in mir.local_decls.iter_enumerated() {
            self.check_local(mir, local, local_decl);
        }

        for (block, block_data) in mir.basic_blocks().iter_enumerated() {
            let mut location = Location {
                block,
                statement_index: 0,
            };
            for stmt in &block_data.statements {
                if stmt.source_info.span != DUMMY_SP {
                    self.last_span = stmt.source_info.span;
                }
                self.check_stmt(mir, stmt, location);
                location.statement_index += 1;
            }

            self.check_terminator(mir, block_data.terminator(), location);
            self.check_iscleanup(mir, block_data);
        }
    }


    fn normalize<T>(&mut self, value: &T, _location: Location) -> T
    where
        T: fmt::Debug + TypeFoldable<'tcx>,
    {
        self.fully_perform_op(|| {
            let mut selcx = traits::SelectionContext::new(self.infcx);
            let cause = traits::ObligationCause::misc(self.last_span, ast::CRATE_NODE_ID);
            let traits::Normalized { value, obligations } =
                traits::normalize(&mut selcx, self.param_env, cause, value);
            Ok(InferOk { value, obligations })
        }).unwrap()
    }
}

pub struct TypeckMir;

impl MirPass for TypeckMir {
    fn run_pass<'a, 'tcx>(&self, tcx: TyCtxt<'a, 'tcx, 'tcx>, src: MirSource, mir: &mut Mir<'tcx>) {
        let item_id = src.item_id();
        let def_id = tcx.hir.local_def_id(item_id);
        debug!("run_pass: {:?}", def_id);

        if tcx.sess.err_count() > 0 {
            // compiling a broken program can obviously result in a
            // broken MIR, so try not to report duplicate errors.
            return;
        }
        let param_env = tcx.param_env(def_id);
        tcx.infer_ctxt().enter(|infcx| {
            let mut checker = TypeChecker::new(&infcx, item_id, param_env);
            {
                let mut verifier = TypeVerifier::new(&mut checker, mir);
                verifier.visit_mir(mir);
                if verifier.errors_reported {
                    // don't do further checks to avoid ICEs
                    return;
                }
            }
            checker.typeck_mir(mir);
        });
    }
}
