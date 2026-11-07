# 0003: The pcomposer store is the default install path

Status: accepted, 2026-11-07

## Decision

Packages with a zip dist are installed from the content-addressable store by default, using pcomposer's exact on-disk format (`files/<hh>/<sha256>[-x]`, `index/`), manifest (`vendor/composer/pcomposer.json`), strategies, fallback rules, configuration keys (`extra.pcomposer.*`) and environment variables (`PCOMPOSER_*`). The store lives under Composer's cache directory. Disabling it yields the plain path, identical to Composer. The zip archive is not kept once ingested. Reflink uses the native `clonefile(2)` and `FICLONE` syscalls rather than `cp`.

## Why

The store is the reason this fork exists. Sharing pcomposer's format lets Composer-with-plugin and romposer alternate on one machine and reuses pcomposer's documented semantics as a second oracle. Native clone syscalls remove the macOS silent-copy limitation of `cp -c`.

## Consequences

The store boundary is pcomposer's: non-zip dists, source checkouts, `path` repositories, metapackages and non-empty targets go through the plain path and never appear in the manifest. Composer PHP on the same machine re-downloads zips it would otherwise find in its cache.
