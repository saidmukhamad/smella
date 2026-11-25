# Release goals and benchmark history

1. **Identify test-only code.** All seven spec acceptance cases must pass through the typed API on Node 24 and Bun. For a private helper, identify each retaining test file and distinguish value usage from type usage. Runtime, tooling, or public retention must suppress a test-only finding.
2. **Explain cross-workspace references with imperfect entries.** Resolve the deep-workspace fixture through nested manifests, aliases and barrels. Show the source and location of a caller with no root path, qualify the affected binding, and recover runtime evidence when the caller's entry is supplied. Keep unrelated negative conclusions independent. Unknown dynamic targets must qualify the broader scope they can affect.
3. **Improve measured end-to-end time.** Check latency, peak RSS and shipped package size against the versioned reference-machine targets. Record a run before and after optimizing the slowest measured stage. Each entry retains its active thresholds so later changes to the targets remain visible. [Targets](targets.json)

4. **Preserve Nx and React evidence.** Discover the default application build from Nx metadata, distinguish runtime and test-only lazy components, and identify an unrooted Nx caller of a private helper. Gate a fresh snapshot, findings and both explanations at 3 ms p95 on the reference machine. [Fixture and interpretation](../docs/nx-react.md)

## Workloads

| Workload | What the timer includes | Correctness check |
| --- | --- | --- |
| Export chain | Analysis plus JS findings for 513 TypeScript files. Fresh-process timing also includes process launch, output and exit. | Exactly the same 512 file/export pairs as the fixture oracle and, when enabled, Knip. |
| Deep workspace | A fresh snapshot, findings, and a full private-binding explanation, in one resident process. The fixture has 32 packages, four nested workspace declarations above the package collection, 64 test roots and a missing caller entry. | All 64 value consumers and the ordered 32-package reference path must be present. |
| Nx and React | Analysis, findings and two full explanations for a TSX storefront with four Nx projects. | Assert the two runtime entries, test-only named panel, unused archive, production catalog and conditional private helper. Optional Knip default and production runs use explicit equivalent roots. |
| Native package | Compressed and unpacked file bytes of the actual JS and Linux x64 glibc tarballs. | The target-specific package must contain the native addon. |

Each latency series uses 20 samples and nearest-rank percentiles. The resident synthetic workloads use one warmup. Peak RSS on the export chain comes from the child process resource counters. Wall times use a warm filesystem. Hardware, kernel, toolchain, fixture version and Git revision are recorded in each run.

## First optimization cycle

On the matched 513-file workload, resident p95 went from 8.06 ms at `16539b5` to 3.46 ms at `5defc13`. On the latter revision, cold p50 was 41.13 ms for Smella and 225.29 ms for Knip 6.48.0. Both produced the same 512 unused export pairs.

The first 32-package run took 8.08 ms at p95 including its explanation. Many callers shared a suffix of the same reference path. Encoding each suffix once across the native boundary reduced full-query p95 to 2.00 ms, including findings, all 64 test origins, and the caller paths. The reference-machine target went from 25 ms to 6 ms. All seven published gates passed, including the package size checks. [Before entry](history/2026-11-25T15-13-06.523Z-c1b2300.json) · [After entry](history/2026-11-25T15-16-17.851Z-5defc13.json)

## Nx and React measurement

The first release build with Nx target discovery and React lazy-loader tracing took 1.08 ms at p95 for the four-project fixture, including all findings and two explanations. The target is 3 ms. On the matched 513-file chain, cold p95 was 46.41 ms for Smella and 241.52 ms for Knip 6.48.0. Each reported the same 512 unused exports. The Nx/React comparison records the actual Knip issue lists separately from the timed Smella run. [Measurement and comparison](history/2026-11-25T16-26-03.443Z-014d590.json)

## Verify upstream behavior

The corpus runner checks predictions against named upstream declarations, call sites and entry files. It reads three pinned public projects. Run this setup once in a new directory and keep the checkouts clean.

```sh
mkdir -p /tmp/smella-corpus
fetch_case() {
  git init -q "/tmp/smella-corpus/$1" &&
    git -C "/tmp/smella-corpus/$1" fetch -q --depth 1 "https://github.com/$2.git" "$3" &&
    git -C "/tmp/smella-corpus/$1" checkout -q --detach FETCH_HEAD
}
fetch_case transformers huggingface/transformers.js 890e398f3fe99e52e855ffc4f2c81fccbe762dd5
fetch_case nx nrwl/nx 3272c313f283c9cf4e2af868a2b3617fad280f40
fetch_case react facebook/react 92bb0678c52133a47b3095f6e54da883ea0d6cff
SMELLA_CORPUS_DIR=/tmp/smella-corpus npm run verify:corpus
```

| Repository | Assertions |
| --- | --- |
| Transformers.js | A real test reaches `pipeline` through the public barrel. Toggling the test and public entries changes the corresponding facts independently. A directory-driven suite carries a computed-import gap. |
| Nx | A devkit test reaches the private `toClassName` helper. Adding the public API produces a contract witness through both barrels. |
| React | The streaming-render fixture reaches the lazy `Comments` component in ordinary `.js` files. Removing its render site in a temporary copy drops both value and evaluation usage while `Sidebar` stays reachable. |

An assertion or revision mismatch fails the run. These checks establish the selected facts even when missing dependencies, Flow syntax or dynamic loaders keep the broader report partial. The React control edits only a temporary copy. [Verification runner](../scripts/verify-corpus.mjs)

## Record progress

From a checkout with Rust 1.88+, Node 24, npm, and Gnuplot with the pngcairo terminal, run:

```sh
npm run bench:record
```

This builds the release addon, writes a timestamped JSON entry, refreshes the plot, and returns a nonzero status for a missed budget. Failed measurements stay in the history. Use `npm run bench:check` to measure without writing history, and `npm run bench:plot` to regenerate the PNG from existing entries. Git status is recorded so development runs are distinguishable from pinned clean revisions.

Set `KNIP_BIN` to the executable from a separate installation of Knip 6.48.0 with TypeScript 6.0.3 to run the matched comparison. The independent behavioral comparison uses `KNIP_BIN=/path/to/knip node scripts/compare-test-usage.mjs`.

The plot groups runs matching the most recent machine and toolchain. Points correspond to stored runs. Dashed lines show today's targets, and missing measurements stay blank. Historical entries predating the tracker carry their source. [Measurement entries](history/)

![Smella latency by revision and current p95 targets](progress.png)
