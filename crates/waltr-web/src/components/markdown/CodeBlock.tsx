import { Show, createResource } from "solid-js";

import { highlightedCodeHtml } from "../../web-lib/highlight";
import { normalizeLanguage } from "../../web-lib/language";

export function CodeBlock(props: { code: string; language?: string; label?: string; class?: string }) {
  const language = () => normalizeLanguage(props.language);
  const [highlighted] = createResource(
    () => ({ code: props.code, language: language() }),
    ({ code, language }) => highlightedCodeHtml(code, language),
  );

  return (
    <figure class={`sessions__code-block${props.class ? ` ${props.class}` : ""}`}>
      <Show when={props.label || language()}>
        <figcaption class="sessions__code-block-header">{props.label ?? language()}</figcaption>
      </Show>
      <Show
        when={highlighted()}
        fallback={<pre class="sessions__code-block-body"><code>{props.code}</code></pre>}
      >
        {(html) => <div class="sessions__code-block-highlight" innerHTML={html()} />}
      </Show>
    </figure>
  );
}

export function HighlightedCode(props: { code: string; language?: string; class?: string }) {
  const language = () => normalizeLanguage(props.language);
  const [highlighted] = createResource(
    () => ({ code: props.code, language: language() }),
    ({ code, language }) => highlightedCodeHtml(code, language),
  );

  return (
    <Show
      when={highlighted()}
      fallback={<code class={`sessions__highlighted-code-fallback${props.class ? ` ${props.class}` : ""}`}>{props.code}</code>}
    >
      {(html) => <span class={`sessions__highlighted-code${props.class ? ` ${props.class}` : ""}`} innerHTML={html()} />}
    </Show>
  );
}
