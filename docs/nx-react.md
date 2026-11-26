# Analyze an Nx workspace with React

Run Smella from the directory containing `nx.json`. Start by inspecting the entry files it selected. A production entry should lead to the code that starts the application, such as the file that calls `createRoot(...).render(<App />)`.

```js
import { analyze } from 'smella';

const report = await analyze({ root: process.cwd() });
try {
  console.table(report.roots);
  console.table(report.coverage.gaps.map(({ code, file, message }) => ({ code, file, message })));
} finally {
  report.dispose();
}
```

## How entries are selected

| Nx input | Smella behavior |
| --- | --- |
| Nested `project.json` | Registers the project directory for cross-project references, including projects without a package manifest. Honors a workspace-relative `root` override. Nx JSON files can contain comments and trailing commas. |
| Workspace `package.json#nx` and colocated `project.json` | Combines explicit targets. Project settings override matching package settings. A different executor replaces the lower executor's options. |
| `nx.json#targetDefaults` | Checks the executor key, then the exact target name, then simple matching target globs. Project options override defaults. The selected configuration overrides base options. `report.roots[].input` identifies the field supplying each entry. |
| Application with `@nx/webpack:webpack` or `@nx/esbuild:esbuild` | Selects the declared main and additional JS/TS entries. Webpack also supports injected scripts. Literal paths can use `{projectRoot}` and `{workspaceRoot}`. The corresponding `@nrwl` names are supported. |
| `@nx/js:node` or `@nx/webpack:dev-server` | Follows an explicit `project:target[:configuration]` to a supported build executor. Reports ambiguous, missing, or overridden build targets. |
| Library project | Registers its boundary. An application import, test, tool, or declared public surface supplies the context for each retained symbol. A build invoked by an application launcher supplies a runtime entry. |
| Tool configuration | Recognizes JS/TS paths declared in `webpackConfig`, `esbuildConfig`, `jestConfig`, `viteConfig`, `vitestConfig`, and `eslintConfig` as tooling entries. Test files use the ordinary test discovery rules. |
| TypeScript path aliases | Uses each importer's nearest `tsconfig.json` and its project references. At an Nx root with only `tsconfig.base.json`, uses that base for otherwise unconfigured files. |

Discovery covers the default configuration of each declared application target, plus configurations selected by supported launchers. Two deployment targets can therefore contribute different runtime entries. To analyze a single deployment, supply its bootstrap files explicitly. Nx describes how plugins, target defaults and project-level configuration contribute to its effective targets. [Nx project configuration](https://nx.dev/docs/reference/project-configuration)

### Plugin-inferred and customized applications

For a Vite app whose bootstrap comes from an HTML module script, use the script's source path. For Next.js file-system routing or a custom remote loader, list the application bootstraps you can identify. Framework-invoked exports can also require a public surface or a source adapter with explicit binding reads. Selecting a file as a runtime entry evaluates its top-level code.

```json
{
  "entries": {
    "runtime": ["products/store/src/client.tsx", "products/store/src/worker.ts"],
    "test": ["**/*.{spec,test}.{ts,tsx}", "products/store/tests/setup.ts"],
    "tooling": ["**/vite.config.ts", "tools/sync-catalog.ts"]
  },
  "publicEntries": ["packages/published-sdk/src/index.ts"]
}
```

Each supplied category replaces discovery for that role. Inspect `report.roots` after applying the configuration. For a stale `dist/` entry, supply the source bootstrap or public entry according to its role. For an orphaned caller, add the entry that executes it. Merely adding a file that exports an uncalled function can leave that function unretained.

Smella reads explicit metadata locally. Plugin-inferred targets, extended Nx presets, custom commands, dynamic bundler configuration and build-time file replacements can introduce additional entry or resolution behavior. Discovery surfaces these uncertainties for supported entry checks. Complex Nx merge operators, environment-selected profiles, bundler aliases and file-system routes require project-specific configuration or further analysis. A coverage status of `complete` is relative to the configured roots and supported model.

## Read the evidence for a React component

The fixture's storefront uses a JSX entry, a barrel, and a TS path alias. Its design library declares these components.

```tsx
export const Catalog = split(() => import("./Catalog"));
export const Preview = React.lazy(() =>
  import("./panels").then(p => ({ default: p.PreviewPanel }))
);
```

Smella resolves named, aliased, default and namespace imports from React by lexical binding. A literal lazy loader selects the module's default export. The single-property adapter above selects `PreviewPanel`. A destructured alias such as `.then(({ Panel: View }) => ({ default: View }))` also works. React calls a lazy loader when the component is first rendered. Smella follows a retained component statically. The witness identifies the source reference that reaches its loader. [React lazy reference](https://react.dev/reference/react/lazy)

| Query in the fixture | Expected evidence |
| --- | --- |
| `Catalog.tsx` default export | Runtime value witness from the storefront's JSX entry through the barrel and lazy loader. |
| `panels.tsx#PreviewPanel` | Complete test-only finding. The test renders `<UI.Preview />`. The unrelated panel export remains unused. |
| `pricing.ts` private binding `discount` | Conditional test-only finding. An unrooted checkout in another Nx project also reaches it through an imported function. |
| `Archive.tsx` module | Unreferenced. The evaluated routes module declares a memoized component that could render the archive, but no root retains that component. |

```js
const panel = report.explain({ file: 'shared/design/ui/src/panels.tsx', exportName: 'PreviewPanel' });
console.log(panel.kind, panel.tests);
const discount = report.explain({ file: 'shared/domain/pricing/src/pricing.ts', bindingName: 'discount' });
console.log(discount.status, discount.unrootedWorkspaceReads.map(read => read.path));
```

JSX tags and namespace members preserve their referenced bindings. Inline render callbacks passed to `memo`, `forwardRef` and `memo(forwardRef(...))` retain their dependencies with the owning component when used as its initializer. Arbitrary higher-order components use conservative initializer handling. Opaque lazy adapters leave a namespace gap for their target, and a reachable computed import or `import.meta.glob` can qualify findings across the project. Existing plain CSS and CSS Modules resolve as stylesheet inputs. Missing stylesheets and bundler loader queries appear as gaps. [Vite features](https://vite.dev/guide/features.html)

## Reproduce the Knip comparison

From the Smella checkout, build the release addon and run the following command. The harness creates a temporary copy of the fixture, supplies inert installed-package boundaries for React and React DOM, verifies Smella's witnesses and runs both Knip modes with the same application and test entries and TS aliases. It prints the file and export lists for each mode.

```sh
bun run build
KNIP_BIN=/path/to/knip bun scripts/bench-nx-react.mjs
```

On Knip 6.48.0 the default run omitted the `Preview` wrapper from unused exports. The production run listed it in both the barrel and the routes module. Both runs listed the unrooted legacy checkout as an unused file. Smella's single snapshot additionally identified the precise test reaching `PreviewPanel`, the unreferenced archive module, and the checkout's reference chain to the private pricing helper. This comparison covers a specific fixture and Knip's files/exports report. Knip also documents Nx support and integrated-workspace configuration. [Knip Nx plugin](https://knip.dev/reference/plugins/nx) · [Knip integrated monorepos](https://knip.dev/features/integrated-monorepos)

## Open coverage goals

- **Inferred Vite entries and route maps.** Recover the HTML bootstrap and literal glob-selected routes for two Nx apps with different Vite roots. Each route should have evidence from its own app. Keep dynamic patterns conditional.
- **Next.js and module federation.** Explain how a route or a remote exposes one component, including the selected export and the configuration that exposed it. Preserve separate classifications for neighboring exports.
- **Storybook and browser tests.** Attribute a component used only by a story, a Playwright spec or a production route to its configured context. Include the story or test file in the explanation and cover re-exports through a shared design library.

Each goal needs a fixture with an expected witness, a comparison on the same source and roots, and a release timing recorded alongside the current workloads.
