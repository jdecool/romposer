# Benchmark

`install` of [Symfony demo](https://github.com/symfony/demo) at `f42980c`, run with `bench/install.sh 3`, with `PCOMPOSER_SRC` pointing at a pcomposer checkout.

## Method

- Each cell is the median wall time of 3 runs.
- A cold run starts from an empty cache directory, so the zip cache and the store are both empty. A warm run reuses the cache directory left by the previous run.
- `vendor/` is deleted before every run.
- Every run uses `--no-scripts`. The demo's plugins (flex, runtime) are disabled, since romposer never runs them.
- Machine: MacBook Pro (MacBookPro18,3) with an Apple M1 Pro and 16 GB, running macOS 27.2 on APFS, over a home connection.
- Software: PHP 8.5.12 and Composer 2.11.1. romposer is built with `cargo build --release` (rustc 1.99.1).
- Pass absolute paths in `COMPOSER_CACHE_DIR` and `BENCH_DIR`. A relative cache dir is resolved against `-d`, so the store lands inside the demo project.

## Results

| install (median of 3) | composer | composer + pcomposer | romposer, store disabled | romposer, store enabled |
|---|---|---|---|---|
| cold | 12.56 s | 37.40 s | 10.03 s | 15.75 s |
| warm | 4.20 s | 8.14 s | 3.61 s | 4.19 s |

- **Without the store**, romposer is faster than Composer both cold and warm.
- **With the store**, a cold run costs about 25% more than Composer, because it fills the store. A warm run matches Composer.
- **pcomposer** still costs about 3x Composer cold and 2x warm.
- The store's main gain is still disk space shared across projects, not speed.

## Optimisations

Before these changes, romposer with the store took 51.04 s cold and 9.34 s warm. How each one works: [optimizations.md](optimizations.md).

| commit | change |
|---|---|
| `f06d87d` | Each store file's SHA-256 is computed while the zip inflates, instead of rereading every file afterwards. |
| `048a854` | Each zip entry streams straight into its store file, with no temporary extract directory and no extra copy. Zips with symlinks, non-ASCII names, case collisions or file/dir conflicts keep the extract-then-ingest path. |
| `2209214` | Packages are extracted, ingested and linked on all cores before the install loop, which still prints in Composer's order. |

The parallel step stays sequential in these cases:
- updates and removals;
- git, path and non-zip installs;
- packages whose install folders overlap;
- any project with package-event scripts.

`2209214` on its own, 2 runs each:

| | before | after |
|---|---|---|
| cold, store | 21.8 / 20.1 s | 15.7 / 15.4 s |
| warm, store | 9.6 / 6.9 s | 4.5 / 4.3 s |
| warm, no store | 5.1 / 4.8 s | 4.2 / 5.0 s |

The output and the vendor tree are byte-identical to before the optimisations.

## Not done

- **Reflink as the default on macOS.** The spec and ADR 0003 make hardlink the default to match pcomposer. `PCOMPOSER_STRATEGY=reflink` or `extra.pcomposer.strategy: "reflink"` opts in to reflink.
