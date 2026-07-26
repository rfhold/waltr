import { readFileSync } from "node:fs";

import { describe, expect, test } from "vitest";

describe("generated client access binding", () => {
  test("matches the attempt-scoped shared client export", () => {
    const binding = readFileSync("src/generated/waltr-client.ts", "utf8");

    expect(binding).toContain('export type AccessStatus = "unknownChecking" | "loginRequired" | "loginInProgress" | "authenticated" | "loginFailed";');
    expect(binding).toMatch(/export type AccessSnapshot = \{[\s\S]*status: AccessStatus,[\s\S]*activeAttemptId: string \| null,/);
    expect(binding).toMatch(/\{ "type": "completeWaltrLogin",[\s\S]*attemptId: string,/);
    expect(binding).toMatch(/\{ "type": "failWaltrLogin",[\s\S]*attemptId: string,[\s\S]*error: string,/);
    expect(binding).toMatch(/\{ "type": "cancelWaltrLogin",[\s\S]*attemptId: string,/);
    expect(binding).toMatch(/\{ "type": "waltrAccessExpired",[\s\S]*error: string,/);
  });
});
