//! Port of composer/semver's `Intervals` and `Interval` (no memoization: the
//! callers that need it cache at their own level).

use super::compare::version_compare;
use super::constraint::{Constraint, Op, POSITIVE_INFINITY, ZERO};
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start: Constraint,
    pub end: Constraint,
}

impl Interval {
    pub fn from_zero() -> Constraint {
        Constraint::new(">=", ZERO)
    }

    pub fn until_positive_infinity() -> Constraint {
        Constraint::new("<", POSITIVE_INFINITY)
    }

    pub fn any() -> Interval {
        Interval {
            start: Self::from_zero(),
            end: Self::until_positive_infinity(),
        }
    }
}

/// Which `dev-*` branches a constraint matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branches {
    pub names: Vec<String>,
    pub exclude: bool,
}

impl Branches {
    /// any == exclude nothing
    pub fn any_dev() -> Branches {
        Branches {
            names: vec![],
            exclude: true,
        }
    }

    /// nothing == no names included
    pub fn no_dev() -> Branches {
        Branches {
            names: vec![],
            exclude: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intervals {
    pub numeric: Vec<Interval>,
    pub branches: Branches,
}

fn c_eq(a: &Constraint, b: &Constraint) -> bool {
    a.to_string() == b.to_string()
}

/// `Intervals::isSubsetOf`
pub fn is_subset_of(candidate: &Constraint, constraint: &Constraint) -> bool {
    if constraint.is_match_all() {
        return true;
    }
    if candidate.is_match_none() || constraint.is_match_none() {
        return false;
    }
    let intersection = get(&Constraint::Multi {
        constraints: vec![candidate.clone(), constraint.clone()],
        conjunctive: true,
    });
    let candidate_intervals = get(candidate);
    if intersection.numeric.len() != candidate_intervals.numeric.len() {
        return false;
    }
    for (i, interval) in intersection.numeric.iter().enumerate() {
        let Some(ci) = candidate_intervals.numeric.get(i) else {
            return false;
        };
        if !c_eq(&ci.start, &interval.start) || !c_eq(&ci.end, &interval.end) {
            return false;
        }
    }
    intersection.branches == candidate_intervals.branches
}

/// `Intervals::haveIntersections`, equivalent to `$a->matches($b)`.
pub fn have_intersections(a: &Constraint, b: &Constraint) -> bool {
    if a.is_match_all() || b.is_match_all() {
        return true;
    }
    if a.is_match_none() || b.is_match_none() {
        return false;
    }
    let i = generate_intervals(
        &Constraint::Multi {
            constraints: vec![a.clone(), b.clone()],
            conjunctive: true,
        },
        true,
    );
    !i.numeric.is_empty() || i.branches.exclude || !i.branches.names.is_empty()
}

/// `Intervals::compactConstraint`
pub fn compact_constraint(constraint: &Constraint) -> Constraint {
    if !matches!(constraint, Constraint::Multi { .. }) {
        return constraint.clone();
    }
    let intervals = generate_intervals(constraint, false);
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut has_numeric_match_all = false;

    let numeric = &intervals.numeric;
    if numeric.len() == 1
        && c_eq(&numeric[0].start, &Interval::from_zero())
        && c_eq(&numeric[0].end, &Interval::until_positive_infinity())
    {
        constraints.push(numeric[0].start.clone());
        has_numeric_match_all = true;
    } else {
        let count = numeric.len();
        let mut skip_not_equal = false;
        if !intervals.branches.exclude
            && count > 1
            && c_eq(&numeric[0].start, &Interval::from_zero())
            && c_eq(
                &numeric[count - 1].end,
                &Interval::until_positive_infinity(),
            )
        {
            skip_not_equal = true;
            for i in 0..count - 1 {
                if numeric[i].end.op() != Some(Op::Lt)
                    || numeric[i + 1].start.op() != Some(Op::Gt)
                    || numeric[i].end.version() != numeric[i + 1].start.version()
                {
                    skip_not_equal = false;
                    break;
                }
            }
        }

        let mut unequal: Vec<Constraint> = Vec::new();
        for i in 0..count {
            let interval = &numeric[i];
            if !skip_not_equal && interval.end.op() == Some(Op::Lt) && i + 1 < count {
                let next = &numeric[i + 1];
                if interval.end.version() == next.start.version() && next.start.op() == Some(Op::Gt)
                {
                    if unequal.is_empty() && !c_eq(&interval.start, &Interval::from_zero()) {
                        unequal.push(interval.start.clone());
                    }
                    unequal.push(Constraint::new("!=", interval.end.version().unwrap()));
                    continue;
                }
            }
            if !unequal.is_empty() {
                if !c_eq(&interval.end, &Interval::until_positive_infinity()) {
                    unequal.push(interval.end.clone());
                }
                if unequal.len() > 1 {
                    constraints.push(Constraint::Multi {
                        constraints: std::mem::take(&mut unequal),
                        conjunctive: true,
                    });
                } else {
                    constraints.push(unequal.pop().unwrap());
                }
                continue;
            }
            if interval.start.version() == interval.end.version()
                && interval.start.op() == Some(Op::Ge)
                && interval.end.op() == Some(Op::Le)
            {
                constraints.push(Constraint::new("==", interval.start.version().unwrap()));
                continue;
            }
            if c_eq(&interval.start, &Interval::from_zero()) {
                constraints.push(interval.end.clone());
            } else if c_eq(&interval.end, &Interval::until_positive_infinity()) {
                constraints.push(interval.start.clone());
            } else {
                constraints.push(Constraint::Multi {
                    constraints: vec![interval.start.clone(), interval.end.clone()],
                    conjunctive: true,
                });
            }
        }
    }

    let mut dev_constraints: Vec<Constraint> = Vec::new();
    if intervals.branches.names.is_empty() {
        if intervals.branches.exclude && has_numeric_match_all {
            return Constraint::MatchAll;
        }
    } else {
        for name in &intervals.branches.names {
            dev_constraints.push(Constraint::new(
                if intervals.branches.exclude {
                    "!="
                } else {
                    "=="
                },
                name,
            ));
        }
        if intervals.branches.exclude {
            if constraints.len() > 1 {
                let mut all = vec![Constraint::Multi {
                    constraints,
                    conjunctive: false,
                }];
                all.extend(dev_constraints);
                return Constraint::Multi {
                    constraints: all,
                    conjunctive: true,
                };
            }
            if constraints.len() == 1 && c_eq(&constraints[0], &Interval::from_zero()) {
                if dev_constraints.len() > 1 {
                    return Constraint::Multi {
                        constraints: dev_constraints,
                        conjunctive: true,
                    };
                }
                return dev_constraints.pop().unwrap();
            }
            constraints.extend(dev_constraints);
            return Constraint::Multi {
                constraints,
                conjunctive: true,
            };
        }
        constraints.extend(dev_constraints);
    }

    if constraints.len() > 1 {
        return Constraint::Multi {
            constraints,
            conjunctive: false,
        };
    }
    if constraints.len() == 1 {
        return constraints.pop().unwrap();
    }
    Constraint::MatchNone
}

/// `Intervals::get`
pub fn get(constraint: &Constraint) -> Intervals {
    generate_intervals(constraint, false)
}

fn op_sort_order(op: Op) -> i32 {
    match op {
        Op::Ge => -3,
        Op::Lt => -2,
        Op::Gt => 2,
        Op::Le => 3,
        _ => 0,
    }
}

struct Border<'a> {
    version: &'a str,
    op: Op,
    start: bool,
}

fn generate_intervals(constraint: &Constraint, stop_on_first_valid_interval: bool) -> Intervals {
    match constraint {
        Constraint::MatchAll => {
            return Intervals {
                numeric: vec![Interval::any()],
                branches: Branches::any_dev(),
            };
        }
        Constraint::MatchNone => {
            return Intervals {
                numeric: vec![],
                branches: Branches::no_dev(),
            };
        }
        Constraint::Single { .. } => return single_constraint_intervals(constraint),
        Constraint::Multi { .. } => {}
    }
    let Constraint::Multi {
        constraints,
        conjunctive,
    } = constraint
    else {
        unreachable!()
    };

    let results: Vec<Intervals> = constraints.iter().map(get).collect();

    let mut branches;
    if !conjunctive {
        branches = Branches::no_dev();
        for b in results.iter().map(|r| &r.branches) {
            if b.exclude {
                if branches.exclude {
                    // !=a,!=b || !=b,!=c => !=b
                    branches.names.retain(|n| b.names.contains(n));
                } else {
                    // (==b || ==c) || !=a,!=b => !=a
                    branches.exclude = true;
                    branches.names = b
                        .names
                        .iter()
                        .filter(|n| !branches.names.contains(n))
                        .cloned()
                        .collect();
                }
            } else if branches.exclude {
                // !=a,!=b || (==b || ==c) => !=a
                branches.names.retain(|n| !b.names.contains(n));
            } else {
                // (==a || ==b) || ==c => ==a || ==b || ==c
                branches.names.extend(b.names.iter().cloned());
            }
        }
    } else {
        branches = Branches::any_dev();
        for b in results.iter().map(|r| &r.branches) {
            if b.exclude {
                if branches.exclude {
                    // !=a && !=b => !=a,!=b
                    branches.names.extend(b.names.iter().cloned());
                } else {
                    // (==a||==c) && !=a,!=b => ==c
                    branches.names.retain(|n| !b.names.contains(n));
                }
            } else if branches.exclude {
                // !=a,!=b && (==a||==c) => ==c
                branches.names = b
                    .names
                    .iter()
                    .filter(|n| !branches.names.contains(n))
                    .cloned()
                    .collect();
                branches.exclude = false;
            } else {
                // (==a||==b) && (==a||==c) => ==a
                branches.names.retain(|n| b.names.contains(n));
            }
        }
    }
    let mut unique = Vec::new();
    for n in branches.names {
        if !unique.contains(&n) {
            unique.push(n);
        }
    }
    branches.names = unique;

    if results.len() == 1 {
        return Intervals {
            numeric: results.into_iter().next().unwrap().numeric,
            branches,
        };
    }

    let mut borders: Vec<Border<'_>> = Vec::new();
    for group in &results {
        for interval in &group.numeric {
            borders.push(Border {
                version: interval.start.version().unwrap(),
                op: interval.start.op().unwrap(),
                start: true,
            });
            borders.push(Border {
                version: interval.end.version().unwrap(),
                op: interval.end.op().unwrap(),
                start: false,
            });
        }
    }
    borders.sort_by(|a, b| match version_compare(a.version, b.version) {
        Ordering::Equal => op_sort_order(a.op).cmp(&op_sort_order(b.op)),
        other => other,
    });

    let mut active: usize = 0;
    let mut intervals = Vec::new();
    let activation_threshold = if *conjunctive { results.len() } else { 1 };
    let mut start: Option<Constraint> = None;
    for border in &borders {
        if border.start {
            active += 1;
        } else {
            active -= 1;
        }
        if start.is_none() && active >= activation_threshold {
            start = Some(Constraint::new(border.op.as_str(), border.version));
        } else if let Some(s) = &start
            && active < activation_threshold
        {
            // filter out invalid intervals like > x - <= x, or >= x - < x
            let invalid = version_compare(s.version().unwrap(), border.version) == Ordering::Equal
                && ((s.op() == Some(Op::Gt) && border.op == Op::Le)
                    || (s.op() == Some(Op::Ge) && border.op == Op::Lt));
            if !invalid {
                intervals.push(Interval {
                    start: s.clone(),
                    end: Constraint::new(border.op.as_str(), border.version),
                });
                if stop_on_first_valid_interval {
                    break;
                }
            }
            start = None;
        }
    }

    Intervals {
        numeric: intervals,
        branches,
    }
}

fn single_constraint_intervals(constraint: &Constraint) -> Intervals {
    let (op, version) = (constraint.op().unwrap(), constraint.version().unwrap());
    if version.starts_with("dev-") {
        let mut numeric = vec![];
        let mut branches = Branches::no_dev();
        if op == Op::Ne {
            numeric.push(Interval::any());
            branches = Branches {
                names: vec![version.to_string()],
                exclude: true,
            };
        } else if op == Op::Eq {
            branches.names.push(version.to_string());
        }
        return Intervals { numeric, branches };
    }
    match op {
        Op::Gt | Op::Ge => Intervals {
            numeric: vec![Interval {
                start: constraint.clone(),
                end: Interval::until_positive_infinity(),
            }],
            branches: Branches::no_dev(),
        },
        Op::Lt | Op::Le => {
            if op == Op::Lt && version == ZERO {
                return Intervals {
                    numeric: vec![],
                    branches: Branches::no_dev(),
                };
            }
            Intervals {
                numeric: vec![Interval {
                    start: Interval::from_zero(),
                    end: constraint.clone(),
                }],
                branches: Branches::no_dev(),
            }
        }
        Op::Ne => Intervals {
            numeric: vec![
                Interval {
                    start: Interval::from_zero(),
                    end: Constraint::new("<", version),
                },
                Interval {
                    start: Constraint::new(">", version),
                    end: Interval::until_positive_infinity(),
                },
            ],
            branches: Branches::any_dev(),
        },
        Op::Eq => Intervals {
            numeric: vec![Interval {
                start: Constraint::new(">=", version),
                end: Constraint::new("<=", version),
            }],
            branches: Branches::no_dev(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semver::version_parser::parse_constraints;

    fn compact(parts: &[&str], conjunctive: bool) -> String {
        let parts = parts
            .iter()
            .map(|p| parse_constraints(p).unwrap())
            .collect();
        compact_constraint(&Constraint::Multi {
            constraints: parts,
            conjunctive,
        })
        .to_string()
    }

    fn expected(s: &str) -> String {
        if s == "NONE" {
            "[]".to_string()
        } else {
            parse_constraints(s).unwrap().to_string()
        }
    }

    // composer/semver tests/IntervalsTest.php, compactProvider.
    #[test]
    fn compacts_like_composer() {
        let cases: &[(&str, &[&str], bool)] = &[
            (
                "1.0 - 1.2 || ^1.5",
                &["1.0 - 1.2 || ^1.5", "1.8 - 1.9 || ^1.12"],
                false,
            ),
            (
                "1.8 - 1.9 || ^1.12",
                &["1.0 - 1.2 || ^1.5", "1.8 - 1.9 || ^1.12"],
                true,
            ),
            (
                "1.8 - 1.9 || ^1.12 || dev-master || dev-foo",
                &["1.8 - 1.9 || ^1.12", "dev-master", "dev-foo"],
                false,
            ),
            (
                "NONE",
                &["1.8 - 1.9 || ^1.12", "dev-master", "dev-foo"],
                true,
            ),
            ("NONE", &["1.0", "2.0"], true),
            ("1.0 || 2.0", &["1.0", "2.0"], false),
            ("!= 1.2, != 1.6", &["!= 1.2", "!= 1.6"], true),
            ("!= 1.0, != 2.0", &["!= 1.0", "!= 2.0"], true),
            (">1.5, != 2.0", &["!= 2.0", "> 1.5"], true),
            ("!= 1.0", &["!= 1.0", "!= 1.0"], false),
            ("*", &["!= 1.0", "!= 1.0", "!= dev-foo", "1.0.5.*"], false),
            (
                "< 2.0.0.0-stable || > 2.0.0",
                &["> 1.0.0 != 2.0.0", "< 1.9.0"],
                false,
            ),
            (
                "< 2.0.0.0-stable || > 2.0.0 < 3.0.0.0-stable || > 3.0.0",
                &["> 1.0.0 != 2.0.0 != 3.0.0", "< 1.5"],
                false,
            ),
            (
                "< 2.0.0.0-stable || > 2.0.0 < 3.0.0.0-stable || > 3.0.0 || dev-bar",
                &["> 1.0.0 != 2.0.0 != 3.0.0", "< 1.5", "dev-bar"],
                false,
            ),
            (
                "1.0.5.*",
                &["!= 1.0", "!= 1.0", "!= dev-foo", "1.0.5.*"],
                true,
            ),
            (
                ">= 1.0-dev, != 1.2-stable, <2",
                &["!= 1.2", "!= dev-foo", "!= dev-bar", "1.*"],
                true,
            ),
            (
                "!= 1.2, != dev-foo, != dev-bar",
                &["!= 1.2", "!= dev-foo", "!= dev-bar"],
                true,
            ),
            ("*", &["!= 1.2", "!= dev-foo", "!= dev-bar"], false),
            ("== dev-foo", &["!= 1.2", "== dev-foo", "!= dev-bar"], true),
            (
                "*",
                &["!= 1.0", "!= 1.0", "!= dev-foo", "1.0.5.*", "== dev-bla"],
                false,
            ),
            (
                "dev-bla",
                &["!= 1.0", "!= 1.0", "!= dev-foo", "== dev-bla"],
                true,
            ),
            (
                "NONE",
                &["!= 1.0", "!= 1.0", "!= dev-foo", "1.0.5.*", "== dev-bla"],
                true,
            ),
            (
                "!= dev-master, != dev-foo",
                &["!= dev-master", "!= dev-foo"],
                true,
            ),
            ("== dev-foo", &["!= dev-master", "== dev-foo"], true),
            ("!= dev-master", &["!= dev-master", "== dev-foo"], false),
            ("> 5", &["!= dev-master", "!= dev-foo", "> 5"], true),
            ("!= dev-foo", &["!= dev-foo", "!= dev-foo"], true),
            (
                "!= 3-stable, <5 || >=6, <9",
                &["!= 3, <5", ">=6, <9"],
                false,
            ),
            (
                "== dev-foo || == dev-bar",
                &["!= 1.0", "!= 2.0", "==dev-foo || ==dev-bar"],
                true,
            ),
        ];
        for (exp, parts, conjunctive) in cases {
            assert_eq!(
                compact(parts, *conjunctive),
                expected(exp),
                "{parts:?} conjunctive={conjunctive}"
            );
        }
    }

    // compactDevSafetyProvider
    #[test]
    fn compacting_keeps_the_matched_set() {
        let cases: &[(&[&str], bool)] = &[
            (&["> 1.0.0 != 2.0.0", "< 1.9.0"], false),
            (&["> 1.0.0 != 2.0.0 != 3.0.0", "< 1.5"], false),
            (&["> 1.0.0 != 2.0.0", "< 1.5", "dev-bar"], false),
            (&["> 1.0.0 != 2.0.0 != 3.0.0", "< 1.5", "dev-bar"], false),
            (&["> 1.0.0 != 2.0.0 < 3.0.0", "< 1.5", ">= 5.0"], false),
            (&["!= 2.0", "!= 3.0"], true),
            (&["!= 2.0", "!= dev-foo"], true),
            (&["!= 2.0", "dev-bar"], false),
        ];
        let dev = Constraint::new("==", "dev-foo");
        for (parts, conjunctive) in cases {
            let parts = parts
                .iter()
                .map(|p| parse_constraints(p).unwrap())
                .collect();
            let constraint = Constraint::Multi {
                constraints: parts,
                conjunctive: *conjunctive,
            };
            let compacted = compact_constraint(&constraint);
            assert_eq!(
                constraint.matches(&dev),
                compacted.matches(&dev),
                "{compacted}"
            );
            assert!(is_subset_of(&compacted, &constraint), "{compacted}");
            assert!(is_subset_of(&constraint, &compacted), "{compacted}");
        }
    }

    #[test]
    fn subsets_and_intersections() {
        let p = |s: &str| parse_constraints(s).unwrap();
        assert!(is_subset_of(&p("^1.1"), &p("^1.0")));
        assert!(!is_subset_of(&p("^1.0"), &p("^1.1")));
        assert!(is_subset_of(&p("dev-foo"), &p("*")));
        assert!(have_intersections(&p("^1.0"), &p(">=1.5")));
        assert!(!have_intersections(&p("^1.0"), &p(">=2.0")));
        assert!(have_intersections(&p("!=dev-foo"), &p("dev-bar")));
        assert!(!have_intersections(&p("dev-foo"), &p("dev-bar")));
    }
}
