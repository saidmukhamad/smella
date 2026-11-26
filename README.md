# smella

Smella is a Rust-backed library for tracing JavaScript and TypeScript usage. Given an export, it shows which application entry points, tests, and tools reach it. It also checks whether a package exposes the export as part of its public API.

In the example below, searching the test files for `resetFixture` misses the caller. The test calls `prepare()`. Inside that helper, the same function is called `clear()`. Smella follows the re-exports back to the declaration and identifies the test.

## Try it on a fixture

Build from this checkout with Rust 1.88 or later and Bun 1.2 or later (also runs on Node 22 or later). The packaged native target is Linux x64 with glibc.

```sh
bun install
bun run build
```

From the repository root, ask which tests use `resetFixture`.

```sh
bun -e "
import { analyze } from 'smella';

const report = await analyze({ root: 'test/fixtures/contexts' });
try {
  const reset = report.explain({ file: 'src/service.ts', exportName: 'resetFixture' });
  console.log(reset.kind);                   // test-only-export
  console.log(reset.tests.map(t => t.file)); // [ 'tests/service.test.ts' ]
} finally {
  report.dispose();
}
"
```

The test reaches `resetFixture` through five files. The value witness in `reset.witnesses` gives the source location of each step along this path.

```text
tests/service.test.ts → tests/fixture.ts → src/index.ts → src/barrel.ts → src/service.ts
```

Use `report.findings({ kind: 'test-only-export' })` to list other exports with the same usage. Pass a finding’s `id` to `report.explain()` to inspect it. You can also ask about a private declaration with `{ file, bindingName }` or a whole module with `{ file }`. Finding IDs belong to one snapshot. Call `dispose()` to release it and `analyze` again after changing a file or an entry point. Witness locations use zero-based UTF-8 byte offsets with an exclusive end.

## Configure entry points

An entry point is where Smella starts tracing references. Use `runtime` for an application or CLI, `test` for test files, and `tooling` for scripts such as a build or linter. Use `publicEntries` for modules whose exports other packages can import. An exported function can have a test caller and still belong to the public API.

Put a `smella.config.json` at the repository root or pass the same fields to `analyze`. The package also exports a typed `createConfig` helper.

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

API options take precedence over the JSON file for each category. If neither supplies a category, Smella uses discovery. An explicit list replaces discovery for that category, and an empty list disables it. Paths and globs are relative to the analysis root.

Discovery picks up package `bin` targets, public `exports` or `main` and declared type entries, conventional test files, and common tool configs. It follows nested npm and Bun workspaces and the package patterns in `pnpm-workspace.yaml`. Check `report.roots` to see which files were selected and why. If your manifest points at generated output such as `dist/index.js`, supply the corresponding source entry. The scanner skips directories such as `dist` and `node_modules`, as well as symlinks. If no usable entry remains, `analyze` rejects with `ANALYSIS_FAILED`.

## Read a finding

| Finding | What Smella found |
| --- | --- |
| `test-only-export` | Only test entry points reach this export through a value or type reference. |
| `test-only-binding` | Only tests reach this private top-level declaration, which lives outside the test entry files. |
| `tooling-only-export` | Only tooling entry points reach this export through a value or type reference. |
| `unused-export` | Smella found no value reference, type reference, or public API path to the export. Loading the module can still run the export’s initializer. |
| `unreferenced-module` | Smella found no path that loads the module, uses its types, or exposes it through a public entry. |

The `usage` field records these facts separately. For example, `import './setup.js'` evaluates the module. It can run top-level registration code even if every named export in that file is unused.

Check a finding’s `status` before acting on it. A `conditional` finding has a gap that affects its conclusion. Its `gapIds` point into `report.coverage.gaps`, where you can see what Smella could not resolve. A missing import or a dynamically constructed module name can leave some callers unknown.

### When a workspace entry is missing

In the deep-workspace fixture, a CLI in package one calls `reset` in package three through a TypeScript alias and a re-export in package two. The CLI is missing from the runtime entries. Smella can see the reference in source, but only the tests have a known path from an entry point.

| Query in package three | Result | Evidence |
| --- | --- | --- |
| Export `reset` | `test-only-export`, conditional | `unrootedWorkspaceReads` shows the caller path from the CLI. Adding the CLI to `entries.runtime` establishes runtime usage. |
| Private helper `makeSample` | `test-only-binding`, complete | Only `alpha.test.ts` and `beta.test.ts` reach it. The missing CLI entry has no bearing on this helper. |

The same distinction matters in React. Loading a routes module can leave a lazy panel reachable only from a JSX test. In Nx workspaces, Smella reads application build targets and follows aliases across projects, including projects without a package manifest. [Workspace fixture](test/fixtures/deep-workspace) · [Nx and React examples](docs/nx-react.md)

## Analysis scope

Smella parses ES modules with Oxc and follows named imports, re-exports, literal dynamic imports, and type-only references. Resolution supports TypeScript path aliases, project references, and JS-to-TS extension mapping. Analysis reads the project without executing its code or configuration.

Once Smella reaches a function or class, it follows references in its body. A runtime value witness shows a possible read or call, including one inside a conditional branch. Computed imports and namespace objects passed to unknown code can produce coverage gaps. [API types, witnesses and coverage](lib/types.ts)

## Development

Run `bun run check` for Rust formatting/linting, TypeScript typechecking, and the Bun test suite (`bun run test`). After building, `node --test test/*.test.mjs` also verifies Node 22+ compatibility. The public corpus checks specific call paths against pinned checkouts of React, Nx, and Transformers.js. [Corpus setup](benchmarks/README.md#verify-upstream-behavior)

`bun run bench` measures fresh and resident processes on a 513-file export chain. Set `KNIP_BIN` to a Knip executable to compare both tools on the same expected findings. `bun run bench:record` saves raw timings and a plot in the Git-ignored `benchmarks/history/` directory. [Workloads, measurements and targets](benchmarks/README.md)

To package a Linux x64 glibc build, run `bun pm pack` in `npm/linux-x64-gnu` and again in the repository root. This produces tarballs for the native addon and its JavaScript loader.
