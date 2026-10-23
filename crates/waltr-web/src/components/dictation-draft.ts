export type DraftSelection = { start: number; end: number };
export type DictationDraft = {
  update: (text: string) => boolean;
  rollback: () => void;
};

// Programmatic writes do not advance the user-edit revision.
export function dictationDraft(options: {
  read: () => { text: string; revision: number; selection: DraftSelection };
  write: (text: string, selection: DraftSelection) => void;
}): DictationDraft {
  const original = options.read();
  let expected = original;
  let attached = true;
  const owned = () => {
    const current = options.read();
    attached &&= current.revision === expected.revision
      && current.text === expected.text && current.selection.start === expected.selection.start
      && current.selection.end === expected.selection.end;
    return attached;
  };
  return {
    update(text) {
      if (!owned()) return false;
      const end = original.selection.start + text.length;
      const selection = { start: end, end };
      const value = original.text.slice(0, original.selection.start) + text + original.text.slice(original.selection.end);
      expected = { ...expected, text: value, selection };
      options.write(value, selection);
      return true;
    },
    rollback() {
      if (owned()) options.write(original.text, original.selection);
      attached = false;
    },
  };
}
