import { For, Show, createMemo, type JSX } from "solid-js";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import { unified } from "unified";

import { languageFromFilePath } from "../../web-lib/language";
import { CodeBlock } from "./CodeBlock";

type HastNode = {
  type?: string;
  tagName?: string;
  value?: string;
  properties?: Record<string, unknown>;
  children?: HastNode[];
};

const markdownProcessor = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkRehype)
  .use(rehypeSanitize, {
    ...defaultSchema,
    attributes: {
      ...defaultSchema.attributes,
      code: [
        ...(defaultSchema.attributes?.code ?? []),
        ["className", /^language-[\w./=-]+$/],
      ],
    },
  });

export function MarkdownMessage(props: { text: string; tone: "user" | "assistant" | "thinking" }) {
  const tree = createMemo(() => markdownProcessor.runSync(markdownProcessor.parse(props.text)) as HastNode);

  return (
    <div class={`sessions__markdown sessions__markdown--${props.tone}`}>
      <For each={tree().children ?? []}>{renderNode}</For>
    </div>
  );
}

function renderNode(node: HastNode): JSX.Element {
  if (node.type === "text") {
    return node.value ?? "";
  }
  if (node.type !== "element") {
    return <For each={node.children ?? []}>{renderNode}</For>;
  }

  if (node.tagName === "pre") {
    const code = codeChild(node);
    if (code) {
      return <CodeBlock code={nodeText(code)} language={codeLanguage(code)} />;
    }
  }

  const children = () => <For each={node.children ?? []}>{renderNode}</For>;
  switch (node.tagName) {
    case "a":
      return <a href={stringProperty(node, "href")} rel="noreferrer" target="_blank">{children()}</a>;
    case "blockquote":
      return <blockquote>{children()}</blockquote>;
    case "br":
      return <br />;
    case "code":
      return <code class="sessions__inline-code">{children()}</code>;
    case "del":
      return <del>{children()}</del>;
    case "em":
      return <em>{children()}</em>;
    case "h1":
      return <h1>{children()}</h1>;
    case "h2":
      return <h2>{children()}</h2>;
    case "h3":
      return <h3>{children()}</h3>;
    case "h4":
      return <h4>{children()}</h4>;
    case "h5":
      return <h5>{children()}</h5>;
    case "h6":
      return <h6>{children()}</h6>;
    case "hr":
      return <hr />;
    case "li":
      return <li>{children()}</li>;
    case "ol":
      return <ol>{children()}</ol>;
    case "p":
      return <p>{children()}</p>;
    case "strong":
      return <strong>{children()}</strong>;
    case "table":
      return <div class="sessions__markdown-table"><table>{children()}</table></div>;
    case "tbody":
      return <tbody>{children()}</tbody>;
    case "td":
      return <td>{children()}</td>;
    case "th":
      return <th>{children()}</th>;
    case "thead":
      return <thead>{children()}</thead>;
    case "tr":
      return <tr>{children()}</tr>;
    case "ul":
      return <ul>{children()}</ul>;
    default:
      return <Show when={(node.children ?? []).length > 0}>{children()}</Show>;
  }
}

function codeChild(node: HastNode) {
  return (node.children ?? []).find((child) => child.type === "element" && child.tagName === "code");
}

function codeLanguage(node: HastNode) {
  const className = node.properties?.className;
  const classes = Array.isArray(className) ? className : typeof className === "string" ? [className] : [];
  const languageClass = classes.find((value) => typeof value === "string" && value.startsWith("language-"));
  if (typeof languageClass !== "string") {
    return undefined;
  }

  const language = languageClass.slice("language-".length);
  return languageFromFilePath(language) ?? language;
}

function nodeText(node: HastNode): string {
  if (node.type === "text") {
    return node.value ?? "";
  }

  return (node.children ?? []).map(nodeText).join("");
}

function stringProperty(node: HastNode, key: string) {
  const value = node.properties?.[key];
  return typeof value === "string" ? value : undefined;
}
