use rustc::traits::{self};

use eval_context::EvalContext;
use memory::{MemoryPointer};
use value::{Value, PrimVal};

use rustc::hir::def_id::DefId;
use rustc::infer::InferCtxt;
use rustc::ty::subst::Substs;
use rustc::ty::{self, Ty};
use rustc::ty::layout::{Size, Align, HasDataLayout};
use rustc::traits::TraitEngine;
use syntax::codemap::{DUMMY_SP, Span};
use syntax::ast;

use error::{EvalResult, EvalError};


fn drain_fulfillment_cx_or_panic<'a, 'gcx, 'tcx, T>(ifcx: &InferCtxt<'a, 'gcx, 'tcx>,
                                                    span: Span,
                                                    fulfill_cx: &mut traits::FulfillmentContext<'tcx>,
                                                    result: &T)
                                                    -> T::Lifted
    where T: ty::TypeFoldable<'tcx> + ty::Lift<'gcx>
{
    debug!("drain_fulfillment_cx_or_panic()");

    // In principle, we only need to do this so long as `result`
    // contains unbound type parameters. It could be a slight
    // optimization to stop iterating early.
    match fulfill_cx.select_all_or_error(ifcx) {
        Ok(()) => { }
        Err(errors) => {
            span_bug!(span, "Encountered errors `{:?}` resolving bounds after type-checking",
                      errors);
            }
        }

    let result = ifcx.resolve_type_vars_if_possible(result);
    let result = ifcx.tcx.erase_regions(&result);

    match ifcx.tcx.lift_to_global(&result) {
        Some(result) => result,
        None => {
            span_bug!(span, "Uninferred types/regions in `{:?}`", result);
        }
    }
}


impl<'a, 'tcx> EvalContext<'a, 'tcx> {

    pub(crate) fn fulfill_obligation(&self, trait_ref: ty::PolyTraitRef<'tcx>) -> traits::Vtable<'tcx, ()> {
        // Do the initial selection for the obligation. This yields the shallow result we are
        // looking for -- that is, what specific impl.
        self.tcx.infer_ctxt().enter(|infcx| {
            let mut selcx = traits::SelectionContext::new(&infcx);

            let obligation = traits::Obligation::new(
                traits::ObligationCause::misc(DUMMY_SP, ast::DUMMY_NODE_ID),
                ty::ParamEnv::empty(),
                trait_ref.to_poly_trait_predicate(),
            );
            let selection = selcx.select(&obligation).unwrap().unwrap();

            // Currently, we use a fulfillment context to completely resolve all nested obligations.
            // This is because they can inform the inference of the impl's type parameters.
            let mut fulfill_cx = traits::FulfillmentContext::new();
            let vtable = selection.map(|predicate| {
                fulfill_cx.register_predicate_obligation(&infcx, predicate);
            });
            drain_fulfillment_cx_or_panic(&infcx, DUMMY_SP, &mut fulfill_cx, &vtable)
        })
    }

    /// Creates a dynamic vtable for the given type and vtable origin. This is used only for
    /// objects.
    ///
    /// The `trait_ref` encodes the erased self type. Hence if we are
    /// making an object `Foo<Trait>` from a value of type `Foo<T>`, then
    /// `trait_ref` would map `T:Trait`.
    pub fn get_vtable(&mut self, ty: Ty<'tcx>, trait_ref: ty::PolyTraitRef<'tcx>) -> EvalResult<'tcx, MemoryPointer> {
        debug!("get_vtable(trait_ref={:?})", trait_ref);

        let size = self.type_size(trait_ref.self_ty())?.expect("can't create a vtable for an unsized type");
        let align = self.type_align(trait_ref.self_ty())?;

        let ptr_size = self.memory.pointer_size();
        let methods = self.tcx.vtable_methods(trait_ref);
        let vtable = self.memory.allocate(ptr_size * (3 + methods.iter().count() as u64), ptr_size)?;

        let drop = ::eval_context::resolve_drop_in_place(self.tcx, ty);
        let drop = self.memory.create_fn_alloc(drop);
        self.memory.write_ptr(vtable, drop)?;

        self.memory.write_usize(vtable.offset(ptr_size, self.memory.layout)?, size)?;
        self.memory.write_usize(vtable.offset(ptr_size * 2, self.memory.layout)?, align)?;

        for (i, method) in self.tcx.vtable_methods(trait_ref).iter().enumerate() {
            if let Some((def_id, substs)) = *method {
                let instance = self.resolve(def_id, substs)?;
                let fn_ptr = self.memory.create_fn_alloc(instance);
                self.memory.write_ptr(vtable.offset(ptr_size * (3 + i as u64), self.memory.layout)?, fn_ptr)?;
            }
        }

        self.memory.mark_static_initalized(vtable.alloc_id, false)?;

        Ok(vtable)
    }

    pub fn read_drop_type_from_vtable(&mut self, vtable: MemoryPointer) -> EvalResult<'tcx, Option<ty::Instance<'tcx>>> {
        // we don't care about the pointee type, we just want a pointer
        let np = self.tcx.mk_nil_ptr();
        let drop_fn = match self.read_ptr(vtable, np)? {
            // some values don't need to call a drop impl, so the value is null
            Value::ByVal(PrimVal::Bytes(0)) => return Ok(None),
            Value::ByVal(PrimVal::Ptr(drop_fn)) => drop_fn,
            _ => return Err(EvalError::ReadBytesAsPointer),
        };

        self.memory.get_fn(drop_fn).map(Some)
    }

    pub fn read_size_and_align_from_vtable(
        &self,
        vtable: MemoryPointer,
    ) -> EvalResult<'tcx, (Size, Align)> {
        let pointer_size = self.memory.pointer_size();
        let size = self.memory.read_usize(vtable.offset(pointer_size, self.data_layout())?)?;
        let align = self.memory.read_usize(vtable.offset(pointer_size * 2, self.data_layout())?)?;
        Ok((Size::from_bytes(size), Align::from_bytes(align, align).unwrap()))
    }

    pub(crate) fn resolve_associated_const(
        &self,
        def_id: DefId,
        substs: &'tcx Substs<'tcx>,
    ) -> ty::Instance<'tcx> {
        if let Some(trait_id) = self.tcx.trait_of_item(def_id) {
            let trait_ref = ty::Binder(ty::TraitRef::new(trait_id, substs));
            let vtable = self.fulfill_obligation(trait_ref);
            if let traits::VtableImpl(vtable_impl) = vtable {
                let name = self.tcx.item_name(def_id);
                let assoc_const_opt = self.tcx.associated_items(vtable_impl.impl_def_id)
                    .find(|item| item.kind == ty::AssociatedKind::Const && item.name == name);
                if let Some(assoc_const) = assoc_const_opt {
                    return ty::Instance::new(assoc_const.def_id, vtable_impl.substs);
                }
            }
        }
        ty::Instance::new(def_id, substs)
    }
}
