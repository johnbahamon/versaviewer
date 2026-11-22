// Mirror of crates/vv-protocol/src/signal.rs. Keep both in sync; the Rust
// tests pin the JSON shape (snake_case `type` tag).

export const PROTOCOL_VERSION = 1;

export type ClientMessage =
  | { type: "register"; device_id: string; protocol: number }
  | { type: "connect"; device_id: string; password: string; protocol: number }
  | { type: "accept"; session_id: string }
  | { type: "reject"; session_id: string; reason: string }
  | { type: "sdp"; session_id: string; sdp: string }
  | { type: "hangup"; session_id: string };

export type ServerMessage =
  | { type: "registered"; device_id: string }
  | { type: "connect_request"; session_id: string; password: string }
  | { type: "accepted"; session_id: string }
  | { type: "rejected"; session_id: string | null; reason: string }
  | { type: "sdp"; session_id: string; sdp: string }
  | { type: "hangup"; session_id: string }
  | { type: "error"; message: string };

const DEVICE_ID = /^[0-9]{9}$/;
const SESSION_ID = /^[0-9a-f-]{36}$/;
const MAX_FIELD = 64 * 1024;

function str(v: unknown, max = 256): v is string {
  return typeof v === "string" && v.length > 0 && v.length <= max;
}

/** Strict validation: anything unexpected is rejected, never coerced. */
export function parseClientMessage(raw: string): ClientMessage | null {
  let m: Record<string, unknown>;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return null;
    m = parsed as Record<string, unknown>;
  } catch {
    return null;
  }
  switch (m.type) {
    case "register":
      return str(m.device_id) && DEVICE_ID.test(m.device_id) && typeof m.protocol === "number"
        ? { type: "register", device_id: m.device_id, protocol: m.protocol }
        : null;
    case "connect":
      return str(m.device_id) && DEVICE_ID.test(m.device_id) && str(m.password, 128) &&
        typeof m.protocol === "number"
        ? { type: "connect", device_id: m.device_id, password: m.password, protocol: m.protocol }
        : null;
    case "accept":
    case "hangup":
      return str(m.session_id) && SESSION_ID.test(m.session_id)
        ? { type: m.type, session_id: m.session_id }
        : null;
    case "reject":
      return str(m.session_id) && SESSION_ID.test(m.session_id) && typeof m.reason === "string"
        ? { type: "reject", session_id: m.session_id, reason: m.reason.slice(0, 256) }
        : null;
    case "sdp":
      return str(m.session_id) && SESSION_ID.test(m.session_id) && str(m.sdp, MAX_FIELD)
        ? { type: "sdp", session_id: m.session_id, sdp: m.sdp }
        : null;
    default:
      return null;
  }
}
