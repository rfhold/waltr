import { cp, rm } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const source = fileURLToPath(new URL("../../waltr-web/dist", import.meta.url));
const destination = fileURLToPath(new URL("../frontend-dist", import.meta.url));

await rm(destination, { force: true, recursive: true });
await cp(source, destination, { recursive: true });
