#!/usr/bin/env node

/**
 * itermtabs — Rename iTerm2 tabs via the native WebSocket API.
 *
 * Usage:
 *   itermtabs <session-id> <new-title>
 *   itermtabs --list
 *
 * This connects to iTerm2's local WebSocket API (unix socket or TCP)
 * and sets the tab title override — the same as double-click renaming.
 * AppleScript and escape sequences cannot reliably do this.
 */

import { WebSocket } from "ws";
import { execSync } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { createConnection } from "node:net";
import http from "node:http";

// ── Minimal protobuf encoder ──

function encodeVarint(v) {
  const bytes = [];
  v >>>= 0;
  while (v > 0x7f) { bytes.push((v & 0x7f) | 0x80); v >>>= 7; }
  bytes.push(v & 0x7f);
  return Buffer.from(bytes);
}

function encodeTag(fieldNumber, wireType) {
  return encodeVarint((fieldNumber << 3) | wireType);
}

function encodeString(fieldNumber, value) {
  const buf = Buffer.from(value, "utf-8");
  return Buffer.concat([encodeTag(fieldNumber, 2), encodeVarint(buf.length), buf]);
}

function encodeDouble(fieldNumber, value) {
  const buf = Buffer.alloc(8);
  buf.writeDoubleLE(value);
  return Buffer.concat([encodeTag(fieldNumber, 1), buf]);
}

function encodeLengthDelimited(fieldNumber, buf) {
  return Buffer.concat([encodeTag(fieldNumber, 2), encodeVarint(buf.length), buf]);
}

// ── Minimal protobuf decoder ──

function decodeVarintAt(buf, off) {
  let value = 0, shift = 0;
  while (off < buf.length) {
    const b = buf[off++];
    value |= (b & 0x7f) << shift;
    shift += 7;
    if (!(b & 0x80)) break;
  }
  return { value, offset: off };
}

function decodeField(buf, off) {
  const { value: tag, offset: o1 } = decodeVarintAt(buf, off);
  const fn = tag >>> 3, wt = tag & 0x7;
  if (wt === 0) {
    const { value, offset: o2 } = decodeVarintAt(buf, o1);
    return { fn, wt, value, offset: o2 };
  }
  if (wt === 1) return { fn, wt, value: buf.slice(o1, o1 + 8), offset: o1 + 8 };
  if (wt === 2) {
    const { value: len, offset: o2 } = decodeVarintAt(buf, o1);
    return { fn, wt, value: buf.slice(o2, o2 + len), offset: o2 + len };
  }
  if (wt === 5) return { fn, wt, value: buf.slice(o1, o1 + 4), offset: o1 + 4 };
  throw new Error(`Unknown protobuf wire type ${wt}`);
}

function parseFields(buf) {
  const results = [];
  let off = 0;
  try {
    while (off < buf.length) {
      const f = decodeField(buf, off);
      results.push(f);
      off = f.offset;
    }
  } catch { /* partial parse is OK */ }
  return results;
}

// ── Message builders ──

function buildListSessionsRequest(id) {
  return Buffer.concat([
    Buffer.concat([encodeTag(1, 0), encodeVarint(id)]),
    encodeLengthDelimited(106, Buffer.alloc(0)),
  ]);
}

function buildGetVariableRequest(id, tabId, varName) {
  // variable_request = field 115; tab_id = field 4; get = field 3
  const inner = Buffer.concat([
    encodeString(4, tabId),
    encodeString(3, varName),
  ]);
  return Buffer.concat([
    Buffer.concat([encodeTag(1, 0), encodeVarint(id)]),
    encodeLengthDelimited(115, inner),
  ]);
}

function parseVariableResponse(buf) {
  // Response field 115 = variable_response; field 2 = values (repeated string)
  for (const f of parseFields(buf)) {
    if (f.fn === 115 && f.wt === 2) {
      for (const vf of parseFields(f.value)) {
        if (vf.fn === 2 && vf.wt === 2) {
          const val = vf.value.toString("utf-8");
          // Values are JSON-encoded strings
          try { return JSON.parse(val); } catch { return val; }
        }
      }
    }
  }
  return null;
}

function buildSetTitleRequest(id, tabId, title) {
  const escaped = title.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  const invocation = `iterm2.set_title(title: "${escaped}")`;
  const method = encodeString(1, tabId);
  const inner = Buffer.concat([
    encodeString(5, invocation),
    encodeDouble(6, -1.0),
    encodeLengthDelimited(7, method),
  ]);
  return Buffer.concat([
    Buffer.concat([encodeTag(1, 0), encodeVarint(id)]),
    encodeLengthDelimited(132, inner),
  ]);
}

// ── Session listing & tab resolution ──

function extractSessions(buf) {
  const sessions = [];

  for (const f106 of parseFields(buf)) {
    if (f106.fn !== 106 || f106.wt !== 2) continue;
    for (const fWin of parseFields(f106.value)) {
      if (fWin.fn !== 1 || fWin.wt !== 2) continue;
      for (const fTab of parseFields(fWin.value)) {
        if (fTab.fn !== 1 || fTab.wt !== 2) continue;
        const tabFields = parseFields(fTab.value);
        let tabId = null;
        for (const tf of tabFields) {
          if (tf.fn === 2 && tf.wt === 2) {
            const s = tf.value.toString("utf-8");
            if (/^\d+$/.test(s)) tabId = s;
          }
        }
        // Dig into root (f3) > split tree (f2) > session (f1) to get UUID and name
        for (const tf of tabFields) {
          if (tf.fn !== 3 || tf.wt !== 2) continue;
          for (const rootF of parseFields(tf.value)) {
            if (rootF.fn !== 2 || rootF.wt !== 2) continue;
            for (const sessF of parseFields(rootF.value)) {
              if (sessF.fn !== 1 || sessF.wt !== 2) continue;
              const sessFields = parseFields(sessF.value);
              let uuid = null, name = null;
              for (const sf of sessFields) {
                if (sf.fn === 1 && sf.wt === 2) uuid = sf.value.toString("utf-8");
                if (sf.fn === 4 && sf.wt === 2) {
                  // Name field has a short binary prefix; extract the readable part
                  const raw = sf.value.toString("utf-8");
                  const match = raw.match(/[\x20-\x7e]{3,}/);
                  name = match ? match[0].trim() : raw.replace(/[\x00-\x1f]/g, "").trim();
                }
              }
              if (uuid) sessions.push({ sessionId: uuid, tabId, name: name || "" });
            }
          }
        }
      }
    }
  }
  return sessions;
}

function findTabIdForSession(buf, targetSessionId) {
  for (const f106 of parseFields(buf)) {
    if (f106.fn !== 106 || f106.wt !== 2) continue;
    for (const fWin of parseFields(f106.value)) {
      if (fWin.fn !== 1 || fWin.wt !== 2) continue;
      for (const fTab of parseFields(fWin.value)) {
        if (fTab.fn !== 1 || fTab.wt !== 2) continue;
        const tabFields = parseFields(fTab.value);
        let tabId = null;
        for (const tf of tabFields) {
          if (tf.fn === 2 && tf.wt === 2) {
            const s = tf.value.toString("utf-8");
            if (/^\d+$/.test(s)) tabId = s;
          }
        }
        if (tabId && fTab.value.toString("utf-8").includes(targetSessionId)) {
          return tabId;
        }
      }
    }
  }
  return null;
}

// ── Auth ──

function getCookieAndKey(appName = "itermtabs") {
  try {
    const result = execSync(
      `osascript -e 'tell application "iTerm2" to request cookie and key for app named "${appName}"'`,
      { timeout: 5000, encoding: "utf-8" },
    ).trim();
    const [cookie, key] = result.split(" ");
    if (!cookie || !key) return null;
    return { cookie, key };
  } catch {
    return null;
  }
}

// ── Connection ──

const SOCKET_PATH = join(homedir(), "Library", "Application Support", "iTerm2", "private", "socket");
const TIMEOUT_MS = 5000;

function createIterm2WebSocket(auth) {
  const headers = {
    "origin": "ws://localhost/",
    "x-iterm2-library-version": "node 1.0",
    "x-iterm2-disable-auth-ui": "true",
    "x-iterm2-cookie": auth.cookie,
    "x-iterm2-key": auth.key,
    "x-iterm2-advisory-name": "itermtabs",
  };

  if (existsSync(SOCKET_PATH)) {
    const agent = new http.Agent();
    agent.createConnection = (_, cb) => createConnection(SOCKET_PATH, cb);
    return new WebSocket("ws://localhost/", ["api.iterm2.com"], { headers, agent });
  }

  return new WebSocket("ws://localhost:1912", ["api.iterm2.com"], { headers });
}

// ── Commands ──

async function listSessions() {
  const auth = getCookieAndKey();
  if (!auth) { console.error("Failed to get iTerm2 auth cookie. Is iTerm2 running?"); process.exit(1); }

  const ws = createIterm2WebSocket(auth);

  return new Promise((resolve) => {
    const timer = setTimeout(() => { ws.close(); console.error("Timeout"); process.exit(1); }, TIMEOUT_MS);
    let msgId = 0;
    let sessions = [];
    const tabTitles = new Map(); // tabId -> title
    const pendingTabs = [];      // tab IDs waiting for title response
    let phase = "list";          // "list" | "titles"

    ws.on("open", () => {
      const id = ++msgId;
      ws.send(buildListSessionsRequest(id));
    });

    ws.on("message", (data) => {
      const buf = Buffer.from(data);

      if (phase === "list") {
        sessions = extractSessions(buf);
        if (sessions.length === 0) {
          clearTimeout(timer);
          console.log("No sessions found.");
          ws.close();
          resolve();
          return;
        }
        // Collect unique tab IDs and request their titles
        const uniqueTabs = [...new Set(sessions.map(s => s.tabId).filter(Boolean))];
        phase = "titles";
        for (const tabId of uniqueTabs) {
          const id = ++msgId;
          pendingTabs.push({ id, tabId });
          ws.send(buildGetVariableRequest(id, tabId, "title"));
        }
        return;
      }

      // Phase "titles" — collect responses
      // Extract response id
      let responseId = -1;
      try {
        let off = 0;
        while (off < buf.length) {
          const f = decodeField(buf, off);
          off = f.offset;
          if (f.fn === 1 && f.wt === 0) { responseId = f.value; break; }
        }
      } catch { /* */ }

      const pending = pendingTabs.find(p => p.id === responseId);
      if (pending) {
        const title = parseVariableResponse(buf);
        if (title) tabTitles.set(pending.tabId, title);
        pending.done = true;
      }

      // Check if all titles received
      if (pendingTabs.every(p => p.done)) {
        clearTimeout(timer);
        console.log("iTerm2 Tabs:\n");
        const seenTabs = new Set();
        for (const s of sessions) {
          if (seenTabs.has(s.tabId)) continue;
          seenTabs.add(s.tabId);
          const title = tabTitles.get(s.tabId) || s.name || "";
          console.log(`  ${String(s.tabId).padStart(3)}  ${title}`);
        }
        ws.close();
        resolve();
      }
    });

    ws.on("error", (err) => {
      clearTimeout(timer);
      console.error(`Connection error: ${err.message}`);
      process.exit(1);
    });
  });
}

async function setTabTitle(target, newTitle) {
  const auth = getCookieAndKey();
  if (!auth) { console.error("Failed to get iTerm2 auth cookie. Is iTerm2 running?"); process.exit(1); }

  const ws = createIterm2WebSocket(auth);

  return new Promise((resolve, reject) => {
    let msgId = 0;
    let sessions = [];
    const tabTitleQueries = [];
    const tabTitles = new Map();
    let phase = "list";
    const timer = setTimeout(() => { ws.close(); reject(new Error("Timeout")); }, TIMEOUT_MS);

    function extractResponseId(buf) {
      try {
        let off = 0;
        while (off < buf.length) {
          const f = decodeField(buf, off);
          off = f.offset;
          if (f.fn === 1 && f.wt === 0) return f.value;
        }
      } catch { /* */ }
      return -1;
    }

    function resolveTarget() {
      // 1. Try tab ID (numeric)
      if (/^\d+$/.test(target)) {
        const s = sessions.find(s => s.tabId === target);
        if (s) return s.tabId;
      }
      // 2. Try session UUID
      const byUuid = sessions.find(s => s.sessionId === target);
      if (byUuid) return byUuid.tabId;
      // 3. Try tab title substring (case-insensitive)
      const matches = [];
      for (const [tabId, title] of tabTitles) {
        if (title.toLowerCase().includes(target.toLowerCase())) {
          matches.push({ tabId, title });
        }
      }
      if (matches.length === 1) return matches[0].tabId;
      if (matches.length > 1) {
        const names = matches.map(m => `"${m.title}"`).join(", ");
        throw new Error(`Ambiguous: "${target}" matches ${matches.length} tabs: ${names}`);
      }
      return null;
    }

    ws.on("open", () => {
      const id = ++msgId;
      ws.send(buildListSessionsRequest(id));
    });

    ws.on("message", (data) => {
      const buf = Buffer.from(data);

      if (phase === "list") {
        sessions = extractSessions(buf);
        // Quick check: if target is a UUID, we can skip title resolution
        const byUuid = sessions.find(s => s.sessionId === target);
        if (byUuid) {
          phase = "set";
          const id = ++msgId;
          ws.send(buildSetTitleRequest(id, byUuid.tabId, newTitle));
          return;
        }
        // If target is a numeric tab ID, try direct match
        if (/^\d+$/.test(target)) {
          const byTab = sessions.find(s => s.tabId === target);
          if (byTab) {
            phase = "set";
            const id = ++msgId;
            ws.send(buildSetTitleRequest(id, byTab.tabId, newTitle));
            return;
          }
        }
        // Otherwise need to resolve by name — query tab titles
        phase = "titles";
        const uniqueTabs = [...new Set(sessions.map(s => s.tabId).filter(Boolean))];
        for (const tabId of uniqueTabs) {
          const id = ++msgId;
          tabTitleQueries.push({ id, tabId, done: false });
          ws.send(buildGetVariableRequest(id, tabId, "title"));
        }
        return;
      }

      const responseId = extractResponseId(buf);

      if (phase === "titles") {
        const q = tabTitleQueries.find(q => q.id === responseId);
        if (q) {
          const title = parseVariableResponse(buf);
          if (title) tabTitles.set(q.tabId, title);
          q.done = true;
        }
        if (tabTitleQueries.every(q => q.done)) {
          const tabId = resolveTarget();
          if (!tabId) {
            clearTimeout(timer);
            ws.close();
            reject(new Error(`No tab matching "${target}"`));
            return;
          }
          phase = "set";
          const id = ++msgId;
          ws.send(buildSetTitleRequest(id, tabId, newTitle));
        }
        return;
      }

      if (phase === "set") {
        clearTimeout(timer);
        ws.close();
        console.log(`Tab title set to "${newTitle}"`);
        resolve();
      }
    });

    ws.on("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });
}

// ── CLI ──

const args = process.argv.slice(2);

if (args.length === 0 || args.includes("--help") || args.includes("-h")) {
  console.log(`itermtabs — Rename iTerm2 tabs via the native WebSocket API

Usage:
  itermtabs --list                  List all tabs
  itermtabs <target> <new-title>   Rename a tab

Target can be:
  - Tab ID (number from --list)
  - Tab name or substring (case-insensitive, must be unique)
  - Session UUID

Examples:
  itermtabs --list
  itermtabs 77 "My Project"
  itermtabs "Jobs" "Job Search"
`);
  process.exit(0);
}

if (args[0] === "--list" || args[0] === "-l") {
  await listSessions();
} else if (args.length >= 2) {
  const sessionId = args[0];
  const title = args.slice(1).join(" ");
  try {
    await setTabTitle(sessionId, title);
  } catch (err) {
    console.error(`Error: ${err.message}`);
    process.exit(1);
  }
} else {
  console.error("Usage: itermtabs <session-id> <new-title>");
  console.error("       itermtabs --list");
  process.exit(1);
}
