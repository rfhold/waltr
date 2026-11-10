import type { InteractionFormField, InteractionFormValue, InteractionQuestionView } from "../../generated/waltr-client";

export type FormDraft = Record<string, InteractionFormValue | undefined>;

export function initialFormDraft(fields: InteractionFormField[]): FormDraft {
  return Object.fromEntries(fields.map(field => [field.name, field.default ?? undefined]));
}

export function questionAnswers(questions: InteractionQuestionView[], selections: string[][], custom: string[]): string[][] | string {
  const answers = questions.map((question, index) => [...new Set([
    ...(selections[index] ?? []), ...(question.custom && custom[index]?.trim() ? [custom[index]!.trim()] : []),
  ])]);
  if (answers.some((answer, index) => answer.length === 0 || (!questions[index]!.multiple && answer.length !== 1))) return "Answer each question; single-choice questions need exactly one answer.";
  return answers;
}

export function formValues(fields: InteractionFormField[], draft: FormDraft): Record<string, InteractionFormValue> | string {
  const values: Record<string, InteractionFormValue> = {};
  for (const field of fields) {
    let value = draft[field.name];
    const error = (reason: string) => `${field.title ?? field.name}: ${reason}`;
    if (value === undefined) {
      if (field.required) return error("a value is required.");
      continue;
    }
    const schema = field.schema;
    switch (schema.kind) {
      case "boolean":
        if (typeof value !== "boolean") return error("choose Yes or No.");
        break;
      case "number": {
        if (typeof value !== "string" && typeof value !== "number") return error("enter a number.");
        if (typeof value === "string" && !value.trim()) return error("enter a number.");
        value = Number(value);
        if (!Number.isFinite(value) || (schema.integer && !Number.isInteger(value))) return error(schema.integer ? "enter a finite integer." : "enter a finite number.");
        if (schema.minimum !== null && value < schema.minimum) return error(`minimum is ${schema.minimum}.`);
        if (schema.maximum !== null && value > schema.maximum) return error(`maximum is ${schema.maximum}.`);
        break;
      }
      case "string": {
        if (typeof value !== "string") return error("enter text.");
        const length = [...value].length;
        if (schema.minLength !== null && length < Number(schema.minLength)) return error(`minimum length is ${schema.minLength}.`);
        if (schema.maxLength !== null && length > Number(schema.maxLength)) return error(`maximum length is ${schema.maxLength}.`);
        if (schema.choices && !schema.choices.includes(value)) return error("select an offered choice.");
        if (schema.format && !validFormat(value, schema.format)) return error(`enter a valid ${schema.format}.`);
        break;
      }
      case "stringArray":
        if (!Array.isArray(value) || value.some(item => !schema.choices.includes(item)) || new Set(value).size !== value.length) return error("select offered choices only.");
        if (schema.minItems !== null && value.length < Number(schema.minItems)) return error(`select at least ${schema.minItems} choices.`);
        if (schema.maxItems !== null && value.length > Number(schema.maxItems)) return error(`select at most ${schema.maxItems} choices.`);
        break;
    }
    Object.defineProperty(values, field.name, { value, enumerable: true, configurable: true, writable: true });
  }
  return values;
}

function validFormat(value: string, format: string): boolean {
  switch (format) {
    case "email": return /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value);
    case "uri": try { return Boolean(new URL(value).protocol); } catch { return false; }
    case "date": {
      if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
      const date = new Date(`${value}T00:00:00Z`);
      return Number.isFinite(date.valueOf()) && date.toISOString().slice(0, 10) === value;
    }
    case "date-time": {
      const match = /^(\d{4}-\d{2}-\d{2})T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/i.exec(value);
      return Boolean(match && validFormat(match[1]!, "date") && Number.isFinite(Date.parse(value)));
    }
    default: return false;
  }
}
