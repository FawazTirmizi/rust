// Based on rust-lang/rust 1.52.0-nightly (25c15cdbe 2021-04-22)
// https://github.com/rust-lang/rust/blob/25c15cdbe/compiler/rustc_mir_build/src/thir/pattern/usefulness.rs

use std::{cell::RefCell, iter::FromIterator, ops::Index, sync::Arc};

use hir_def::{body::Body, expr::ExprId, HasModule, ModuleId};
use la_arena::Arena;
use once_cell::unsync::OnceCell;
use rustc_hash::FxHashMap;
use smallvec::{smallvec, SmallVec};

use crate::{db::HirDatabase, InferenceResult, Interner, Ty};

use super::{
    deconstruct_pat::{Constructor, Fields, SplitWildcard},
    Pat, PatId, PatKind, PatternFoldable, PatternFolder,
};

use self::{
    helper::{Captures, PatIdExt},
    Usefulness::*,
    WitnessPreference::*,
};

pub(crate) struct MatchCheckCtx<'a> {
    pub(crate) module: ModuleId,
    pub(crate) match_expr: ExprId,
    pub(crate) body: Arc<Body>,
    pub(crate) infer: &'a InferenceResult,
    pub(crate) db: &'a dyn HirDatabase,
    /// Lowered patterns from self.body.pats plus generated by the check.
    pub(crate) pattern_arena: &'a RefCell<PatternArena>,
}

impl<'a> MatchCheckCtx<'a> {
    pub(super) fn is_uninhabited(&self, ty: &Ty) -> bool {
        // FIXME(iDawer) implement exhaustive_patterns feature. More info in:
        // Tracking issue for RFC 1872: exhaustive_patterns feature https://github.com/rust-lang/rust/issues/51085
        false
    }

    /// Returns whether the given type is an enum from another crate declared `#[non_exhaustive]`.
    pub(super) fn is_foreign_non_exhaustive_enum(&self, enum_id: hir_def::EnumId) -> bool {
        let has_non_exhaustive_attr =
            self.db.attrs(enum_id.into()).by_key("non_exhaustive").exists();
        let is_local =
            hir_def::AdtId::from(enum_id).module(self.db.upcast()).krate() == self.module.krate();
        has_non_exhaustive_attr && !is_local
    }

    // Rust feature described as "Allows exhaustive pattern matching on types that contain uninhabited types."
    pub(super) fn feature_exhaustive_patterns(&self) -> bool {
        // TODO
        false
    }

    pub(super) fn alloc_pat(&self, pat: Pat) -> PatId {
        self.pattern_arena.borrow_mut().alloc(pat)
    }

    /// Get type of a pattern. Handles expanded patterns.
    pub(super) fn type_of(&self, pat: PatId) -> Ty {
        self.pattern_arena.borrow()[pat].ty.clone()
    }
}

#[derive(Copy, Clone)]
pub(super) struct PatCtxt<'a> {
    pub(super) cx: &'a MatchCheckCtx<'a>,
    /// Type of the current column under investigation.
    pub(super) ty: &'a Ty,
    /// Whether the current pattern is the whole pattern as found in a match arm, or if it's a
    /// subpattern.
    pub(super) is_top_level: bool,
}

pub(crate) fn expand_pattern(pat: Pat) -> Pat {
    LiteralExpander.fold_pattern(&pat)
}

struct LiteralExpander;

impl PatternFolder for LiteralExpander {
    fn fold_pattern(&mut self, pat: &Pat) -> Pat {
        match (pat.ty.kind(&Interner), pat.kind.as_ref()) {
            (_, PatKind::Binding { subpattern: Some(s), .. }) => s.fold_with(self),
            _ => pat.super_fold_with(self),
        }
    }
}

impl Pat {
    fn is_wildcard(&self) -> bool {
        matches!(*self.kind, PatKind::Binding { subpattern: None, .. } | PatKind::Wild)
    }
}

impl PatIdExt for PatId {
    fn is_or_pat(self, cx: &MatchCheckCtx<'_>) -> bool {
        matches!(*cx.pattern_arena.borrow()[self].kind, PatKind::Or { .. })
    }

    /// Recursively expand this pattern into its subpatterns. Only useful for or-patterns.
    fn expand_or_pat(self, cx: &MatchCheckCtx<'_>) -> Vec<Self> {
        fn expand(pat: PatId, vec: &mut Vec<PatId>, mut pat_arena: &mut PatternArena) {
            if let PatKind::Or { pats } = pat_arena[pat].kind.as_ref() {
                let pats = pats.clone();
                for pat in pats {
                    // TODO(iDawer): Ugh, I want to go back to references (PatId -> &Pat)
                    let pat = pat_arena.alloc(pat.clone());
                    expand(pat, vec, pat_arena);
                }
            } else {
                vec.push(pat)
            }
        }

        let mut pat_arena = cx.pattern_arena.borrow_mut();
        let mut pats = Vec::new();
        expand(self, &mut pats, &mut pat_arena);
        pats
    }
}

/// A row of a matrix. Rows of len 1 are very common, which is why `SmallVec[_; 2]`
/// works well.
#[derive(Clone)]
pub(super) struct PatStack {
    pats: SmallVec<[PatId; 2]>,
    /// Cache for the constructor of the head
    head_ctor: OnceCell<Constructor>,
}

impl PatStack {
    fn from_pattern(pat: PatId) -> Self {
        Self::from_vec(smallvec![pat])
    }

    fn from_vec(vec: SmallVec<[PatId; 2]>) -> Self {
        PatStack { pats: vec, head_ctor: OnceCell::new() }
    }

    fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    fn len(&self) -> usize {
        self.pats.len()
    }

    fn head(&self) -> PatId {
        self.pats[0]
    }

    #[inline]
    fn head_ctor(&self, cx: &MatchCheckCtx<'_>) -> &Constructor {
        self.head_ctor.get_or_init(|| Constructor::from_pat(cx, self.head()))
    }

    fn iter(&self) -> impl Iterator<Item = PatId> + '_ {
        self.pats.iter().copied()
    }

    // Recursively expand the first pattern into its subpatterns. Only useful if the pattern is an
    // or-pattern. Panics if `self` is empty.
    fn expand_or_pat(&self, cx: &MatchCheckCtx<'_>) -> impl Iterator<Item = PatStack> + '_ {
        self.head().expand_or_pat(cx).into_iter().map(move |pat| {
            let mut new_patstack = PatStack::from_pattern(pat);
            new_patstack.pats.extend_from_slice(&self.pats[1..]);
            new_patstack
        })
    }

    /// This computes `S(self.head_ctor(), self)`. See top of the file for explanations.
    ///
    /// Structure patterns with a partial wild pattern (Foo { a: 42, .. }) have their missing
    /// fields filled with wild patterns.
    ///
    /// This is roughly the inverse of `Constructor::apply`.
    fn pop_head_constructor(
        &self,
        ctor_wild_subpatterns: &Fields,
        cx: &MatchCheckCtx<'_>,
    ) -> PatStack {
        // We pop the head pattern and push the new fields extracted from the arguments of
        // `self.head()`.
        let mut new_fields =
            ctor_wild_subpatterns.replace_with_pattern_arguments(self.head(), cx).into_patterns();
        new_fields.extend_from_slice(&self.pats[1..]);
        PatStack::from_vec(new_fields)
    }
}

impl Default for PatStack {
    fn default() -> Self {
        Self::from_vec(smallvec![])
    }
}

impl PartialEq for PatStack {
    fn eq(&self, other: &Self) -> bool {
        self.pats == other.pats
    }
}

impl FromIterator<PatId> for PatStack {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = PatId>,
    {
        Self::from_vec(iter.into_iter().collect())
    }
}

/// A 2D matrix.
#[derive(Clone)]
pub(super) struct Matrix {
    patterns: Vec<PatStack>,
}

impl Matrix {
    fn empty() -> Self {
        Matrix { patterns: vec![] }
    }

    /// Number of columns of this matrix. `None` is the matrix is empty.
    pub(super) fn column_count(&self) -> Option<usize> {
        self.patterns.get(0).map(|r| r.len())
    }

    /// Pushes a new row to the matrix. If the row starts with an or-pattern, this recursively
    /// expands it.
    fn push(&mut self, row: PatStack, cx: &MatchCheckCtx<'_>) {
        if !row.is_empty() && row.head().is_or_pat(cx) {
            for row in row.expand_or_pat(cx) {
                self.patterns.push(row);
            }
        } else {
            self.patterns.push(row);
        }
    }

    /// Iterate over the first component of each row
    fn heads(&self) -> impl Iterator<Item = PatId> + '_ {
        self.patterns.iter().map(|r| r.head())
    }

    /// Iterate over the first constructor of each row.
    fn head_ctors<'a>(
        &'a self,
        cx: &'a MatchCheckCtx<'_>,
    ) -> impl Iterator<Item = &'a Constructor> + Clone {
        self.patterns.iter().map(move |r| r.head_ctor(cx))
    }

    /// This computes `S(constructor, self)`. See top of the file for explanations.
    fn specialize_constructor(
        &self,
        pcx: PatCtxt<'_>,
        ctor: &Constructor,
        ctor_wild_subpatterns: &Fields,
    ) -> Matrix {
        let rows = self
            .patterns
            .iter()
            .filter(|r| ctor.is_covered_by(pcx, r.head_ctor(pcx.cx)))
            .map(|r| r.pop_head_constructor(ctor_wild_subpatterns, pcx.cx));
        Matrix::from_iter(rows, pcx.cx)
    }

    fn from_iter(rows: impl IntoIterator<Item = PatStack>, cx: &MatchCheckCtx<'_>) -> Matrix {
        let mut matrix = Matrix::empty();
        for x in rows {
            // Using `push` ensures we correctly expand or-patterns.
            matrix.push(x, cx);
        }
        matrix
    }
}

/// Given a pattern or a pattern-stack, this struct captures a set of its subpatterns. We use that
/// to track reachable sub-patterns arising from or-patterns. In the absence of or-patterns this
/// will always be either `Empty` (the whole pattern is unreachable) or `Full` (the whole pattern
/// is reachable). When there are or-patterns, some subpatterns may be reachable while others
/// aren't. In this case the whole pattern still counts as reachable, but we will lint the
/// unreachable subpatterns.
///
/// This supports a limited set of operations, so not all possible sets of subpatterns can be
/// represented. That's ok, we only want the ones that make sense for our usage.
///
/// What we're doing is illustrated by this:
/// ```
/// match (true, 0) {
///     (true, 0) => {}
///     (_, 1) => {}
///     (true | false, 0 | 1) => {}
/// }
/// ```
/// When we try the alternatives of the `true | false` or-pattern, the last `0` is reachable in the
/// `false` alternative but not the `true`. So overall it is reachable. By contrast, the last `1`
/// is not reachable in either alternative, so we want to signal this to the user.
/// Therefore we take the union of sets of reachable patterns coming from different alternatives in
/// order to figure out which subpatterns are overall reachable.
///
/// Invariant: we try to construct the smallest representation we can. In particular if
/// `self.is_empty()` we ensure that `self` is `Empty`, and same with `Full`. This is not important
/// for correctness currently.
#[derive(Debug, Clone)]
enum SubPatSet {
    /// The empty set. This means the pattern is unreachable.
    Empty,
    /// The set containing the full pattern.
    Full,
    /// If the pattern is a pattern with a constructor or a pattern-stack, we store a set for each
    /// of its subpatterns. Missing entries in the map are implicitly full, because that's the
    /// common case.
    Seq { subpats: FxHashMap<usize, SubPatSet> },
    /// If the pattern is an or-pattern, we store a set for each of its alternatives. Missing
    /// entries in the map are implicitly empty. Note: we always flatten nested or-patterns.
    Alt {
        subpats: FxHashMap<usize, SubPatSet>,
        /// Counts the total number of alternatives in the pattern
        alt_count: usize,
        /// We keep the pattern around to retrieve spans.
        pat: PatId,
    },
}

impl SubPatSet {
    fn full() -> Self {
        SubPatSet::Full
    }

    fn empty() -> Self {
        SubPatSet::Empty
    }

    fn is_empty(&self) -> bool {
        match self {
            SubPatSet::Empty => true,
            SubPatSet::Full => false,
            // If any subpattern in a sequence is unreachable, the whole pattern is unreachable.
            SubPatSet::Seq { subpats } => subpats.values().any(|set| set.is_empty()),
            // An or-pattern is reachable if any of its alternatives is.
            SubPatSet::Alt { subpats, .. } => subpats.values().all(|set| set.is_empty()),
        }
    }

    fn is_full(&self) -> bool {
        match self {
            SubPatSet::Empty => false,
            SubPatSet::Full => true,
            // The whole pattern is reachable only when all its alternatives are.
            SubPatSet::Seq { subpats } => subpats.values().all(|sub_set| sub_set.is_full()),
            // The whole or-pattern is reachable only when all its alternatives are.
            SubPatSet::Alt { subpats, alt_count, .. } => {
                subpats.len() == *alt_count && subpats.values().all(|set| set.is_full())
            }
        }
    }

    /// Union `self` with `other`, mutating `self`.
    fn union(&mut self, other: Self) {
        use SubPatSet::*;
        // Union with full stays full; union with empty changes nothing.
        if self.is_full() || other.is_empty() {
            return;
        } else if self.is_empty() {
            *self = other;
            return;
        } else if other.is_full() {
            *self = Full;
            return;
        }

        match (&mut *self, other) {
            (Seq { subpats: s_set }, Seq { subpats: mut o_set }) => {
                s_set.retain(|i, s_sub_set| {
                    // Missing entries count as full.
                    let o_sub_set = o_set.remove(&i).unwrap_or(Full);
                    s_sub_set.union(o_sub_set);
                    // We drop full entries.
                    !s_sub_set.is_full()
                });
                // Everything left in `o_set` is missing from `s_set`, i.e. counts as full. Since
                // unioning with full returns full, we can drop those entries.
            }
            (Alt { subpats: s_set, .. }, Alt { subpats: mut o_set, .. }) => {
                s_set.retain(|i, s_sub_set| {
                    // Missing entries count as empty.
                    let o_sub_set = o_set.remove(&i).unwrap_or(Empty);
                    s_sub_set.union(o_sub_set);
                    // We drop empty entries.
                    !s_sub_set.is_empty()
                });
                // Everything left in `o_set` is missing from `s_set`, i.e. counts as empty. Since
                // unioning with empty changes nothing, we can take those entries as is.
                s_set.extend(o_set);
            }
            _ => panic!("bug"),
        }

        if self.is_full() {
            *self = Full;
        }
    }

    /// Returns a list of the unreachable subpatterns. If `self` is empty (i.e. the
    /// whole pattern is unreachable) we return `None`.
    fn list_unreachable_subpatterns(&self, cx: &MatchCheckCtx<'_>) -> Option<Vec<PatId>> {
        /// Panics if `set.is_empty()`.
        fn fill_subpats(
            set: &SubPatSet,
            unreachable_pats: &mut Vec<PatId>,
            cx: &MatchCheckCtx<'_>,
        ) {
            match set {
                SubPatSet::Empty => panic!("bug"),
                SubPatSet::Full => {}
                SubPatSet::Seq { subpats } => {
                    for (_, sub_set) in subpats {
                        fill_subpats(sub_set, unreachable_pats, cx);
                    }
                }
                SubPatSet::Alt { subpats, pat, alt_count, .. } => {
                    let expanded = pat.expand_or_pat(cx);
                    for i in 0..*alt_count {
                        let sub_set = subpats.get(&i).unwrap_or(&SubPatSet::Empty);
                        if sub_set.is_empty() {
                            // Found a unreachable subpattern.
                            unreachable_pats.push(expanded[i]);
                        } else {
                            fill_subpats(sub_set, unreachable_pats, cx);
                        }
                    }
                }
            }
        }

        if self.is_empty() {
            return None;
        }
        if self.is_full() {
            // No subpatterns are unreachable.
            return Some(Vec::new());
        }
        let mut unreachable_pats = Vec::new();
        fill_subpats(self, &mut unreachable_pats, cx);
        Some(unreachable_pats)
    }

    /// When `self` refers to a patstack that was obtained from specialization, after running
    /// `unspecialize` it will refer to the original patstack before specialization.
    fn unspecialize(self, arity: usize) -> Self {
        use SubPatSet::*;
        match self {
            Full => Full,
            Empty => Empty,
            Seq { subpats } => {
                // We gather the first `arity` subpatterns together and shift the remaining ones.
                let mut new_subpats = FxHashMap::default();
                let mut new_subpats_first_col = FxHashMap::default();
                for (i, sub_set) in subpats {
                    if i < arity {
                        // The first `arity` indices are now part of the pattern in the first
                        // column.
                        new_subpats_first_col.insert(i, sub_set);
                    } else {
                        // Indices after `arity` are simply shifted
                        new_subpats.insert(i - arity + 1, sub_set);
                    }
                }
                // If `new_subpats_first_col` has no entries it counts as full, so we can omit it.
                if !new_subpats_first_col.is_empty() {
                    new_subpats.insert(0, Seq { subpats: new_subpats_first_col });
                }
                Seq { subpats: new_subpats }
            }
            Alt { .. } => panic!("bug"),
        }
    }

    /// When `self` refers to a patstack that was obtained from splitting an or-pattern, after
    /// running `unspecialize` it will refer to the original patstack before splitting.
    ///
    /// For example:
    /// ```
    /// match Some(true) {
    ///     Some(true) => {}
    ///     None | Some(true | false) => {}
    /// }
    /// ```
    /// Here `None` would return the full set and `Some(true | false)` would return the set
    /// containing `false`. After `unsplit_or_pat`, we want the set to contain `None` and `false`.
    /// This is what this function does.
    fn unsplit_or_pat(mut self, alt_id: usize, alt_count: usize, pat: PatId) -> Self {
        use SubPatSet::*;
        if self.is_empty() {
            return Empty;
        }

        // Subpatterns coming from inside the or-pattern alternative itself, e.g. in `None | Some(0
        // | 1)`.
        let set_first_col = match &mut self {
            Full => Full,
            Seq { subpats } => subpats.remove(&0).unwrap_or(Full),
            Empty => unreachable!(),
            Alt { .. } => panic!("bug"), // `self` is a patstack
        };
        let mut subpats_first_col = FxHashMap::default();
        subpats_first_col.insert(alt_id, set_first_col);
        let set_first_col = Alt { subpats: subpats_first_col, pat, alt_count };

        let mut subpats = match self {
            Full => FxHashMap::default(),
            Seq { subpats } => subpats,
            Empty => unreachable!(),
            Alt { .. } => panic!("bug"), // `self` is a patstack
        };
        subpats.insert(0, set_first_col);
        Seq { subpats }
    }
}

/// This carries the results of computing usefulness, as described at the top of the file. When
/// checking usefulness of a match branch, we use the `NoWitnesses` variant, which also keeps track
/// of potential unreachable sub-patterns (in the presence of or-patterns). When checking
/// exhaustiveness of a whole match, we use the `WithWitnesses` variant, which carries a list of
/// witnesses of non-exhaustiveness when there are any.
/// Which variant to use is dictated by `WitnessPreference`.
#[derive(Clone, Debug)]
enum Usefulness {
    /// Carries a set of subpatterns that have been found to be reachable. If empty, this indicates
    /// the whole pattern is unreachable. If not, this indicates that the pattern is reachable but
    /// that some sub-patterns may be unreachable (due to or-patterns). In the absence of
    /// or-patterns this will always be either `Empty` (the whole pattern is unreachable) or `Full`
    /// (the whole pattern is reachable).
    NoWitnesses(SubPatSet),
    /// Carries a list of witnesses of non-exhaustiveness. If empty, indicates that the whole
    /// pattern is unreachable.
    WithWitnesses(Vec<Witness>),
}

impl Usefulness {
    fn new_useful(preference: WitnessPreference) -> Self {
        match preference {
            ConstructWitness => WithWitnesses(vec![Witness(vec![])]),
            LeaveOutWitness => NoWitnesses(SubPatSet::full()),
        }
    }
    fn new_not_useful(preference: WitnessPreference) -> Self {
        match preference {
            ConstructWitness => WithWitnesses(vec![]),
            LeaveOutWitness => NoWitnesses(SubPatSet::empty()),
        }
    }

    /// Combine usefulnesses from two branches. This is an associative operation.
    fn extend(&mut self, other: Self) {
        match (&mut *self, other) {
            (WithWitnesses(_), WithWitnesses(o)) if o.is_empty() => {}
            (WithWitnesses(s), WithWitnesses(o)) if s.is_empty() => *self = WithWitnesses(o),
            (WithWitnesses(s), WithWitnesses(o)) => s.extend(o),
            (NoWitnesses(s), NoWitnesses(o)) => s.union(o),
            _ => unreachable!(),
        }
    }

    /// When trying several branches and each returns a `Usefulness`, we need to combine the
    /// results together.
    fn merge(pref: WitnessPreference, usefulnesses: impl Iterator<Item = Self>) -> Self {
        let mut ret = Self::new_not_useful(pref);
        for u in usefulnesses {
            ret.extend(u);
            if let NoWitnesses(subpats) = &ret {
                if subpats.is_full() {
                    // Once we reach the full set, more unions won't change the result.
                    return ret;
                }
            }
        }
        ret
    }

    /// After calculating the usefulness for a branch of an or-pattern, call this to make this
    /// usefulness mergeable with those from the other branches.
    fn unsplit_or_pat(self, alt_id: usize, alt_count: usize, pat: PatId) -> Self {
        match self {
            NoWitnesses(subpats) => NoWitnesses(subpats.unsplit_or_pat(alt_id, alt_count, pat)),
            WithWitnesses(_) => panic!("bug"),
        }
    }

    /// After calculating usefulness after a specialization, call this to recontruct a usefulness
    /// that makes sense for the matrix pre-specialization. This new usefulness can then be merged
    /// with the results of specializing with the other constructors.
    fn apply_constructor(
        self,
        pcx: PatCtxt<'_>,
        matrix: &Matrix,
        ctor: &Constructor,
        ctor_wild_subpatterns: &Fields,
    ) -> Self {
        match self {
            WithWitnesses(witnesses) if witnesses.is_empty() => WithWitnesses(witnesses),
            WithWitnesses(witnesses) => {
                let new_witnesses = if matches!(ctor, Constructor::Missing) {
                    let mut split_wildcard = SplitWildcard::new(pcx);
                    split_wildcard.split(pcx, matrix.head_ctors(pcx.cx));
                    // Construct for each missing constructor a "wild" version of this
                    // constructor, that matches everything that can be built with
                    // it. For example, if `ctor` is a `Constructor::Variant` for
                    // `Option::Some`, we get the pattern `Some(_)`.
                    let new_patterns: Vec<_> = split_wildcard
                        .iter_missing(pcx)
                        .map(|missing_ctor| {
                            Fields::wildcards(pcx, missing_ctor).apply(pcx, missing_ctor)
                        })
                        .collect();
                    witnesses
                        .into_iter()
                        .flat_map(|witness| {
                            new_patterns.iter().map(move |pat| {
                                let mut witness = witness.clone();
                                witness.0.push(pat.clone());
                                witness
                            })
                        })
                        .collect()
                } else {
                    witnesses
                        .into_iter()
                        .map(|witness| witness.apply_constructor(pcx, &ctor, ctor_wild_subpatterns))
                        .collect()
                };
                WithWitnesses(new_witnesses)
            }
            NoWitnesses(subpats) => NoWitnesses(subpats.unspecialize(ctor_wild_subpatterns.len())),
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum WitnessPreference {
    ConstructWitness,
    LeaveOutWitness,
}

/// A witness of non-exhaustiveness for error reporting, represented
/// as a list of patterns (in reverse order of construction) with
/// wildcards inside to represent elements that can take any inhabitant
/// of the type as a value.
///
/// A witness against a list of patterns should have the same types
/// and length as the pattern matched against. Because Rust `match`
/// is always against a single pattern, at the end the witness will
/// have length 1, but in the middle of the algorithm, it can contain
/// multiple patterns.
///
/// For example, if we are constructing a witness for the match against
///
/// ```
/// struct Pair(Option<(u32, u32)>, bool);
///
/// match (p: Pair) {
///    Pair(None, _) => {}
///    Pair(_, false) => {}
/// }
/// ```
///
/// We'll perform the following steps:
/// 1. Start with an empty witness
///     `Witness(vec![])`
/// 2. Push a witness `true` against the `false`
///     `Witness(vec![true])`
/// 3. Push a witness `Some(_)` against the `None`
///     `Witness(vec![true, Some(_)])`
/// 4. Apply the `Pair` constructor to the witnesses
///     `Witness(vec![Pair(Some(_), true)])`
///
/// The final `Pair(Some(_), true)` is then the resulting witness.
#[derive(Clone, Debug)]
pub(crate) struct Witness(Vec<Pat>);

impl Witness {
    /// Asserts that the witness contains a single pattern, and returns it.
    fn single_pattern(self) -> Pat {
        assert_eq!(self.0.len(), 1);
        self.0.into_iter().next().unwrap()
    }

    /// Constructs a partial witness for a pattern given a list of
    /// patterns expanded by the specialization step.
    ///
    /// When a pattern P is discovered to be useful, this function is used bottom-up
    /// to reconstruct a complete witness, e.g., a pattern P' that covers a subset
    /// of values, V, where each value in that set is not covered by any previously
    /// used patterns and is covered by the pattern P'. Examples:
    ///
    /// left_ty: tuple of 3 elements
    /// pats: [10, 20, _]           => (10, 20, _)
    ///
    /// left_ty: struct X { a: (bool, &'static str), b: usize}
    /// pats: [(false, "foo"), 42]  => X { a: (false, "foo"), b: 42 }
    fn apply_constructor(
        mut self,
        pcx: PatCtxt<'_>,
        ctor: &Constructor,
        ctor_wild_subpatterns: &Fields,
    ) -> Self {
        let pat = {
            let len = self.0.len();
            let arity = ctor_wild_subpatterns.len();
            let pats = self.0.drain((len - arity)..).rev();
            ctor_wild_subpatterns.replace_fields(pcx.cx, pats).apply(pcx, ctor)
        };

        self.0.push(pat);

        self
    }
}

/// Algorithm from <http://moscova.inria.fr/~maranget/papers/warn/index.html>.
/// The algorithm from the paper has been modified to correctly handle empty
/// types. The changes are:
///   (0) We don't exit early if the pattern matrix has zero rows. We just
///       continue to recurse over columns.
///   (1) all_constructors will only return constructors that are statically
///       possible. E.g., it will only return `Ok` for `Result<T, !>`.
///
/// This finds whether a (row) vector `v` of patterns is 'useful' in relation
/// to a set of such vectors `m` - this is defined as there being a set of
/// inputs that will match `v` but not any of the sets in `m`.
///
/// All the patterns at each column of the `matrix ++ v` matrix must have the same type.
///
/// This is used both for reachability checking (if a pattern isn't useful in
/// relation to preceding patterns, it is not reachable) and exhaustiveness
/// checking (if a wildcard pattern is useful in relation to a matrix, the
/// matrix isn't exhaustive).
///
/// `is_under_guard` is used to inform if the pattern has a guard. If it
/// has one it must not be inserted into the matrix. This shouldn't be
/// relied on for soundness.
fn is_useful(
    cx: &MatchCheckCtx<'_>,
    matrix: &Matrix,
    v: &PatStack,
    witness_preference: WitnessPreference,
    is_under_guard: bool,
    is_top_level: bool,
) -> Usefulness {
    let Matrix { patterns: rows, .. } = matrix;

    // The base case. We are pattern-matching on () and the return value is
    // based on whether our matrix has a row or not.
    // NOTE: This could potentially be optimized by checking rows.is_empty()
    // first and then, if v is non-empty, the return value is based on whether
    // the type of the tuple we're checking is inhabited or not.
    if v.is_empty() {
        let ret = if rows.is_empty() {
            Usefulness::new_useful(witness_preference)
        } else {
            Usefulness::new_not_useful(witness_preference)
        };
        return ret;
    }

    assert!(rows.iter().all(|r| r.len() == v.len()));

    // FIXME(Nadrieril): Hack to work around type normalization issues (see rust-lang/rust#72476).
    // TODO(iDawer): ty.strip_references()  ?
    let ty = matrix.heads().next().map_or(cx.type_of(v.head()), |r| cx.type_of(r));
    let pcx = PatCtxt { cx, ty: &ty, is_top_level };

    // If the first pattern is an or-pattern, expand it.
    let ret = if v.head().is_or_pat(cx) {
        //expanding or-pattern
        let v_head = v.head();
        let vs: Vec<_> = v.expand_or_pat(cx).collect();
        let alt_count = vs.len();
        // We try each or-pattern branch in turn.
        let mut matrix = matrix.clone();
        let usefulnesses = vs.into_iter().enumerate().map(|(i, v)| {
            let usefulness = is_useful(cx, &matrix, &v, witness_preference, is_under_guard, false);
            // If pattern has a guard don't add it to the matrix.
            if !is_under_guard {
                // We push the already-seen patterns into the matrix in order to detect redundant
                // branches like `Some(_) | Some(0)`.
                matrix.push(v, cx);
            }
            usefulness.unsplit_or_pat(i, alt_count, v_head)
        });
        Usefulness::merge(witness_preference, usefulnesses)
    } else {
        let v_ctor = v.head_ctor(cx);
        // if let Constructor::IntRange(ctor_range) = v_ctor {
        //     // Lint on likely incorrect range patterns (#63987)
        //     ctor_range.lint_overlapping_range_endpoints(
        //         pcx,
        //         matrix.head_ctors_and_spans(cx),
        //         matrix.column_count().unwrap_or(0),
        //         hir_id,
        //     )
        // }

        // We split the head constructor of `v`.
        let split_ctors = v_ctor.split(pcx, matrix.head_ctors(cx));
        // For each constructor, we compute whether there's a value that starts with it that would
        // witness the usefulness of `v`.
        let start_matrix = matrix;
        let usefulnesses = split_ctors.into_iter().map(|ctor| {
            // debug!("specialize({:?})", ctor);
            // We cache the result of `Fields::wildcards` because it is used a lot.
            let ctor_wild_subpatterns = Fields::wildcards(pcx, &ctor);
            let spec_matrix =
                start_matrix.specialize_constructor(pcx, &ctor, &ctor_wild_subpatterns);
            let v = v.pop_head_constructor(&ctor_wild_subpatterns, cx);
            let usefulness =
                is_useful(cx, &spec_matrix, &v, witness_preference, is_under_guard, false);
            usefulness.apply_constructor(pcx, start_matrix, &ctor, &ctor_wild_subpatterns)
        });
        Usefulness::merge(witness_preference, usefulnesses)
    };

    ret
}

/// The arm of a match expression.
#[derive(Clone, Copy)]
pub(crate) struct MatchArm {
    pub(crate) pat: PatId,
    pub(crate) has_guard: bool,
}

/// Indicates whether or not a given arm is reachable.
#[derive(Clone, Debug)]
pub(crate) enum Reachability {
    /// The arm is reachable. This additionally carries a set of or-pattern branches that have been
    /// found to be unreachable despite the overall arm being reachable. Used only in the presence
    /// of or-patterns, otherwise it stays empty.
    Reachable(Vec<PatId>),
    /// The arm is unreachable.
    Unreachable,
}

/// The output of checking a match for exhaustiveness and arm reachability.
pub(crate) struct UsefulnessReport {
    /// For each arm of the input, whether that arm is reachable after the arms above it.
    pub(crate) arm_usefulness: Vec<(MatchArm, Reachability)>,
    /// If the match is exhaustive, this is empty. If not, this contains witnesses for the lack of
    /// exhaustiveness.
    pub(crate) non_exhaustiveness_witnesses: Vec<Pat>,
}

/// The entrypoint for the usefulness algorithm. Computes whether a match is exhaustive and which
/// of its arms are reachable.
///
/// Note: the input patterns must have been lowered through
/// `check_match::MatchVisitor::lower_pattern`.
pub(crate) fn compute_match_usefulness(
    cx: &MatchCheckCtx<'_>,
    arms: &[MatchArm],
) -> UsefulnessReport {
    let mut matrix = Matrix::empty();
    let arm_usefulness: Vec<_> = arms
        .iter()
        .copied()
        .map(|arm| {
            let v = PatStack::from_pattern(arm.pat);
            let usefulness = is_useful(cx, &matrix, &v, LeaveOutWitness, arm.has_guard, true);
            if !arm.has_guard {
                matrix.push(v, cx);
            }
            let reachability = match usefulness {
                NoWitnesses(subpats) if subpats.is_empty() => Reachability::Unreachable,
                NoWitnesses(subpats) => {
                    Reachability::Reachable(subpats.list_unreachable_subpatterns(cx).unwrap())
                }
                WithWitnesses(..) => panic!("bug"),
            };
            (arm, reachability)
        })
        .collect();

    let wild_pattern =
        cx.pattern_arena.borrow_mut().alloc(Pat::wildcard_from_ty(&cx.infer[cx.match_expr]));
    let v = PatStack::from_pattern(wild_pattern);
    let usefulness = is_useful(cx, &matrix, &v, ConstructWitness, false, true);
    let non_exhaustiveness_witnesses = match usefulness {
        WithWitnesses(pats) => pats.into_iter().map(Witness::single_pattern).collect(),
        NoWitnesses(_) => panic!("bug"),
    };
    UsefulnessReport { arm_usefulness, non_exhaustiveness_witnesses }
}

pub(crate) type PatternArena = Arena<Pat>;

mod helper {
    use hir_def::expr::{Pat, PatId};

    use super::MatchCheckCtx;

    pub(super) trait PatIdExt: Sized {
        // fn is_wildcard(self, cx: &MatchCheckCtx<'_>) -> bool;
        fn is_or_pat(self, cx: &MatchCheckCtx<'_>) -> bool;
        fn expand_or_pat(self, cx: &MatchCheckCtx<'_>) -> Vec<Self>;
    }

    // Copy-pasted from rust/compiler/rustc_data_structures/src/captures.rs
    /// "Signaling" trait used in impl trait to tag lifetimes that you may
    /// need to capture but don't really need for other reasons.
    /// Basically a workaround; see [this comment] for details.
    ///
    /// [this comment]: https://github.com/rust-lang/rust/issues/34511#issuecomment-373423999
    // FIXME(eddyb) false positive, the lifetime parameter is "phantom" but needed.
    #[allow(unused_lifetimes)]
    pub(crate) trait Captures<'a> {}

    impl<'a, T: ?Sized> Captures<'a> for T {}
}

#[test]
fn it_works() {}
