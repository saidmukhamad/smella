export type Context = 'runtime' | 'test' | 'tooling';
export type Role = Context | 'public';
export type FindingKind = 'unused-export' | 'test-only-export' | 'tooling-only-export' | 'unreferenced-module';
export interface Config {
  include?: readonly string[];
  exclude?: readonly string[];
  entries?: Partial<Record<Context, readonly string[]>>;
  publicEntries?: readonly string[];
}
export function createConfig<T extends Config>(config: T): T;
export interface Root { file: string; role: Role; origin: 'explicit' | 'discovered'; input: string }
/** Zero-based, half-open UTF-8 byte offsets within the source file. */
export interface Location { start: number; end: number }
export interface Gap { id: string; code: string; file: string | null; location: Location | null; message: string; affectedFiles: string[] | null }
export interface Coverage { status: 'complete' | 'partial'; files: string[]; excluded: string[]; gaps: Gap[] }
export interface Usage { value: Context[]; type: Context[]; contract: boolean; evaluation: Context[] }
interface SubjectRecord {
  id: string; file: string; exportName: string | null; location: Location; usage: Usage;
  status: 'complete' | 'conditional'; gapIds: string[];
}
export interface Finding extends SubjectRecord { kind: FindingKind }
export interface Step { file: string; kind: string; name: string; location: Location; via: { file: string; location: Location } | null }
export interface Witness { role: Role; fact: 'value' | 'type' | 'evaluation' | 'contract'; path: Step[] }
export interface Explanation extends SubjectRecord { kind: FindingKind | null; reason: string; witnesses: Witness[] }
export interface Report {
  readonly roots: readonly Root[];
  readonly coverage: Coverage;
  findings(filter?: { kind?: FindingKind }): Finding[];
  explain(subject: string | { file: string; exportName?: string }): Explanation;
  dispose(): void;
}
export class AnalysisError extends Error {
  readonly code: 'NATIVE_LOAD' | 'ANALYSIS_FAILED' | 'REPORT_DISPOSED' | 'UNKNOWN_SUBJECT';
}
export function analyze(options?: Config & { root?: string }): Promise<Report>;
