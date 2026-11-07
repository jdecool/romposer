# 0002: The dependency solver is Composer's, ported

Status: accepted, 2026-11-07

## Decision

Composer's SAT solver (pool builder, rule set, solver, policies, problem reporting) and `composer/semver` are ported faithfully to Rust. No third-party resolver crate.

## Why

ADR-0001 requires an identical lock after `update`. A different resolver, even a correct one, may choose different versions among equally valid solutions. Only the same algorithm with the same policy guarantees the same choice and the same problem messages.

## Consequences

The port is the largest ticket and the most interesting one. Composer's `.test` fixtures are the primary test seam for it.
