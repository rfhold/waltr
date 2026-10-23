export type RoutineSelection = { routine: string; trigger: string; input: string; output: string };
export type DictationStatus = {
  operationId: number;
  phase: "starting" | "recording" | "processing" | "succeeded" | "error" | "cancelled";
  preview: string;
  finalText?: string | null;
};
export type DictationOperation = {
  operationId: number;
  status(): Promise<DictationStatus>;
  finish(): Promise<void>;
  cancel(): void;
};
export type DictationHost = {
  /** Opt in to provisional composer text. Absent or false keeps final-only insertion. */
  readonly livePreview?: boolean;
  /** Start synchronously in the initiating user gesture. No queued capture. */
  start(selection: RoutineSelection, sessionId: string): DictationOperation;
  cancel(): void;
  dispose(): void;
};
export function validSelection(value: unknown): value is RoutineSelection {
  if (!value || typeof value !== "object" || Object.keys(value).length !== 4) return false;
  return ["routine", "trigger", "input", "output"].every(key => {
    const field = (value as Record<string, unknown>)[key];
    return typeof field === "string" && /^[a-zA-Z0-9_./-]{1,128}$/.test(field);
  });
}
