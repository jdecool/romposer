# romposer

Composer, rewritten in Rust, installing from a shared package store.

romposer is observably identical to **Composer 2.11.1**: same `composer.json` in, same `composer.lock`, same `vendor/`, same generated autoloader, same `--format=json` outputs, same exit codes. A project can be installed by `composer` one day and by `romposer` the next with no diff in committed files.

Underneath, `vendor/` is materialised from the [pcomposer](https://github.com/jdecool/pcomposer) store: one content-addressable copy of every file per machine, hard-linked or cloned into each project. The store is the default install path, and its on-disk format and manifest are pcomposer's, so both tools share one store.

This is a personal, exploratory project. Its deliverables are the binary and two numbers below.

## Status

| Metric | Value |
|---|---|
| Composer installer fixtures passing | 197/228 (the rest need dependency policies or a composer repository served from a directory) |
| `update` lock vs composer | byte-identical on a live monolog + symfony/console + phpunit project (full, `--no-dev`, `--prefer-lowest`, `--lock`, partial) |
| `show` / `search` golden cases vs composer | 31 on a recorded Packagist + live `show symfony/console -a --format=json` |
| `audit` vs composer | 30 invocations identical (every format, `--locked`, installed, ignore rules, `install --audit`) on a live project pinning a vulnerable guzzle |
| scripts and events vs composer | every event in the same order, 52 invocations identical (installer events, `run-script`, `exec`, script aliases, failing listeners) |
| project commands vs composer | 83 `config` invocations (get, set, unset, `--list`, `--global`, `--json`, `--merge`, `--append`, auth keys), 17 `init`/`global`, 4 `clear-cache`, 6 `archive` entry listings, and 9 `create-project` runs identical; `create-project symfony/skeleton --no-plugins` gives the same lock and file listing |
| git repositories vs composer | identical lock, `installed.json`, `installed.php` and vendor checkout (HEAD, branches, remotes) for tags, branches, branch aliases, `only-branches`, the bare mirror cache and in-place source updates on a generated repository; a public GitHub package locks identically through the API driver and `no-api` |
| path, artifact and archive dists vs composer | identical lock, `installed.json`, `installed.php`, autoload, vendor/ listing (symlink targets included), stderr and `status -v` on a generated monorepo: globbed and mirrored `path` packages (one versioned from its git feature branch), an `artifact` directory with a tar.gz library and a zip metapackage, xz and phar dists; none reach the store |
| `install` on Symfony demo, cold / warm, vs Composer with and without pcomposer | see [Benchmark](#benchmark) |

## Benchmark

`install` of [Symfony demo](https://github.com/symfony/demo) at `f42980c`, median wall time of 3 runs, from `bench/install.sh 3` (`PCOMPOSER_SRC` pointing at a pcomposer checkout). Cold starts from an empty cache directory (no zip cache, no store), warm keeps it; `vendor/` is deleted before every run. All runs use `--no-scripts`, and the demo's plugins (flex, runtime) are disabled since romposer never runs them.

| install (median of 3) | composer | composer + pcomposer | romposer, store disabled | romposer, store enabled |
|---|---|---|---|---|
| cold | 12.56 s | 37.40 s | 10.03 s | 15.75 s |
| warm | 4.20 s | 8.14 s | 3.61 s | 4.19 s |

Machine: MacBook Pro (MacBookPro18,3), Apple M1 Pro, 16 GB, macOS 27.2, APFS, PHP 8.5.12, Composer 2.11.1, romposer built with `cargo build --release` (rustc 1.99.1), over a home connection. Numbers are after romposer's install optimisations: store files hashed while the zip inflates, zip entries streamed straight into the store, and packages extracted, ingested and linked on all cores while the output keeps Composer's order. Without the store, romposer is now faster than Composer cold and warm. With it, a cold run, which fills the store, costs about a quarter more than Composer's, and a warm run from it matches Composer; pcomposer's store still costs about 3x cold and 2x warm. The store's main gain stays disk space shared across projects. Installer fixtures: 197/228 (the `Installer fixtures pass rate` step of CI).

## Install

```sh
cargo install --git https://github.com/jdecool/romposer
```

Or download a binary (macOS arm64/x86_64, Linux x86_64/arm64) from the GitHub Releases page; each `v*` tag builds them.

macOS and Linux only.

## Limits

- PHP Composer plugins are installed but never executed (a warning names them).
- PHP class callbacks in `scripts` are skipped with a warning; shell scripts run as with Composer.
- svn, hg, fossil, perforce and forgejo repositories are not supported (an explicit error); git, GitHub, GitLab and Bitbucket are.
- No `self-update`; `diagnose` keeps only the checks that apply to romposer and adds the store.
- `config --editor` and `config --source` are not supported.
- `archive` archives the current project only, with the same entries as Composer but not the same bytes.
- `rar` dists need `unrar`, `phar` dists `php`, `tar`/`xz` dists the system `tar`.
- No Windows.
- Human-readable console output is close to Composer's but not byte-identical; machine outputs are.

## Layout

- `reference/composer`: git submodule of Composer at the pinned tag. Source of the JSON schema and the installer fixtures used by the conformance suite.
- `docs/adr/`: decisions. `GLOSSARY.md`: vocabulary.

## License

MIT. Parts are ported from Composer, © Nils Adermann and Jordi Boggiano.
