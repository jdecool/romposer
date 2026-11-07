# Glossary

- **Pinned Composer**: the exact Composer release romposer reproduces, held as the `reference/composer` submodule. Currently 2.11.1.
- **Iso**: observable equivalence with the pinned Composer: identical lock, `vendor/` layout, `vendor/composer/*`, machine-readable outputs and exit codes. Internal design is free; human console text is not asserted.
- **Store**: the per-machine content-addressable directory of package files, `<cache-dir>/pcomposer-store`, shared with the pcomposer plugin.
- **Index entry**: the JSON file in the store describing one `<name>@<version>-<reference>`: its files with hashes and modes, empty directories and symlinks.
- **Manifest**: `vendor/composer/pcomposer.json`, listing the packages of a project materialised from the store and the strategy used for each.
- **Strategy**: how a package's files reach `vendor/` from the store: `hardlink`, `reflink` (copy-on-write clone) or `copy`.
- **Ingest**: write an extracted package's files into the store and create its index entry.
- **Materialise**: create a package directory in `vendor/` from its index entry with a strategy.
- **Fallback**: per-package switch to `copy` when the configured strategy fails, with one warning per run.
- **Plain path**: the install path that bypasses the store and behaves exactly like Composer: used for non-zip dists, source checkouts, `path` repositories, metapackages, non-empty targets, and whenever the store is disabled.
- **Platform package**: `php`, `ext-*`, `lib-*` and the `composer-*-api` virtual packages, detected by shelling out to the `php` binary.
- **Fixture**: one `.test` file from Composer's installer test suite, run in-process by the fixture runner without network.
- **Golden test**: an end-to-end run of the pinned `composer.phar` and romposer on the same project, diffing the files and JSON outputs they produce.
- **Slice**: one delivery ticket; each yields a usable binary.
