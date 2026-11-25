# smella

Smella explains which JavaScript and TypeScript exports your application, tests, tools, and public API retain. Ask about one export and get a source path through its consumers, local bindings, import aliases, and barrel files. An export can have test value usage, application type usage, and public-contract retention in the same snapshot.

## Run the development package

Use Rust 1.88 or later and Node 24 to build from this checkout. The compiled Node-API addon also runs on Bun. The first packaged target is Linux x64 with glibc.

```sh
npm test
npm run build
bun test test/analysis.test.mjs
```

From the repository root, run this example with Node 24 or Bun.

```js
import { analyze } from 'smella';

const report = await analyze({ root: 'test/fixtures/contexts' });
try {
  const reset = report.explain({ file: 'src/service.ts', exportName: 'resetFixture' });
  console.log(reset.kind); // test-only-export
  console.log(reset.witnesses.find(w => w.fact === 'value').path.map(s => s.file));
  console.log(report.findings({ kind: 'unused-export' }));
} finally {
  report.dispose();
}
```

## Configure one repository

Place a `smella.config.json` at the analysis root. The analyzer reads JSON, inspects source text, and writes no project files. Programs embedding the package can pass the same fields to `analyze` or use the typed `createConfig({...})` helper in their own code.

```json
{
  "include": ["src/**", "tests/**", "scripts/**", "packages/**"],
  "exclude": ["**/generated/**"],
  "entries": {
    "runtime": ["src/app.ts"],
    "test": ["tests/**/*.test.ts"],
    "tooling": ["scripts/**/*.ts"]
  },
  "publicEntries": ["packages/sdk/src/index.ts"]
}
```

Each API-supplied category overrides the corresponding config category. Each omitted category uses discovery. An empty array explicitly disables that category. Patterns match repository-relative paths with shell-style globs and brace alternatives. The walker skips symlinks and prunes directories named `.git`, `node_modules`, `dist`, `build`, `target`, `coverage`, `.next`, and `.cache`. The report lists scanned files and excluded paths.

Discovery recognizes manifest `bin` targets as runtime entries, `exports` or `main` and declared type entries as public surfaces, `*.test.*`, `*.spec.*` and `__tests__` as tests, and common bundler, linter, and test configuration filenames as tooling. Discovery follows nested npm/Bun workspace declarations. A root `pnpm-workspace.yaml` selects pnpm members using its `packages` patterns, including exclusions. For a manifest that points to generated output, supply the corresponding source entries. A start script with an unspecified runtime source produces an ambiguity diagnostic. For application bootstrap code outside these conventions, supply `entries.runtime`. Inspect `report.roots` for the role, origin, and selecting pattern or manifest field.

## Interpret a report

| Finding | Meaning within the effective roots and scope |
| --- | --- |
| `test-only-export` | Only tests retain the binding through value or type references. |
| `test-only-binding` | Only tests retain a private top-level declaration outside the configured test entry files. |
| `tooling-only-export` | Only tooling retains the binding through value or type references. |
| `unused-export` | No known value, type, or public-contract path retains the binding. Its initializer can still run. |
| `unreferenced-module` | No known evaluation, type-retention, or contract path reaches the module. |

Each record includes independent value and type contexts, public retention, and the containing module's evaluation contexts. Findings cover named export surfaces, including barrels, so a declaration and its re-export can both appear. Select an export with `{ file, exportName }`, a private binding with `{ file, bindingName }`, or a module with `{ file }`. Passing a finding ID selects the same subject. The witness for a known fact starts at an effective root. Each step has a declaration location, and its `via` field locates the reference in the preceding file. An unused binding can have an evaluation witness alongside its absence-of-reference explanation.

Paths are relative with forward slashes. Locations are zero-based, half-open UTF-8 byte offsets. Finding IDs belong to a single immutable snapshot. Calling `dispose()` releases that native snapshot and can be repeated. Later queries, including metadata getters, throw `AnalysisError` with code `REPORT_DISPOSED`. Previously returned data remains available.

A `partial` report contains located coverage gaps where possible. Conditional findings carry the corresponding gap IDs. A gap's `affectedFiles` lists its bounded scope, or is `null` for a project-wide uncertainty. Missing files, a computed dynamic import, or a namespace escape can weaken a negative conclusion. If every root is missing or unusable, `analyze` rejects with `ANALYSIS_FAILED`. Addon loading failures use `NATIVE_LOAD`.

## Trace tests and missing workspace entries

`explain().tests` enumerates the distinct configured test files that reach the selected subject and the facts each establishes. A test that only evaluates a module appears on the module explanation. A test that calls an exported wrapper can also appear on the explanation of its private helper.

```js
const report = await analyze({ root: 'test/fixtures/deep-workspace' });
try {
  const file = 'products/group/units/three/src/impl.ts';
  const sample = report.explain({ file, bindingName: 'makeSample' });
  console.log(sample.kind, sample.status, sample.tests);
  // test-only-binding, complete, alpha.test.ts and beta.test.ts

  const reset = report.explain({ file, exportName: 'reset' });
  console.log(reset.kind, reset.status); // test-only-export, conditional
  console.log(reset.unrootedWorkspaceReads.map(read => read.path));
} finally {
  report.dispose();
}
```

In that fixture, package one has a stale manifest entry. Its source still reads a function through a package-local TypeScript alias. Package two forwards the call to package three. Smella resolves this chain from the source files. The caller's execution role is unknown, so its reference qualifies the affected finding. Adding package one's CLI to `entries.runtime` establishes runtime usage. A neighboring private helper with only test consumers remains a complete test-only finding.

This evidence follows resolved binding reads and module loads from files outside every effective root path when a reference crosses a declared workspace boundary. An unread static import can explain evaluation and initializer dependencies. An unavailable target or ambiguous package name yields a resolution gap. For missing build outputs, provide a source path mapping or explicit source entry. A report still requires at least one usable root.

## Analysis boundary

- Oxc parses ESM JS, TS, JSX, and TSX and resolves lexical references. The Rust engine releases each syntax tree after extracting its graph edges. Breadth-first traversal records one predecessor per reached context and phase. Finding records and witness paths cross the native boundary when queried. The analysis itself runs on a libuv worker.
- Ordinary imports and re-exports evaluate their target modules. Explicitly type-only statements retain type scope independently. Resolution uses Node/import/default package conditions, the importing file’s nearest in-scope `tsconfig.json` paths and project references, JS-to-TS extension aliases, and npm workspace package maps. Type-only requests additionally admit declaration files and the types condition. Installed third-party packages and Node builtins end traversal. Unresolved bare packages produce gaps, including missing third-party installations.
- Named imports, local references, named re-exports, and unambiguous star chains preserve binding identity. Direct `ns.key`, literal `ns["key"]`, and `(await import("./x")).key` accesses select an export. A namespace that escapes into another value produces a bounded gap. Computed dynamic paths produce a project-wide gap when reachable.
- Functions and classes are analyzed conservatively once retained. The engine follows references inside their bodies without proving which calls or branches execute. Top-level initializers can retain their inputs regardless of export consumption. Local type declarations in execution entries contribute type dependencies. Class members, general object-property flow, arbitrary loaders, framework templates and CommonJS inference are outside this milestone. Consumers must supply framework-specific roots.
- Cleanup candidates describe static binding reachability. Use their evidence and coverage before editing source. The API never executes project code or suggests automatic deletion.

## Measurement and packaging

`npm run bench` builds in release mode and measures a deterministic 513-file graph in fresh and resident Node processes. Set `KNIP_BIN` to an installed Knip executable to include the exports-and-files comparison. The harness checks the full set of findings before accepting samples. The source fixture and runtime have different costs from a real repository with plugin discovery, unresolved imports, or many test roots.

`npm run build` produces the checkout addon. On Linux x64 with glibc it also stages the native package under `npm/linux-x64-gnu`. Run `npm pack` there and in the repository root to create the two installable tarballs. Both manifests are private during development.

The benchmark ledger records revisions, latency percentiles, memory, package sizes, and target checks. Use `npm run bench:record` to append a measurement and update the progress plot. [Release goals, workloads and history](benchmarks/README.md)
