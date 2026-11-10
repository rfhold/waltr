import type { RoutineSchema } from "../../generated/waltr-client";

export function initialParameters(schema: RoutineSchema): string {
  const value = (s: RoutineSchema): unknown => {
    switch (s.type) {
      case "null": case "nullable": return null;
      case "boolean": return false;
      case "number": return Math.max(s.min, Math.min(0, s.max));
      case "string": return "";
      case "array": return [];
      case "object": return Object.fromEntries(s.required.map(key => [key, value(s.properties[key]!)]));
    }
  };
  return JSON.stringify(value(schema), null, 2);
}

export function validateParameters(json: string, schema: RoutineSchema): string | undefined {
  if (new TextEncoder().encode(json).length > 65536) return "Parameters exceed 64 KiB.";
  let input: unknown;
  try { input = JSON.parse(json); } catch { return "Enter valid JSON parameters."; }
  const check = (value: unknown, s: RoutineSchema, path: string): string | undefined => {
    if (s.type === "nullable") return value === null ? undefined : check(value, s.schema, path);
    if (s.type === "null") return value === null ? undefined : `${path}: expected null.`;
    if (s.type === "string") return typeof value === "string" && Array.from(value).length <= s.maxLength ? undefined : `${path}: expected string (maximum ${s.maxLength} characters).`;
    if (s.type === "number") return typeof value === "number" && Number.isFinite(value) && value >= s.min && value <= s.max ? undefined : `${path}: expected number between ${s.min} and ${s.max}.`;
    if (s.type === "boolean") return typeof value === "boolean" ? undefined : `${path}: expected boolean.`;
    if (s.type === "array") {
      if (!Array.isArray(value) || value.length > s.maxItems) return `${path}: expected array (maximum ${s.maxItems} items).`;
      for (let i = 0; i < value.length; i++) { const error = check(value[i], s.items, `${path}[${i}]`); if (error) return error; }
    } else {
      if (!value || typeof value !== "object" || Array.isArray(value)) return `${path}: expected object.`;
      const object = value as Record<string, unknown>;
      for (const key of s.required) if (!Object.hasOwn(object, key)) return `${path}.${key}: required.`;
      for (const [key, entry] of Object.entries(object)) {
        if (!Object.hasOwn(s.properties, key)) return `${path}.${key}: unknown property.`;
        const error = check(entry, s.properties[key]!, `${path}.${key}`); if (error) return error;
      }
    }
  };
  // Validation never reserializes the caller's JSON or rounds opaque numeric values.
  return check(input, schema, "parameters");
}
