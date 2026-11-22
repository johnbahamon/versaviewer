// Phase 0 signaling relay.
//
// Responsibilities: device presence (who is online under which ID), session
// brokering (connect -> accept/reject), and relaying SDP between the two
// peers of a session. It never sees media or input: those flow P2P/TURN.
//
// Not persisted, single instance. Phase 2 replaces this with the NestJS
// session service (Redis-backed presence for horizontal scaling, JWT auth,
// device certificates, audit log).

import { randomUUID } from "node:crypto";
import type { IncomingMessage } from "node:http";
import { WebSocket, WebSocketServer } from "ws";
import { PROTOCOL_VERSION, parseClientMessage, type ServerMessage } from "./protocol.js";

export interface ServerOptions {
  port: number;
  host?: string;
  /** Max connect attempts per IP per minute (brute-force protection). */
  connectRateLimit?: number;
  log?: (msg: string) => void;
}

interface Client {
  ws: WebSocket;
  ip: string;
  /** Set once this connection registered as an agent. */
  deviceId?: string;
  alive: boolean;
}

interface Session {
  id: string;
  deviceId: string;
  agent: Client;
  viewer: Client;
  accepted: boolean;
}

const MAX_MESSAGE_BYTES = 128 * 1024;
const HEARTBEAT_MS = 30_000;
const PENDING_SESSION_TTL_MS = 120_000;

export function startServer(opts: ServerOptions) {
  const log = opts.log ?? ((m: string) => console.log(`[signaling] ${m}`));
  const connectLimit = opts.connectRateLimit ?? 20;
  const devices = new Map<string, Client>();
  const sessions = new Map<string, Session>();
  const connectAttempts = new Map<string, number[]>();

  const wss = new WebSocketServer({
    port: opts.port,
    host: opts.host,
    maxPayload: MAX_MESSAGE_BYTES,
  });

  const send = (c: Client, msg: ServerMessage) => {
    if (c.ws.readyState === WebSocket.OPEN) c.ws.send(JSON.stringify(msg));
  };

  const peerOf = (s: Session, c: Client) => (s.agent === c ? s.viewer : s.agent);

  const endSession = (s: Session, notify?: Client) => {
    sessions.delete(s.id);
    if (notify) send(notify, { type: "hangup", session_id: s.id });
  };

  const rateLimited = (ip: string): boolean => {
    const now = Date.now();
    const recent = (connectAttempts.get(ip) ?? []).filter((t) => now - t < 60_000);
    recent.push(now);
    connectAttempts.set(ip, recent);
    return recent.length > connectLimit;
  };

  wss.on("connection", (ws: WebSocket, req: IncomingMessage) => {
    const client: Client = { ws, ip: req.socket.remoteAddress ?? "unknown", alive: true };
    ws.on("pong", () => (client.alive = true));

    ws.on("message", (data, isBinary) => {
      if (isBinary) return send(client, { type: "error", message: "binary frames not supported" });
      const msg = parseClientMessage(data.toString());
      if (!msg) return send(client, { type: "error", message: "invalid message" });

      switch (msg.type) {
        case "register": {
          if (msg.protocol !== PROTOCOL_VERSION) {
            return send(client, { type: "error", message: `unsupported protocol ${msg.protocol}` });
          }
          const existing = devices.get(msg.device_id);
          if (existing && existing !== client && existing.ws.readyState === WebSocket.OPEN) {
            // First come, first served. Phase 2: device keypair proves ownership.
            return send(client, { type: "error", message: "device_id already online" });
          }
          if (client.deviceId && client.deviceId !== msg.device_id) devices.delete(client.deviceId);
          client.deviceId = msg.device_id;
          devices.set(msg.device_id, client);
          log(`agent ${msg.device_id} online (${client.ip})`);
          return send(client, { type: "registered", device_id: msg.device_id });
        }

        case "connect": {
          if (rateLimited(client.ip)) {
            log(`rate limited connect attempts from ${client.ip}`);
            return send(client, { type: "rejected", session_id: null, reason: "too many attempts" });
          }
          if (msg.protocol !== PROTOCOL_VERSION) {
            return send(client, { type: "rejected", session_id: null, reason: "protocol mismatch" });
          }
          const agent = devices.get(msg.device_id);
          if (!agent || agent.ws.readyState !== WebSocket.OPEN) {
            return send(client, { type: "rejected", session_id: null, reason: "device offline" });
          }
          if (agent === client) {
            return send(client, { type: "rejected", session_id: null, reason: "cannot connect to self" });
          }
          const s: Session = {
            id: randomUUID(),
            deviceId: msg.device_id,
            agent,
            viewer: client,
            accepted: false,
          };
          sessions.set(s.id, s);
          setTimeout(() => {
            const cur = sessions.get(s.id);
            if (cur && !cur.accepted) {
              send(cur.viewer, { type: "rejected", session_id: s.id, reason: "timeout" });
              endSession(cur, cur.agent);
            }
          }, PENDING_SESSION_TTL_MS).unref();
          log(`session ${s.id}: viewer ${client.ip} -> agent ${msg.device_id}`);
          // The password is relayed for the agent to verify; it is never stored or logged.
          return send(agent, { type: "connect_request", session_id: s.id, password: msg.password });
        }

        case "accept":
        case "reject": {
          const s = sessions.get(msg.session_id);
          if (!s || s.agent !== client) return send(client, { type: "error", message: "unknown session" });
          if (msg.type === "accept") {
            s.accepted = true;
            log(`session ${s.id}: accepted`);
            return send(s.viewer, { type: "accepted", session_id: s.id });
          }
          log(`session ${s.id}: rejected (${msg.reason})`);
          send(s.viewer, { type: "rejected", session_id: s.id, reason: msg.reason });
          return endSession(s);
        }

        case "sdp": {
          const s = sessions.get(msg.session_id);
          if (!s || !s.accepted || (s.agent !== client && s.viewer !== client)) {
            return send(client, { type: "error", message: "unknown session" });
          }
          return send(peerOf(s, client), { type: "sdp", session_id: s.id, sdp: msg.sdp });
        }

        case "hangup": {
          const s = sessions.get(msg.session_id);
          if (!s || (s.agent !== client && s.viewer !== client)) return;
          log(`session ${s.id}: hangup`);
          return endSession(s, peerOf(s, client));
        }
      }
    });

    ws.on("close", () => {
      if (client.deviceId && devices.get(client.deviceId) === client) {
        devices.delete(client.deviceId);
        log(`agent ${client.deviceId} offline`);
      }
      for (const s of sessions.values()) {
        if (s.agent === client || s.viewer === client) endSession(s, peerOf(s, client));
      }
    });
  });

  const heartbeat = setInterval(() => {
    for (const ws of wss.clients) {
      const c = [...devices.values()].find((d) => d.ws === ws);
      // Clients we can't find in `devices` are viewers; ping them too.
      if (c && !c.alive) {
        ws.terminate();
        continue;
      }
      if (c) c.alive = false;
      ws.ping();
    }
    const now = Date.now();
    for (const [ip, ts] of connectAttempts) {
      if (ts.every((t) => now - t > 60_000)) connectAttempts.delete(ip);
    }
  }, HEARTBEAT_MS);
  heartbeat.unref();

  const ready = new Promise<number>((resolve) =>
    wss.on("listening", () => {
      const addr = wss.address();
      resolve(typeof addr === "object" && addr ? addr.port : opts.port);
    }),
  );

  return {
    ready,
    close: () =>
      new Promise<void>((resolve) => {
        clearInterval(heartbeat);
        for (const ws of wss.clients) ws.terminate();
        wss.close(() => resolve());
      }),
    stats: () => ({ devices: devices.size, sessions: sessions.size }),
  };
}
