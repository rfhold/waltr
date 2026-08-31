import { fireEvent, screen } from "@solidjs/testing-library";
import { expect, test, vi } from "vitest";

import { MarkdownMessage } from "../../src/components/markdown/MarkdownMessage";
import { renderWithAppState } from "../support/render";

test("Markdown external links use the native opener without WebView navigation", () => {
  const openExternalUrl = vi.fn().mockResolvedValue(undefined);
  renderWithAppState(() => <MarkdownMessage text="[Waltr](https://example.com/docs)" tone="assistant" />, {
    openExternalUrl,
  });

  const link = screen.getByRole("link", { name: "Waltr" });
  expect(fireEvent.click(link)).toBe(false);
  expect(openExternalUrl).toHaveBeenCalledWith("https://example.com/docs");
});

test("Markdown links preserve browser anchor attributes without a native opener", () => {
  renderWithAppState(() => <MarkdownMessage text="[Waltr](https://example.com/docs)" tone="assistant" />);

  const link = screen.getByRole("link", { name: "Waltr" });
  expect(link.getAttribute("href")).toBe("https://example.com/docs");
  expect(link.getAttribute("target")).toBe("_blank");
});
