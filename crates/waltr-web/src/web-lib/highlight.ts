import { createBundledHighlighter, createSingletonShorthands } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";

import { normalizeLanguage } from "./language";

const THEME = "github-dark";
const createHighlighter = createBundledHighlighter({
  engine: () => createJavaScriptRegexEngine(),
  langs: {
    bash: () => import("@shikijs/langs/bash"),
    css: () => import("@shikijs/langs/css"),
    diff: () => import("@shikijs/langs/diff"),
    html: () => import("@shikijs/langs/html"),
    javascript: () => import("@shikijs/langs/javascript"),
    json: () => import("@shikijs/langs/json"),
    jsx: () => import("@shikijs/langs/jsx"),
    markdown: () => import("@shikijs/langs/markdown"),
    rust: () => import("@shikijs/langs/rust"),
    toml: () => import("@shikijs/langs/toml"),
    tsx: () => import("@shikijs/langs/tsx"),
    typescript: () => import("@shikijs/langs/typescript"),
    yaml: () => import("@shikijs/langs/yaml"),
  },
  themes: {
    [THEME]: () => import("@shikijs/themes/github-dark"),
  },
});
const { codeToHtml } = createSingletonShorthands(createHighlighter);

export async function highlightedCodeHtml(code: string, language: string | undefined) {
  const lang = normalizeLanguage(language);
  if (!lang) {
    return undefined;
  }

  try {
    return await codeToHtml(code, { lang, theme: THEME });
  } catch {
    return undefined;
  }
}
