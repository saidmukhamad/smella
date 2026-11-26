export type Context = 'runtime' | 'test' | 'tooling';
export type Role = Context | 'public';

export type FindingKind =
  | 'unused-export'
  | 'test-only-export'
  | 'test-only-binding'
  | 'tooling-only-export'
  | 'unreferenced-module';

export type DependencyFindingKind =
  | 'unused-dependency'
  | 'unlisted-dependency'
  | 'unlisted-binary'
  | 'dev-dependency-in-runtime';

export interface DependencyFinding {
  id: string;
  kind: DependencyFindingKind;
  workspace: string;
  file: string;
  manifest: string;
  dependency?: string;
  dependencyGroup?: 'dependencies' | 'devDependencies';
  binary?: string;
  script?: string;
  location?: Location;
  status: 'complete';
  diff?: DiffStatus;
}

export interface SuppressionRule {
  file: string;
  symbol: string;
  reason: string;
}

export interface SuppressedFinding {
  workspace: string;
  file: string;
  symbol: string;
  kind: FindingKind;
  reason: string;
  source: 'config' | 'jsdoc';
  location: Location;
}

export interface StaleConfigItem {
  kind:
    | 'stale-entry'
    | 'stale-include'
    | 'stale-exclude'
    | 'stale-suppression'
    | 'stale-dynamic-import'
    | 'invalid-suppression';
  source: string;
  pattern?: string;
  file?: string;
  symbol?: string;
  location?: Location;
  message: string;
}

export interface Config {
  include?: readonly string[];
  exclude?: readonly string[];
  entries?: Partial<Record<Context, readonly string[]>>;
  publicEntries?: readonly string[];
  dynamicImports?: Readonly<Record<string, readonly string[]>>;
  suppressions?: readonly SuppressionRule[];
}

export interface AnalyzeOptions extends Config {
  root?: string;
  base?: string;
  staged?: boolean;
  mergeBase?: string | boolean;
  dependencies?: boolean;
}

export interface Root {
  file: string;
  role: Role;
  origin: 'explicit' | 'discovered';
  input: string;
}

/** Zero-based, half-open UTF-8 byte offsets within the source file. */
export interface Location {
  start: number;
  end: number;
}

export interface Gap {
  id: string;
  code: string;
  file: string | null;
  location: Location | null;
  message: string;
  affectedFiles: string[] | null;
  testOnly?: boolean;
}

export interface InlineGap {
  id: string;
  code: string;
  file: string | null;
  location: Location | null;
  message: string;
}

export interface Coverage {
  status: 'complete' | 'partial';
  files: string[];
  excluded: string[];
  gaps: Gap[];
}

export interface Usage {
  value: Context[];
  type: Context[];
  contract: boolean;
  evaluation: Context[];
}

export interface WorkspaceInfo {
  dir: string;
  name: string | null;
}

export interface SubjectRecord {
  id: string;
  workspace: string;
  file: string;
  exportName: string | null;
  bindingName?: string;
  location: Location;
  usage: Usage;
  status: 'complete' | 'conditional';
  gapIds: readonly string[];
  primaryGaps: readonly InlineGap[];
}

export type DiffStatus = 'introduced' | 'pre-existing';

export interface Finding extends SubjectRecord {
  kind: FindingKind;
  diff?: DiffStatus;
}

export interface DiffSummary {
  base: string;
  parents?: readonly string[];
  staged?: boolean;
  changedFiles: readonly string[];
  introduced: number;
  preExisting: number;
}

export interface Step {
  file: string;
  kind: string;
  name: string;
  location: Location;
  via: { file: string; location: Location } | null;
}

export interface Witness {
  role: Role;
  fact: 'value' | 'type' | 'evaluation' | 'contract';
  path: Step[];
}

export type TestFact = 'value' | 'type' | 'evaluation';

export interface TestConsumer {
  file: string;
  facts: TestFact[];
}

export interface WorkspaceRead {
  file: string;
  location: Location;
  fact: TestFact;
  path: Step[];
}

export interface Explanation extends SubjectRecord {
  kind: FindingKind | null;
  diff?: DiffStatus;
  reason: string;
  witnesses: Witness[];
  tests: TestConsumer[];
  unrootedWorkspaceReads: WorkspaceRead[];
}

export type Subject =
  | string
  | { file: string; exportName?: string; bindingName?: never }
  | { file: string; bindingName: string; exportName?: never };

export type AnalysisErrorCode =
  | 'NATIVE_LOAD'
  | 'ANALYSIS_FAILED'
  | 'REPORT_DISPOSED'
  | 'UNKNOWN_SUBJECT';

export interface DependencyFilter {
  kind?: DependencyFindingKind;
  diff?: DiffStatus;
  workspace?: string | readonly string[];
}

export interface FindingsFilter {
  kind?: FindingKind | DependencyFindingKind;
  diff?: DiffStatus;
  workspace?: string | readonly string[];
}

export interface Report {
  readonly roots: readonly Root[];
  readonly workspaces: readonly WorkspaceInfo[];
  readonly suppressed: readonly SuppressedFinding[];
  readonly staleConfig: readonly StaleConfigItem[];
  readonly coverage: Coverage;
  readonly diff?: DiffSummary;
  dependencies(filter?: DependencyFilter): DependencyFinding[];
  findings(filter?: FindingsFilter): (Finding | DependencyFinding)[];
  explain(subject: Subject): Explanation;
  dispose(): void;
}
