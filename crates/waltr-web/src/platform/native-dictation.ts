import type { DictationHost, DictationOperation, DictationStatus, RoutineSelection } from "./dictation";
import { validSelection } from "./dictation";
export type NativeDictationIdentity = { generation: number; operationId: number };
export type NativeDictationStart = NativeDictationIdentity & { selection: RoutineSelection; sessionId: string };
export type NativeDictationReply = NativeDictationIdentity & Omit<DictationStatus, "operationId">;
export type DictationInvoke = <T>(command: string, payload: Record<string, unknown>) => Promise<T>;
/** Rust owns capture, credentials, Interact and bounded coalesced presentation. No PCM crosses IPC. */
export function nativeDictation(invoke: DictationInvoke, generation: number): DictationHost {
  if (!Number.isSafeInteger(generation) || generation < 0) throw new Error("Invalid client generation");
  let disposed = false;
  let next = 0;
  let active: DictationOperation | undefined;
  return {
    livePreview: true,
    start(selection, sessionId) {
      if (disposed || active) throw new Error("Microphone is busy or unavailable.");
      if (!validSelection(selection) || !sessionId || sessionId.length > 256) throw new Error("Invalid dictation target");
      if (next >= Number.MAX_SAFE_INTEGER) throw new Error("Dictation operation limit");
      const identity: NativeDictationIdentity = { generation, operationId: ++next };
      let cancelled = false;
      let pending: Promise<DictationStatus> | undefined;
      let finished = false;
      const verify = (reply: NativeDictationReply): DictationStatus => {
        if (disposed || cancelled || reply.generation !== generation || reply.operationId !== identity.operationId) throw new Error("Stale dictation operation");
        if (reply.preview.length > 1024 * 1024 || (reply.finalText?.length ?? 0) > 1024 * 1024) throw new Error("Dictation text limit");
        if (["succeeded", "error", "cancelled"].includes(reply.phase) && active === operation) active = undefined;
        return reply;
      };
      const admission = invoke<NativeDictationReply>("dictation_start", { ...identity, selection, sessionId }).then(verify);
      void admission.catch(() => { if (active === operation) active = undefined; });
      const operation: DictationOperation = {
        operationId: identity.operationId,
        status() {
          if (!pending) pending = admission.then(() => invoke<NativeDictationReply>("dictation_status", identity)).then(verify).finally(() => { pending = undefined; });
          return pending;
        },
        async finish() {
          if (finished || cancelled) return;
          finished = true;
          await admission;
          if (!cancelled && !disposed) verify(await invoke<NativeDictationReply>("dictation_finish", identity));
        },
        cancel() {
          if (cancelled) return;
          cancelled = true;
          // Rust must tombstone this identity even if start admission is still pending.
          void invoke("dictation_cancel", identity).catch(() => undefined);
          if (active === operation) active = undefined;
        },
      };
      active = operation;
      return operation;
    },
    cancel() { active?.cancel(); },
    dispose() { disposed = true; active?.cancel(); },
  };
}
