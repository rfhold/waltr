export function normalizeWhitespace(text) {
  return text.replaceAll(/\s+/g, " ").trim();
}

export function includesText(actual, expected) {
  return compactText(actual).includes(compactText(expected));
}

function compactText(text) {
  return normalizeWhitespace(text).replaceAll(" ", "");
}
