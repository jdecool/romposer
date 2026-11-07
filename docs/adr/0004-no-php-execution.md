# 0004: No PHP code is executed by romposer, except scripts the user wrote as shell commands

Status: accepted, 2026-11-07

## Decision

Composer plugins (`type: composer-plugin`) are installed but never loaded. PHP class callbacks in `scripts` are skipped. Both produce a warning naming what was skipped. Shell script entries run with Composer's environment; `@composer` is executed in-process; `@php` uses the `php` binary on `PATH`. Platform packages are detected by shelling out to `php`.

## Why

Plugins and callbacks are PHP code that expects Composer's own classes in-process. Embedding an interpreter or reimplementing popular plugins natively would either negate the native binary or never end. Detecting extensions without running `php` is not reliable.

## Consequences

Projects relying on symfony/flex or similar plugins install but lose the plugin's effects. This is the stated limit of the fork and is documented in the README.
