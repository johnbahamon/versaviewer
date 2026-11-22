import { startServer } from "./server.js";

const port = Number(process.env.PORT ?? 8080);
const host = process.env.HOST ?? "0.0.0.0";

const server = startServer({ port, host });
const actual = await server.ready;
console.log(`[signaling] listening on ws://${host}:${actual}`);

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.on(sig, async () => {
    await server.close();
    process.exit(0);
  });
}
