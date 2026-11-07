# romposer

Composer, rewritten in Rust, installing from a shared package store.

romposer is observably identical to **Composer 2.11.1**: same `composer.json` in, same `composer.lock`, same `vendor/`, same generated autoloader, same `--format=json` outputs, same exit codes. A project can be installed by `composer` one day and by `romposer` the next with no diff in committed files.

Underneath, `vendor/` is materialised from the [pcomposer](https://github.com/jdecool/pcomposer) store: one content-addressable copy of every file per machine, hard-linked or cloned into each project. The store is the default install path, and its on-disk format and manifest are pcomposer's, so both tools share one store.

This is a personal, exploratory project. Its deliverables are the binary and two numbers below.

## Status

| Metric | Value |
|---|---|
| Composer installer fixtures passing | _not measured yet_ |
| `install` on Symfony demo, cold / warm, vs Composer with and without pcomposer | _not measured yet_ |

## Install

```sh
cargo install --git https://github.com/jdecool/romposer
```

macOS and Linux only.

## Limits

- PHP Composer plugins are installed but never executed (a warning names them).
- PHP class callbacks in `scripts` are skipped with a warning; shell scripts run as with Composer.
- svn, hg, fossil and perforce repositories are not supported.
- No `self-update`.
- No Windows.
- Human-readable console output is close to Composer's but not byte-identical; machine outputs are.

## Layout

- `reference/composer`: git submodule of Composer at the pinned tag. Source of the JSON schema and the installer fixtures used by the conformance suite.
- `docs/adr/`: decisions. `GLOSSARY.md`: vocabulary.

## License

MIT. Parts are ported from Composer, © Nils Adermann and Jordi Boggiano.
