import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshotMatrix, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForButton, waitForMissingText, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('a[href="/config/provider"]').click();
    await waitForText(page, "#config-title", "Settings");
    await page.locator('a[href="/config/mcp"]').click();
    const mcpTabSelected = await mcpTabContextPreserved(page);
    await waitForText(page, '[aria-label="MCP server actions"]', "Create");
    await screenshotMatrix(page, "config-mcp-list-empty");

    await page.locator('[aria-label="MCP server actions"] button').click();
    const createServerRoute = await waitForPathname(page, "/config/mcp/create");
    const createServerTabSelected = await mcpTabContextPreserved(page);
    await waitForText(page, 'form[aria-label="Create MCP server"]', "Name");
    await screenshotMatrix(page, "config-mcp-create-form-empty");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create MCP server"] input[name="name"]',
      "localstdio",
    );
    await page
      .locator('form[aria-label="Create MCP server"] select[name="kind"]')
      .selectOption("stdio");
    await setInputValue(
      page,
      'form[aria-label="Create MCP server"] input[name="command"]',
      "npx local-mcp",
    );
    await page.locator('form[aria-label="Create MCP server"] button[type="submit"]').click();
    const createdServer = await waitForText(page, ".config__list", "localstdio");
    const createdCommand = await waitForText(page, ".config__list", "npx local-mcp");
    await waitForButton(page, "Edit localstdio");
    await screenshotMatrix(page, "config-mcp-list-created");

    await page.locator('button[aria-label="Edit localstdio"]').click();
    const editServerRoute = await waitForPathname(page, "/config/mcp/edit/localstdio");
    const editServerTabSelected = await mcpTabContextPreserved(page);
    await waitForText(page, 'form[aria-label="Edit MCP server"]', "Name");
    await screenshotMatrix(page, "config-mcp-edit-form-stdio");
    await page.locator('form[aria-label="Edit MCP server"] select[name="kind"]').selectOption("streamableHttp");
    await setInputValue(
      page,
      'form[aria-label="Edit MCP server"] input[name="url"]',
      "https://localhost:8787/mcp",
    );
    await page.locator('form[aria-label="Edit MCP server"] button[type="submit"]').click();
    const editedServer = await waitForText(page, ".config__list", "localstdio");
    const editedKind = await waitForText(page, ".config__list", "streamableHttp");
    const editedUrl = await waitForText(page, ".config__list", "https://localhost:8787/mcp");
    const editedCommandRemoved = await waitForMissingText(page, ".config__list", "npx local-mcp");

    await page.locator('button[aria-label="Delete localstdio"]').click();
    const deleteConfirm = await waitForText(
      page,
      '[aria-label="Delete localstdio confirmation"]',
      "Delete?",
    );
    await screenshotMatrix(page, "config-mcp-delete-confirmation");
    await page.locator('button[aria-label="Confirm delete localstdio"]').click();
    const deletedServer = await waitForMissingText(page, ".config__list", "localstdio");

    check({
      mcpTabSelected,
      createServerRoute,
      createServerTabSelected,
      createdServer,
      createdCommand,
      editServerRoute,
      editServerTabSelected,
      editedServer,
      editedKind,
      editedUrl,
      editedCommandRemoved,
      deleteConfirm,
      deletedServer,
    }, {
      "mcp server create edit delete works": ({
        mcpTabSelected,
        createServerRoute,
        createServerTabSelected,
        createdServer,
        createdCommand,
        editServerRoute,
        editServerTabSelected,
        editedServer,
        editedKind,
        editedUrl,
        editedCommandRemoved,
        deleteConfirm,
        deletedServer,
      }) =>
        mcpTabSelected &&
        createServerRoute &&
        createServerTabSelected &&
        includesText(createdServer, "localstdio") &&
        includesText(createdCommand, "npx local-mcp") &&
        editServerRoute &&
        editServerTabSelected &&
        includesText(editedServer, "localstdio") &&
        includesText(editedKind, "streamableHttp") &&
        includesText(editedUrl, "https://localhost:8787/mcp") &&
        !includesText(editedCommandRemoved, "npx local-mcp") &&
        includesText(deleteConfirm, "Delete?") &&
        !includesText(deletedServer, "localstdio"),
    });
  });
}

async function waitForPathname(page, expectedPathname) {
  const deadline = Date.now() + 10_000;
  let pathname = "";

  while (Date.now() < deadline) {
    pathname = await page.evaluate(() => window.location.pathname);

    if (pathname === expectedPathname) {
      return true;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for route ${expectedPathname}; saw ${pathname}`);
}

async function setInputValue(page, selector, value) {
  await page.evaluate(({ selector, value }) => {
    const input = document.querySelector(selector);

    if (!(input instanceof HTMLInputElement)) {
      throw new Error(`Input not found: ${selector}`);
    }

    input.value = value;
    input.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
  }, { selector, value });
}

async function mcpTabContextPreserved(page) {
  return page.evaluate(() => {
    const selectedTab = document.querySelector('a[href="/config/mcp"][aria-selected="true"]');
    return selectedTab?.textContent?.trim() === "MCP Servers";
  });
}
