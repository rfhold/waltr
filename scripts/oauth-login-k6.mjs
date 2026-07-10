import { spawnSync } from "node:child_process";

const authUrl = process.env.WALTR_OAUTH_AUTH_URL;

if (!authUrl) {
  console.error("skipping OAuth login: set WALTR_OAUTH_AUTH_URL");
  process.exit(77);
}

const dockerArgs = [
  "run",
  "--rm",
  "-v",
  `${process.cwd()}:/workspace:ro`,
  "-w",
  "/workspace",
  "-e",
  `K6_BROWSER_HEADLESS=${process.env.K6_BROWSER_HEADLESS ?? "true"}`,
  "-e",
  `WALTR_OAUTH_AUTH_URL=${authUrl}`,
  "-e",
  `WALTR_OAUTH_USERNAME=${process.env.WALTR_OAUTH_USERNAME ?? "waltr@example.test"}`,
  "-e",
  `WALTR_OAUTH_PASSWORD=${process.env.WALTR_OAUTH_PASSWORD ?? "password"}`,
  "grafana/k6:1.3.0-with-browser",
  "run",
  "tests/web/k6/oauth/login.browser.js",
];

const result = spawnSync("docker", dockerArgs, { encoding: "utf8" });

if (result.error) {
  console.error(`skipping OAuth login: ${result.error.message}`);
  process.exit(77);
}

process.stdout.write(result.stdout);
process.stderr.write(result.stderr);

const match = result.stdout.match(/WALTR_OAUTH_REDIRECT_URL=(\S+)/);

if (result.status !== 0) {
  process.exit(result.status ?? 1);
}

if (!match) {
  console.error("OAuth login completed without redirect URL output");
  process.exit(1);
}

console.log(match[1]);
