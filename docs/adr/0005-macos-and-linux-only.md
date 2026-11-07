# 0005: macOS and Linux only

Status: accepted, 2026-11-07

## Decision

romposer supports macOS and Linux. It refuses to run on Windows. CI runs on both supported platforms.

## Why

pcomposer is a no-op on Windows; the store relies on hard links, `clonefile(2)` and `FICLONE`; there is no Windows machine to test on. Not promising what is not verified.

## Consequences

Windows contributors to a project keep using Composer; nothing in the project files prevents that.
