# 0001: Iso means observable behaviour against a pinned Composer

Status: accepted, 2026-11-07

## Decision

romposer reproduces the observable behaviour of one exact Composer release (2.11.1, `reference/composer` submodule): `composer.lock` byte-for-byte, `vendor/` layout, every file under `vendor/composer/`, `--format=json` outputs and exit codes. Human-readable console output is close but not asserted. Internal architecture is not constrained by Composer's.

## Why

It is the only definition that can be tested: run both binaries on the same inputs and diff. Lock interchangeability keeps romposer usable on real projects shared with Composer users. Pinning gives a stable oracle; bumping is one submodule move plus a test run.

## Consequences

Composer's lock header fields (`_readme`, `plugin-api-version`) are written verbatim; nothing in the lock mentions romposer. The JSON writer must reproduce Composer's pretty-print. Composer's installer fixtures and golden diffs against `composer.phar` are the conformance suite.
