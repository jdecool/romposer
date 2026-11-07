//! Port of composer/semver 3.x: version normalisation, constraints and intervals.

pub mod compare;
pub mod constraint;
pub mod intervals;
pub mod version_parser;

pub use compare::{version_compare, version_compare_op};
pub use constraint::{Bound, Constraint, Op};
pub use version_parser::{
    DEFAULT_BRANCH_ALIAS, Stability, is_upgrade, is_valid, normalize, normalize_branch,
    parse_constraints, parse_numeric_alias_prefix, parse_stability,
};
