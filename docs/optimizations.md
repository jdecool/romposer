# Install optimizations

These three changes take a store install of Symfony demo from 51.04 s to 15.75 s cold and from 9.34 s to 4.19 s warm. Numbers and method are in [benchmark.md](benchmark.md).

None of them changes what romposer prints, the order it prints in, or the vendor tree it leaves behind.

## Where the time went

Before these changes, a cold store install of each package went through these steps:

1. Extract the zip into a temporary folder.
2. Read every extracted file back to compute its SHA-256.
3. Copy each file into the store.
4. Link it into `vendor/`.

Packages were also handled one at a time. Composer itself does the extraction in parallel, so the store path was slower on two counts: the extra disk passes, and one CPU core doing all the work.

## 1. Hash while extracting (`f06d87d`)

`extract_zip_hashed` (`src/download.rs`) feeds each file to the SHA-256 hasher while it is being written. `Store::ingest_with_hashes` (`src/store/mod.rs`) takes those hashes and skips re-reading the file.

- **What it saves:** one full read of every extracted file.
- **When it falls back:** if two zip entries write the same path (for example, names that differ only in case on a case-insensitive disk), the earlier hashes can no longer be trusted. Every file is then hashed from disk, as before.

## 2. Stream zip entries straight into the store (`048a854`)

`ingest_zip` (`src/download.rs`) reads the zip twice:

1. **Check.** It lists the entries and gives up on any zip that the direct path can't handle safely:
   - zips that contain a symlink;
   - zips with a non-ASCII name;
   - zips where two names differ only in case;
   - zips where a file and a folder share a path.
2. **Write.** `Store::put_reader` inflates each entry into a temporary file inside `files/` and hashes it on the way. It then links the file to `files/<hh>/<sha256>` without overwriting an existing file, and `Store::write_entry` records the package index.

- **What it saves:** the temporary extract folder, and one write and one read of every file.
- **When it falls back:** if the check gives up, `ingest_zip` returns `false`, and the installer falls back to `extract_and_ingest` (`src/installer.rs`), which is extract-then-ingest with the hashing from section 1.

## 3. Prepare packages in parallel (`2209214`)

Before the install loop starts, `src/installer.rs` collects every operation that is a fresh zip install and handles them on a `std::thread::scope` worker pool. The pool has `available_parallelism` threads, and workers take jobs from a shared atomic counter.

Each job extracts the package, or ingests it into the store, and builds its folder in `vendor/`. The install loop that follows is unchanged. It walks the operations in Composer's order, prints each line, and picks up the job's result when it reaches that package. The output is therefore byte-identical to a sequential run.

The parallel step is skipped, and the install stays sequential, in these cases:

- updates, removals, metapackages, source (git) installs, path repositories and non-zip dists;
- a package whose install folder already exists, or whose folder overlaps another operation's folder (same folder, inside it, or around it);
- any project with a `pre-`/`post-package-install`, `update` or `uninstall` script, so that scripts still see each package arrive in order.

Shared helpers were made safe to call from several threads at once:
- folder creation in `src/fs.rs` tolerates another thread creating the same folder;
- store temporary file names are unique per process and per call;
- the installed-binaries code in `src/binaries.rs` was made thread-safe.

If a package fails, the loop stops at that package, as in a sequential run. The folders of packages that were already prepared further down the list are removed, so `vendor/` ends up as a sequential run would leave it.

## Known limits

All are marked `ponytail:` in the code.

- **Interrupted writes:** if romposer is killed while writing a file, the `files/.tmp-*` file stays in the store. `prune` never removes it.
- **Failed installs:**
  - packages prepared after the failing one stay in the store, unlike in a sequential run;
  - their downloaded archives are already deleted.
- **Overlap check:** the check for overlapping folders compares every pair of packages, so it is O(n²). That is negligible at real project sizes.

## Not done

- **Reflink as the default on macOS.** On APFS, a reflink (`clonefile`) gives each project its own copy-on-write file. That is faster for a warm install, and editing a file in `vendor/` no longer touches the store. The spec, ADR 0003 and the golden tests make hardlink the default to match pcomposer, so this stays an opt-in. Turn it on with `PCOMPOSER_STRATEGY=reflink` or `extra.pcomposer.strategy: "reflink"`.
