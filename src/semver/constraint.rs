//! Port of composer/semver's `Constraint`, `MultiConstraint`, `MatchAllConstraint`,
//! `MatchNoneConstraint` and `Bound`.

use super::compare::{version_compare, version_compare_op};
use std::cmp::Ordering;
use std::fmt;

/// `PHP_INT_MAX.'.0.0.0'`, the upper bound of every numeric interval.
pub const POSITIVE_INFINITY: &str = "9223372036854775807.0.0.0";
pub const ZERO: &str = "0.0.0.0-dev";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
    Ne,
}

impl Op {
    pub fn parse(op: &str) -> Option<Op> {
        Some(match op {
            "=" | "==" => Op::Eq,
            "<" => Op::Lt,
            "<=" => Op::Le,
            ">" => Op::Gt,
            ">=" => Op::Ge,
            "<>" | "!=" => Op::Ne,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "==",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Ne => "!=",
        }
    }

    /// The operator with any `=` removed, as `matchSpecific` compares them.
    fn no_equal(self) -> &'static str {
        match self {
            Op::Eq => "",
            Op::Lt | Op::Le => "<",
            Op::Gt | Op::Ge => ">",
            Op::Ne => "!",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub version: String,
    pub inclusive: bool,
}

impl Bound {
    pub fn zero() -> Bound {
        Bound {
            version: ZERO.into(),
            inclusive: true,
        }
    }

    pub fn positive_infinity() -> Bound {
        Bound {
            version: POSITIVE_INFINITY.into(),
            inclusive: false,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.version == ZERO && self.inclusive
    }

    pub fn is_positive_infinity(&self) -> bool {
        self.version == POSITIVE_INFINITY && !self.inclusive
    }

    /// `compareTo($other, '>' | '<')`: "am I higher/lower than other", for lower bounds.
    pub fn compare_to(&self, other: &Bound, greater: bool) -> bool {
        if self == other {
            return false;
        }
        match version_compare(&self.version, &other.version) {
            Ordering::Equal => {
                if greater {
                    other.inclusive
                } else {
                    !other.inclusive
                }
            }
            Ordering::Greater => greater,
            Ordering::Less => !greater,
        }
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} [{}]",
            self.version,
            if self.inclusive {
                "inclusive"
            } else {
                "exclusive"
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Constraint {
    Single {
        op: Op,
        version: String,
    },
    Multi {
        constraints: Vec<Constraint>,
        conjunctive: bool,
    },
    MatchAll,
    MatchNone,
}

impl Constraint {
    pub fn new(op: &str, version: &str) -> Constraint {
        let op = Op::parse(op).unwrap_or_else(|| panic!("Invalid operator \"{op}\" given"));
        Constraint::Single {
            op,
            version: version.to_string(),
        }
    }

    /// `MultiConstraint::create`: collapses empty/single sets and contiguous `>= a <b || >= b <c` ranges.
    pub fn multi(mut constraints: Vec<Constraint>, mut conjunctive: bool) -> Constraint {
        if constraints.is_empty() {
            return Constraint::MatchAll;
        }
        if constraints.len() == 1 {
            return constraints.pop().unwrap();
        }
        if let Some((optimized, conj)) = optimize(&constraints, conjunctive) {
            constraints = optimized;
            conjunctive = conj;
            if constraints.len() == 1 {
                return constraints.pop().unwrap();
            }
        }
        Constraint::Multi {
            constraints,
            conjunctive,
        }
    }

    pub fn is_match_all(&self) -> bool {
        matches!(self, Constraint::MatchAll)
    }

    pub fn is_match_none(&self) -> bool {
        matches!(self, Constraint::MatchNone)
    }

    pub fn is_conjunctive(&self) -> bool {
        matches!(
            self,
            Constraint::Multi {
                conjunctive: true,
                ..
            }
        )
    }

    pub fn is_disjunctive(&self) -> bool {
        matches!(
            self,
            Constraint::Multi {
                conjunctive: false,
                ..
            }
        )
    }

    pub fn op(&self) -> Option<Op> {
        match self {
            Constraint::Single { op, .. } => Some(*op),
            _ => None,
        }
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Constraint::Single { version, .. } => Some(version),
            _ => None,
        }
    }

    pub fn matches(&self, provider: &Constraint) -> bool {
        match self {
            Constraint::MatchAll => true,
            Constraint::MatchNone => false,
            Constraint::Single { .. } => match provider {
                Constraint::Single { .. } => self.match_specific(provider, false),
                _ => provider.matches(self),
            },
            Constraint::Multi {
                constraints,
                conjunctive,
            } => {
                if !conjunctive {
                    return constraints.iter().any(|c| provider.matches(c));
                }
                if provider.is_disjunctive() {
                    return provider.matches(self);
                }
                constraints.iter().all(|c| provider.matches(c))
            }
        }
    }

    pub fn match_specific(&self, provider: &Constraint, compare_branches: bool) -> bool {
        let (
            Constraint::Single {
                op: this_op,
                version: this_v,
            },
            Constraint::Single {
                op: prov_op,
                version: prov_v,
            },
        ) = (self, provider)
        else {
            panic!("match_specific requires two single constraints");
        };
        let no_equal_op = this_op.no_equal();
        let provider_no_equal_op = prov_op.no_equal();
        let is_equal_op = *this_op == Op::Eq;
        let is_non_equal_op = *this_op == Op::Ne;
        let is_provider_equal_op = *prov_op == Op::Eq;
        let is_provider_non_equal_op = *prov_op == Op::Ne;

        if is_non_equal_op || is_provider_non_equal_op {
            if is_non_equal_op
                && !is_provider_non_equal_op
                && !is_provider_equal_op
                && prov_v.starts_with("dev-")
            {
                return false;
            }
            if is_provider_non_equal_op
                && !is_non_equal_op
                && !is_equal_op
                && this_v.starts_with("dev-")
            {
                return false;
            }
            if !is_equal_op && !is_provider_equal_op {
                return true;
            }
            return version_compare_branches(prov_v, this_v, "!=", compare_branches);
        }

        if *this_op != Op::Eq && no_equal_op == provider_no_equal_op {
            return !(this_v.starts_with("dev-") || prov_v.starts_with("dev-"));
        }

        let (version1, version2, operator) = if is_equal_op {
            (this_v.as_str(), prov_v.as_str(), *prov_op)
        } else {
            (prov_v.as_str(), this_v.as_str(), *this_op)
        };

        if version_compare_branches(version1, version2, operator.as_str(), compare_branches) {
            // special case, e.g. require >= 1.0 and provide < 1.0
            return !(prov_op.as_str() == provider_no_equal_op
                && this_op.as_str() != no_equal_op
                && version_compare(prov_v, this_v) == Ordering::Equal);
        }
        false
    }

    pub fn lower_bound(&self) -> Bound {
        self.bounds().0
    }

    pub fn upper_bound(&self) -> Bound {
        self.bounds().1
    }

    fn bounds(&self) -> (Bound, Bound) {
        match self {
            Constraint::MatchAll => (Bound::zero(), Bound::positive_infinity()),
            Constraint::MatchNone => (
                Bound {
                    version: ZERO.into(),
                    inclusive: false,
                },
                Bound {
                    version: ZERO.into(),
                    inclusive: false,
                },
            ),
            Constraint::Single { op, version } => {
                if version.starts_with("dev-") {
                    return (Bound::zero(), Bound::positive_infinity());
                }
                let at = |inclusive| Bound {
                    version: version.clone(),
                    inclusive,
                };
                match op {
                    Op::Eq => (at(true), at(true)),
                    Op::Lt => (Bound::zero(), at(false)),
                    Op::Le => (Bound::zero(), at(true)),
                    Op::Gt => (at(false), Bound::positive_infinity()),
                    Op::Ge => (at(true), Bound::positive_infinity()),
                    Op::Ne => (Bound::zero(), Bound::positive_infinity()),
                }
            }
            Constraint::Multi {
                constraints,
                conjunctive,
            } => {
                let mut lower: Option<Bound> = None;
                let mut upper: Option<Bound> = None;
                for c in constraints {
                    let (cl, cu) = c.bounds();
                    let (Some(l), Some(u)) = (&lower, &upper) else {
                        lower = Some(cl);
                        upper = Some(cu);
                        continue;
                    };
                    if cl.compare_to(l, *conjunctive) {
                        lower = Some(cl);
                    }
                    match version_compare(&cu.version, &u.version) {
                        Ordering::Equal => {
                            // same version: for upper bounds inclusive is higher
                            if cu.inclusive == !*conjunctive {
                                upper = Some(cu);
                            }
                        }
                        Ordering::Less if *conjunctive => upper = Some(cu),
                        Ordering::Greater if !*conjunctive => upper = Some(cu),
                        _ => {}
                    }
                }
                (lower.unwrap(), upper.unwrap())
            }
        }
    }
}

/// `Constraint::versionCompare`: branches only ever compare equal to themselves.
fn version_compare_branches(a: &str, b: &str, op: &str, compare_branches: bool) -> bool {
    let a_is_branch = a.starts_with("dev-");
    let b_is_branch = b.starts_with("dev-");
    if op == "!=" && (a_is_branch || b_is_branch) {
        return a != b;
    }
    if a_is_branch && b_is_branch {
        return op == "==" && a == b;
    }
    if !compare_branches && (a_is_branch || b_is_branch) {
        return false;
    }
    version_compare_op(a, b, op)
}

fn optimize(constraints: &[Constraint], conjunctive: bool) -> Option<(Vec<Constraint>, bool)> {
    if conjunctive {
        return None;
    }
    let range = |c: &Constraint| -> Option<(Constraint, Constraint, String, String)> {
        let Constraint::Multi {
            constraints,
            conjunctive: true,
        } = c
        else {
            return None;
        };
        if constraints.len() != 2 {
            return None;
        }
        let (
            Constraint::Single {
                op: Op::Ge,
                version: lo,
            },
            Constraint::Single {
                op: Op::Lt,
                version: hi,
            },
        ) = (&constraints[0], &constraints[1])
        else {
            return None;
        };
        Some((
            constraints[0].clone(),
            constraints[1].clone(),
            lo.clone(),
            hi.clone(),
        ))
    };
    let mut left = constraints[0].clone();
    let mut merged = Vec::new();
    let mut optimized = false;
    for right in &constraints[1..] {
        if let (Some((l0, _, _, lhi)), Some((_, r1, rlo, _))) = (range(&left), range(right))
            && lhi == rlo
        {
            optimized = true;
            left = Constraint::Multi {
                constraints: vec![l0, r1],
                conjunctive: true,
            };
        } else {
            merged.push(left);
            left = right.clone();
        }
    }
    if optimized {
        merged.push(left);
        return Some((merged, false));
    }
    None
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::MatchAll => f.write_str("*"),
            Constraint::MatchNone => f.write_str("[]"),
            Constraint::Single { op, version } => write!(f, "{} {}", op.as_str(), version),
            Constraint::Multi {
                constraints,
                conjunctive,
            } => {
                f.write_str("[")?;
                let sep = if *conjunctive { " " } else { " || " };
                for (i, c) in constraints.iter().enumerate() {
                    if i > 0 {
                        f.write_str(sep)?;
                    }
                    write!(f, "{c}")?;
                }
                f.write_str("]")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tables from composer/semver tests/Constraint/ConstraintTest.php.
    const SUCCESS: &[(&str, &str, &str, &str)] = &[
        ("==", "2", "==", "2"),
        ("==", "2", "<", "3"),
        ("==", "2", "<=", "2"),
        ("==", "2", "<=", "3"),
        ("==", "2", ">=", "1"),
        ("==", "2", ">=", "2"),
        ("==", "2", ">", "1"),
        ("==", "2", "!=", "1"),
        ("==", "2", "!=", "3"),
        ("<", "2", "==", "1"),
        ("<", "2", "<", "1"),
        ("<", "2", "<", "2"),
        ("<", "2", "<", "3"),
        ("<", "2", "<=", "1"),
        ("<", "2", "<=", "2"),
        ("<", "2", "<=", "3"),
        ("<", "2", ">=", "1"),
        ("<", "2", ">", "1"),
        ("<", "2", "!=", "1"),
        ("<", "2", "!=", "2"),
        ("<", "2", "!=", "3"),
        ("<=", "2", "==", "1"),
        ("<=", "2", "==", "2"),
        ("<=", "2", "<", "1"),
        ("<=", "2", "<", "2"),
        ("<=", "2", "<", "3"),
        ("<=", "2", "<=", "1"),
        ("<=", "2", "<=", "2"),
        ("<=", "2", "<=", "3"),
        ("<=", "2", ">=", "1"),
        ("<=", "2", ">=", "2"),
        ("<=", "2", ">", "1"),
        ("<=", "2", "!=", "1"),
        ("<=", "2", "!=", "2"),
        ("<=", "2", "!=", "3"),
        (">=", "2", "==", "2"),
        (">=", "2", "==", "3"),
        (">=", "2", "<", "3"),
        (">=", "2", "<=", "2"),
        (">=", "2", "<=", "3"),
        (">=", "2", ">=", "1"),
        (">=", "2", ">=", "2"),
        (">=", "2", ">=", "3"),
        (">=", "2", ">", "1"),
        (">=", "2", ">", "2"),
        (">=", "2", ">", "3"),
        (">=", "2", "!=", "1"),
        (">=", "2", "!=", "2"),
        (">=", "2", "!=", "3"),
        (">", "2", "==", "3"),
        (">", "2", "<", "3"),
        (">", "2", "<=", "3"),
        (">", "2", ">=", "1"),
        (">", "2", ">=", "2"),
        (">", "2", ">=", "3"),
        (">", "2", ">", "1"),
        (">", "2", ">", "2"),
        (">", "2", ">", "3"),
        (">", "2", "!=", "1"),
        (">", "2", "!=", "2"),
        (">", "2", "!=", "3"),
        ("!=", "2", "!=", "1"),
        ("!=", "2", "!=", "2"),
        ("!=", "2", "!=", "3"),
        ("!=", "2", "==", "1"),
        ("!=", "2", "==", "3"),
        ("!=", "2", "<", "1"),
        ("!=", "2", "<", "2"),
        ("!=", "2", "<", "3"),
        ("!=", "2", "<=", "1"),
        ("!=", "2", "<=", "2"),
        ("!=", "2", "<=", "3"),
        ("!=", "2", ">=", "1"),
        ("!=", "2", ">=", "2"),
        ("!=", "2", ">=", "3"),
        ("!=", "2", ">", "1"),
        ("!=", "2", ">", "2"),
        ("!=", "2", ">", "3"),
        ("==", "dev-foo-bar", "==", "dev-foo-bar"),
        ("==", "dev-events+issue-17", "==", "dev-events+issue-17"),
        ("==", "dev-foo-bar", "!=", "dev-foo-xyz"),
        ("!=", "dev-foo-bar", "!=", "dev-foo-xyz"),
        ("==", "0.12", "!=", "dev-foo"),
        ("<", "0.12", "!=", "dev-foo"),
        ("<=", "0.12", "!=", "dev-foo"),
        (">=", "0.12", "!=", "dev-foo"),
        (">", "0.12", "!=", "dev-foo"),
        ("!=", "0.12", "==", "dev-foo"),
        ("!=", "0.12", "!=", "dev-foo"),
    ];

    const FAILURE: &[(&str, &str, &str, &str)] = &[
        ("==", "2", "==", "1"),
        ("==", "2", "==", "3"),
        ("==", "2", "<", "1"),
        ("==", "2", "<", "2"),
        ("==", "2", "<=", "1"),
        ("==", "2", ">=", "3"),
        ("==", "2", ">", "2"),
        ("==", "2", ">", "3"),
        ("==", "2", "!=", "2"),
        ("<", "2", "==", "2"),
        ("<", "2", "==", "3"),
        ("<", "2", ">=", "2"),
        ("<", "2", ">=", "3"),
        ("<", "2", ">", "2"),
        ("<", "2", ">", "3"),
        ("<=", "2", "==", "3"),
        ("<=", "2", ">=", "3"),
        ("<=", "2", ">", "2"),
        ("<=", "2", ">", "3"),
        (">=", "2", "==", "1"),
        (">=", "2", "<", "1"),
        (">=", "2", "<", "2"),
        (">=", "2", "<=", "1"),
        (">", "2", "==", "1"),
        (">", "2", "==", "2"),
        (">", "2", "<", "1"),
        (">", "2", "<", "2"),
        (">", "2", "<=", "1"),
        (">", "2", "<=", "2"),
        ("!=", "2", "==", "2"),
        ("==", "2.0-b2", "<", "2.0-beta2"),
        ("==", "dev-foo-dist", "==", "dev-foo-zist"),
        ("==", "dev-foo-bar", "==", "dev-foo-xyz"),
        ("==", "dev-foo-bar", "<", "dev-foo-xyz"),
        ("==", "dev-foo-bar", "<=", "dev-foo-xyz"),
        ("==", "dev-foo-bar", ">=", "dev-foo-xyz"),
        ("==", "dev-foo-bar", ">", "dev-foo-xyz"),
        ("<", "dev-foo-bar", "==", "dev-foo-xyz"),
        ("<", "dev-foo-bar", "<", "dev-foo-xyz"),
        ("<", "dev-foo-bar", "!=", "dev-foo-xyz"),
        (">", "dev-foo-bar", ">=", "dev-foo-xyz"),
        (">", "dev-foo-bar", "!=", "dev-foo-xyz"),
        ("==", "dev-foo-bar", "<", "dev-foo-bar"),
        ("==", "dev-foo-bar", "!=", "dev-foo-bar"),
        ("<", "dev-foo-bar", "==", "dev-foo-bar"),
        ("<", "dev-foo-bar", "<", "dev-foo-bar"),
        ("<=", "dev-foo-bar", "!=", "dev-foo-bar"),
        (">=", "dev-foo-bar", ">", "dev-foo-bar"),
        (">", "dev-foo-bar", "!=", "dev-foo-bar"),
        ("==", "0.12", "==", "dev-foo"),
        ("==", "0.12", "<", "dev-foo"),
        ("==", "0.12", ">", "dev-foo"),
        ("<", "0.12", "==", "dev-foo"),
        ("<", "0.12", "<", "dev-foo"),
        ("<", "0.12", ">", "dev-foo"),
        ("<=", "0.12", "<=", "dev-foo"),
        (">=", "0.12", ">=", "dev-foo"),
        (">", "0.12", "==", "dev-foo"),
        (">", "0.12", ">", "dev-foo"),
        ("!=", "0.12", "<", "dev-foo"),
        ("!=", "0.12", "<=", "dev-foo"),
        ("!=", "0.12", ">=", "dev-foo"),
        ("!=", "0.12", ">", "dev-foo"),
    ];

    #[test]
    fn matching_succeeds_and_is_commutative() {
        for (ro, rv, po, pv) in SUCCESS {
            let require = Constraint::new(ro, rv);
            let provide = Constraint::new(po, pv);
            assert!(
                require.matches(&provide),
                "{require} should match {provide}"
            );
            assert!(
                provide.matches(&require),
                "{provide} should match {require}"
            );
        }
    }

    #[test]
    fn matching_fails_and_is_commutative() {
        for (ro, rv, po, pv) in FAILURE {
            let require = Constraint::new(ro, rv);
            let provide = Constraint::new(po, pv);
            assert!(
                !require.matches(&provide),
                "{require} should not match {provide}"
            );
            assert!(
                !provide.matches(&require),
                "{provide} should not match {require}"
            );
        }
    }

    #[test]
    fn multi_constraint_display_and_matching() {
        let range = Constraint::multi(
            vec![Constraint::new(">", "1"), Constraint::new("<", "2")],
            true,
        );
        assert_eq!(range.to_string(), "[> 1 < 2]");
        let outside = Constraint::multi(
            vec![Constraint::new("<", "1"), Constraint::new(">", "2")],
            false,
        );
        assert_eq!(outside.to_string(), "[< 1 || > 2]");
        assert!(!range.matches(&outside));
        assert!(!outside.matches(&range));
        assert!(range.matches(&Constraint::new("==", "1.5")));
    }

    #[test]
    fn create_collapses_contiguous_ranges() {
        let r = |lo: &str, hi: &str| {
            Constraint::multi(
                vec![Constraint::new(">=", lo), Constraint::new("<", hi)],
                true,
            )
        };
        let c = Constraint::multi(vec![r("1", "2"), r("2", "3"), r("3", "4")], false);
        assert_eq!(c.to_string(), "[>= 1 < 4]");
    }

    #[test]
    fn bounds() {
        let c = Constraint::multi(
            vec![
                Constraint::new(">=", "1.0.0.0"),
                Constraint::new("<", "2.0.0.0-dev"),
            ],
            true,
        );
        assert_eq!(
            c.lower_bound(),
            Bound {
                version: "1.0.0.0".into(),
                inclusive: true
            }
        );
        assert_eq!(
            c.upper_bound(),
            Bound {
                version: "2.0.0.0-dev".into(),
                inclusive: false
            }
        );
        assert!(Constraint::new("==", "dev-main").lower_bound().is_zero());
    }
}
