import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { WebSocket } from "ws";
import { startServer } from "../src/server.js";

let server: ReturnType<typeof startServer>;
let url = "";

before(async () => {
  server = startServer({ port: 0, host: "127.0.0.1", connectRateLimit: 5, log: () => {} });
  url = `ws://127.0.0.1:${await server.ready}`;
});

after(async () => {
  await server.close();
});

/** Minimal client with an awaitable message queue. */
async function client(target = url) {
  const ws = new WebSocket(target);
  const queue: any[] = [];
  const waiters: ((m: any) => void)[] = [];
  ws.on("message", (d) => {
    const m = JSON.parse(d.toString());
    const w = waiters.shift();
    if (w) w(m);
    else queue.push(m);
  });
  await new Promise((r) => ws.once("open", r));
  return {
    send: (m: object) => ws.send(JSON.stringify(m)),
    next: () =>
      new Promise<any>((resolve, reject) => {
        const m = queue.shift();
        if (m) return resolve(m);
        const t = setTimeout(() => reject(new Error("timeout waiting for message")), 2000);
        waiters.push((msg) => {
          clearTimeout(t);
          resolve(msg);
        });
      }),
    close: () => ws.close(),
  };
}

test("full session flow: register, connect, accept, sdp relay, hangup", async () => {
  const agent = await client();
  agent.send({ type: "register", device_id: "123456789", protocol: 1 });
  assert.deepEqual(await agent.next(), { type: "registered", device_id: "123456789" });

  const viewer = await client();
  viewer.send({ type: "connect", device_id: "123456789", password: "secret12", protocol: 1 });
  const req = await agent.next();
  assert.equal(req.type, "connect_request");
  assert.equal(req.password, "secret12");

  agent.send({ type: "accept", session_id: req.session_id });
  assert.deepEqual(await viewer.next(), { type: "accepted", session_id: req.session_id });

  viewer.send({ type: "sdp", session_id: req.session_id, sdp: "offer" });
  assert.deepEqual(await agent.next(), { type: "sdp", session_id: req.session_id, sdp: "offer" });
  agent.send({ type: "sdp", session_id: req.session_id, sdp: "answer" });
  assert.deepEqual(await viewer.next(), { type: "sdp", session_id: req.session_id, sdp: "answer" });

  viewer.send({ type: "hangup", session_id: req.session_id });
  assert.deepEqual(await agent.next(), { type: "hangup", session_id: req.session_id });
  assert.equal(server.stats().sessions, 0);
  agent.close();
  viewer.close();
});

test("offline device is rejected", async () => {
  const viewer = await client();
  viewer.send({ type: "connect", device_id: "999999999", password: "x", protocol: 1 });
  const m = await viewer.next();
  assert.equal(m.type, "rejected");
  assert.equal(m.reason, "device offline");
  viewer.close();
});

test("sdp cannot be injected into a session by a third party", async () => {
  const agent = await client();
  agent.send({ type: "register", device_id: "222222222", protocol: 1 });
  await agent.next();
  const viewer = await client();
  viewer.send({ type: "connect", device_id: "222222222", password: "pw", protocol: 1 });
  const req = await agent.next();
  agent.send({ type: "accept", session_id: req.session_id });
  await viewer.next();

  const attacker = await client();
  attacker.send({ type: "sdp", session_id: req.session_id, sdp: "evil" });
  assert.deepEqual(await attacker.next(), { type: "error", message: "unknown session" });
  attacker.send({ type: "register", device_id: "222222222", protocol: 1 });
  assert.deepEqual(await attacker.next(), { type: "error", message: "device_id already online" });
  [agent, viewer, attacker].forEach((c) => c.close());
});

test("malformed messages are rejected and connect attempts are rate limited", async () => {
  // Dedicated server: the limit is per IP and other tests share 127.0.0.1.
  const limited = startServer({ port: 0, host: "127.0.0.1", connectRateLimit: 5, log: () => {} });
  const c = await client(`ws://127.0.0.1:${await limited.ready}`);
  c.send({ type: "register", device_id: "not-digits", protocol: 1 });
  assert.equal((await c.next()).type, "error");
  for (let i = 0; i < 5; i++) {
    c.send({ type: "connect", device_id: "999999999", password: "x", protocol: 1 });
    assert.equal((await c.next()).reason, "device offline");
  }
  c.send({ type: "connect", device_id: "999999999", password: "x", protocol: 1 });
  assert.equal((await c.next()).reason, "too many attempts");
  c.close();
  await limited.close();
});
