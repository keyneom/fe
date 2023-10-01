use std::collections::{BTreeMap, BTreeSet};

use hir::{
    hir_def::{scope_graph::ScopeId, IngotId, Trait, TraitRef, TypeId, TypeKind, WherePredicate},
    visitor::prelude::*,
};
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::function::Configuration;

use crate::{ty::trait_::TraitEnv, HirAnalysisDb};

use super::{
    diagnostics::{TraitLowerDiag, TyDiagCollection},
    trait_::{TraitDef, TraitInstId},
    trait_lower::lower_trait_ref,
    ty_def::{Subst, TyId},
};

/// Collects super traits, and verify there are no cyclic in
/// the super traits relationship.
///
/// This method is implemented independently from [`collect_constraints`] method
/// because
/// 1. the cycle check should be performed before collecting other constraints
///    to make sure the constraint simplification terminates.
/// 2. `collect_constraints` function needs to care about another cycle which is
///    caused by constraints simplification.
/// 3. we want to emit better error messages for cyclic super traits.
///
/// NOTE: This methods returns all super traits without any simplification.
#[salsa::tracked(return_ref, recovery_fn = recover_collect_super_traits)]
pub(crate) fn collect_super_traits(
    db: &dyn HirAnalysisDb,
    trait_: TraitDef,
) -> (Vec<TraitInstId>, Vec<TyDiagCollection>) {
    let mut collector = SuperTraitCollector::new(db, trait_, FxHashSet::default());
    let (insts, diags) = collector.finalize();

    // Check for cycles.
    for inst in &insts {
        collect_super_traits(db, inst.def(db));
    }

    (insts, diags)
}

#[salsa::tracked(return_ref)]
pub(crate) fn super_trait_insts(
    db: &dyn HirAnalysisDb,
    trait_inst: TraitInstId,
) -> Vec<TraitInstId> {
    let trait_def = trait_inst.def(db);
    let (super_traits, _) = collect_super_traits(db, trait_def);
    let mut subst = trait_inst.subst_table(db);

    super_traits
        .iter()
        .map(|trait_| trait_.apply_subst(db, &mut subst))
        .collect()
}

#[salsa::tracked(return_ref)]
pub(crate) fn compute_super_assumptions(
    db: &dyn HirAnalysisDb,
    assumptions: AssumptionListId,
) -> AssumptionListId {
    let ingot = assumptions.ingot(db);
    let trait_env = TraitEnv::new(db, ingot);
    let mut super_assumptions = BTreeMap::new();

    for (ty, insts) in assumptions.predicates(db) {
        let super_insts = insts
            .iter()
            .flat_map(|inst| super_trait_insts(db, *inst).iter().copied());
        super_assumptions.insert(*ty, super_insts.collect());
    }

    AssumptionListId::new(db, super_assumptions, ingot)
}

#[salsa::interned]
pub(crate) struct PredicateId {
    pub(super) ty: TyId,
    pub(super) trait_: TraitInstId,
}

#[salsa::interned]
pub(crate) struct PredicateListId {
    #[return_ref]
    pub(super) predicates: BTreeMap<TyId, BTreeSet<TraitInstId>>,
    pub(super) ingot: IngotId,
}

pub(super) type AssumptionListId = PredicateListId;
pub(super) type ConstraintListId = PredicateListId;

impl PredicateListId {
    pub(super) fn does_satisfy(self, db: &dyn HirAnalysisDb, predicate: PredicateId) -> bool {
        let trait_ = predicate.trait_(db);
        let ty = predicate.ty(db);

        let Some(insts) = self.predicates(db).get(&ty) else {
            return false;
        };

        insts.contains(&trait_)
    }
}

impl PredicateId {
    pub fn apply_subst<S: Subst>(self, db: &dyn HirAnalysisDb, subst: &mut S) -> Self {
        let ty = self.ty(db).apply_subst(db, subst);
        let trait_ = self.trait_(db).apply_subst(db, subst);
        Self::new(db, ty, trait_)
    }
}

pub(crate) fn recover_collect_super_traits(
    db: &dyn HirAnalysisDb,
    cycle: &salsa::Cycle,
    trait_: TraitDef,
) -> (Vec<TraitInstId>, Vec<TyDiagCollection>) {
    let participants: FxHashSet<_> = cycle
        .participant_keys()
        .map(|key| {
            let trait_ = collect_super_traits::key_from_id(key.key_index());
            trait_.trait_(db)
        })
        .collect();

    let mut collector = SuperTraitCollector::new(db, trait_, participants);
    collector.finalize()
}

struct SuperTraitCollector<'db> {
    db: &'db dyn HirAnalysisDb,
    trait_: TraitDef,
    super_traits: Vec<TraitInstId>,
    diags: Vec<TyDiagCollection>,
    cycle: FxHashSet<Trait>,
    scope: ScopeId,
}

impl<'db> SuperTraitCollector<'db> {
    fn new(db: &'db dyn HirAnalysisDb, trait_: TraitDef, cycle: FxHashSet<Trait>) -> Self {
        Self {
            db,
            trait_,
            super_traits: vec![],
            diags: vec![],
            cycle,
            scope: trait_.trait_(db).scope(),
        }
    }

    fn finalize(mut self) -> (Vec<TraitInstId>, Vec<TyDiagCollection>) {
        let hir_trait = self.trait_.trait_(self.db);
        let mut visitor_ctxt = VisitorCtxt::with_trait(self.db.as_hir_db(), hir_trait);
        self.visit_trait(&mut visitor_ctxt, hir_trait);

        (self.super_traits, self.diags)
    }
}

impl<'db> Visitor for SuperTraitCollector<'db> {
    fn visit_trait_ref(
        &mut self,
        ctxt: &mut VisitorCtxt<'_, LazyTraitRefSpan>,
        trait_ref: TraitRef,
    ) {
        let span = ctxt.span().unwrap();
        let (trait_inst, diags) = lower_trait_ref(self.db, trait_ref, span, self.scope);
        if !diags.is_empty() {
            self.diags.extend(diags);
            return;
        }

        let Some(trait_inst) = trait_inst else {
            return;
        };

        if self
            .cycle
            .contains(&trait_inst.def(self.db).trait_(self.db))
        {
            let span = ctxt.span().unwrap().into();
            self.diags
                .push(TraitLowerDiag::CyclicSuperTraits(span).into());
            return;
        }

        self.super_traits.push(trait_inst);
    }

    fn visit_where_predicate(
        &mut self,
        ctxt: &mut VisitorCtxt<'_, LazyWherePredicateSpan>,
        pred: &WherePredicate,
    ) {
        match pred.ty.to_opt().map(|ty| ty.data(self.db.as_hir_db())) {
            // We just want to check super traits, so we don't care about other type constraints.
            Some(TypeKind::SelfType(args)) if args.is_empty(self.db.as_hir_db()) => {
                walk_where_predicate(self, ctxt, pred);
            }
            _ => (),
        }
    }

    fn visit_item(&mut self, _: &mut VisitorCtxt<'_, LazyItemSpan>, _: hir::hir_def::ItemKind) {
        // We don't want to visit nested items in the trait.
    }
}
