# First executable baseline

Smella revision `3e786383df4cd0ca79d16e0e0f52124507ae5524`. Measurements used Node 24.11.0, Rust 1.88.0, Linux x86-64 kernel 6.8.0-138-generic, glibc 2.35, and an Intel Core i5-12400. Bun API and tarball checks used Bun 1.3.3.

## Matched export workload

The benchmark generates 512 TypeScript modules and one application entry. Each module exports a live function and one unused constant. The entry calls the last function in the chain. Both tools resolve the same files and report the same 512 file/export pairs with zero unreferenced files. A mismatch fails the run.

| Measurement in milliseconds | p50 | p95 |
| --- | ---: | ---: |
| Fresh process importing the Smella JS entry | 18.15 | 19.92 |
| Fresh process running Smella and returning findings | 46.29 | 51.90 |
| Fresh process running Knip 6.48.0 and returning findings | 226.04 | 229.42 |
| Resident Smella process, fresh analysis plus findings | 6.83 | 7.67 |
| Resident analysis through delivery of report metadata | 5.41 | 6.25 |
| Resident findings query, native serialization and JS conversion | 1.40 | 1.61 |

Each row uses 20 observations with nearest-rank percentiles. The fresh-process measurements include process launch, JSON output and exit on a warm filesystem. The startup row loads the lazy JavaScript entry. First analysis includes addon loading. Smella's maximum fresh-process peak RSS was 62,464 KiB. Resident runs follow one warmup and reconstruct the entire graph for each observation.

Reproduce with `npm run bench`. For the comparison, install `knip@6.48.0` and `typescript@6.0.3` in a separate directory and set `KNIP_BIN` to its Knip executable. The harness alternates the order of the fresh processes. Knip uses an explicit entry, project glob, files and exports checks, the JSON reporter, and no cache. Smella uses the public embedding API from a Node process, explicit roots, and its default finding set. These numbers characterize the shared export workload. The fixture has no external packages, framework plugins, unknown loaders, or test/public classification work.

## Repository trials

**p-limit** v7.2.0 at `7bdd25c703337360236722ed7d4da5ffadcb7a08`. The inputs selected `test.js` for tests and `index.js` and `index.d.ts` as public surfaces. Smella reported both local test value usage and public retention for the default export. Six unresolved imports in the dependency-free checkout made all seven candidate findings conditional. [Reference revision](https://github.com/sindresorhus/p-limit/tree/7bdd25c703337360236722ed7d4da5ffadcb7a08)

## Package footprint

| Tarball | Compressed bytes | Unpacked file bytes |
| --- | ---: | ---: |
| JS loader, types, manifest, and README | 5,488 | 13,135 |
| Linux x64 glibc addon and manifest | 1,666,478 | 3,933,645 |
| Total | 1,671,966 | 3,946,780 |

Sizes come from `npm pack --json` for the release build. Both tarballs were installed into an empty consumer with npm offline and lifecycle scripts disabled. Node and Bun each loaded the installed addon and classified the test-only fixture through the public API.

The next release decision is how much root and import coverage the intended repositories need. The synthetic baseline is reproducible, while the repository trials expose resolver, CommonJS, and peak-memory work. Numeric release gates and additional native platforms remain open.
