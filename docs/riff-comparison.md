# Comparison with riff

[riff](https://github.com/shyim/riff) is another Composer port written in Rust.
This compares romposer with riff 0.0.8 at commit `81ee744` (2026-11-07), read from its source and documentation.
riff was not built or run, and its own claims were not verified.

## Goals

romposer aims to produce exactly what Composer 2.11.1 produces, and installs from a shared pcomposer store (ADR 0001, ADR 0003).
riff aims to be a broader, faster Composer-compatible package manager that adds features of its own.

## Side by side

| | romposer | riff |
|---|---|---|
| Goal | Same output as Composer 2.11.1: lock, `vendor/`, autoloader, JSON outputs and exit codes | "Composer-compatible", with its own features and its own cache |
| Size | About 40k lines of Rust in one crate, plus 3.7k lines of tests | About 134k lines in 4 crates: `riff` (CLI, 27k), `riff-core` (100k), `riff-semver`, `riff-spdx` |
| Direct dependencies | 12 | About 60 (rkyv, indicatif, tar, xz2, diffy, serde_yaml...) |
| Install path | pcomposer store by default: one copy of each file per machine, hard-linked or cloned into each project | A dedicated riff cache, no hard-linking or cloning into projects |
| Solver | Port of Composer's solver (ADR 0002) | Same kind of solver as Composer's, plus a pool optimizer run before solving |
| PHP plugins | Installed but never run, with a warning (ADR 0004) | Rust versions of Symfony Flex 2.x (recipes, `recipes:update` as a 3-way merge), Composer Patches 1.x and 2.x, composer-bin, phpstan extension-installer, php-http/discovery and symfony/runtime; any other enabled plugin stops the install with an error |
| Scripts | Shell scripts run as with Composer; PHP class callbacks are skipped with a warning | Scripts run as external processes; handling of PHP class callbacks not checked |
| Platforms | macOS and Linux only (ADR 0005) | macOS, Linux and Windows, plus Homebrew, .deb and .rpm packages |
| Extras | Store commands, `diagnose` with store checks | `--dry-run` that changes nothing, NDJSON output, `bump`, `outdated`, `why`, `fund`, `patch`, `policy`, `reinstall`, shell completion, use as a Rust library, a `composer` executable alias |
| Conformance | 197/228 installer fixtures pass, and outputs are compared to real Composer runs | Composer fixtures ported to Rust, property tests and comparison scripts; its CI rejects any fixture still pending |

## Where each one wins

romposer's byte-for-byte matching is measured against real Composer runs.
Its store saves disk space across projects, and its code base is about 3 times smaller.

riff can install real projects that depend on Symfony Flex or composer-patches.
It is further along on packaging, Windows support and extra commands.

## Ideas worth taking from riff

- Rust versions of the Symfony Flex and composer-patches plugins, the biggest gap for Symfony projects today.
- A `--dry-run` that changes nothing.
- Stopping with an error on an unknown plugin instead of only warning, since a warning can silently leave a wrong `vendor/` behind.

## Benchmarks

The published numbers cannot be compared yet.
The two benchmarks use different machines, different Composer versions (2.11.1 vs 2.10.3) and different symfony/demo commits (`f42980c` vs `920d86d`).
A fair comparison means running riff through `bench/install.sh` on the same machine (see [benchmark.md](benchmark.md)).
