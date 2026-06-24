const LANGUAGE_ALIASES: Record<string, string> = {
  bash: "bash",
  console: "bash",
  diff: "diff",
  patch: "diff",
  shell: "bash",
  sh: "bash",
  terminal: "bash",
  zsh: "bash",
  js: "javascript",
  jsx: "jsx",
  md: "markdown",
  rs: "rust",
  ts: "typescript",
  tsx: "tsx",
  yml: "yaml",
};

const EXTENSION_LANGUAGES: Record<string, string> = {
  bash: "bash",
  css: "css",
  html: "html",
  js: "javascript",
  json: "json",
  jsx: "jsx",
  md: "markdown",
  rs: "rust",
  sh: "bash",
  toml: "toml",
  ts: "typescript",
  tsx: "tsx",
  yaml: "yaml",
  yml: "yaml",
  zsh: "bash",
};

export function normalizeLanguage(language: string | undefined) {
  const key = language?.trim().toLowerCase();
  if (!key) {
    return undefined;
  }

  return LANGUAGE_ALIASES[key] ?? key;
}

export function languageFromFilePath(path: string | undefined) {
  const extension = path?.split(/[?#]/, 1)[0]?.match(/\.([^.\/\\]+)$/)?.[1]?.toLowerCase();
  if (!extension) {
    return undefined;
  }

  return EXTENSION_LANGUAGES[extension];
}
