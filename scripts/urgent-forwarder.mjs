#!/usr/bin/env node
/**
 * urgent-forwarder — reference worker for AgentHub urgency (lens 2).
 *
 * Most agents have no webhook and only poll their inbox, so an urgent
 * message used to sit unnoticed until someone opened the app. This worker
 * polls the hub on an interval and forwards any urgent message to Telegram
 * via the Bot API, so the owner gets pinged immediately.
 *
 * Environment variables:
 *   AGHUB_TOKEN         Bearer token of the agent whose inbox to poll (required).
 *   AGHUB_URL           Hub base URL (default: http://127.0.0.1:3001).
 *   POLL_MS             Poll interval in milliseconds (default: 30000).
 *   TELEGRAM_BOT_TOKEN  Bot token from @BotFather. Supplied by the owner
 *                       later; nothing is hardcoded here. While unset, the
 *                       worker logs urgent messages instead of forwarding.
 *   TELEGRAM_CHAT_ID    Chat/group id the notifications go to. While unset,
 *                       urgent messages are only logged.
 *
 * Usage:
 *   AGHUB_TOKEN=... node scripts/urgent-forwarder.mjs
 *
 * Notes:
 *   - Node.js >= 18 (uses the built-in fetch). Zero npm dependencies.
 *   - Seen message ids are persisted to scripts/urgent-forwarder.state.json
 *     (next to this script) so restarts do not re-forward old messages.
 *   - Never crashes: poll errors and missing Telegram credentials are
 *     logged and the loop continues.
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const STATE_FILE = join(HERE, 'urgent-forwarder.state.json');

const HUB_URL = (process.env.AGHUB_URL || 'http://127.0.0.1:3001').replace(/\/+$/, '');
const AGHUB_TOKEN = process.env.AGHUB_TOKEN || '';
const POLL_MS = Number(process.env.POLL_MS || 30000);
const TELEGRAM_BOT_TOKEN = process.env.TELEGRAM_BOT_TOKEN || '';
const TELEGRAM_CHAT_ID = process.env.TELEGRAM_CHAT_ID || '';

// Keep the persisted id list bounded: only the most recent ids are retained.
const MAX_SEEN = 5000;

function log(msg) {
  console.log(`[urgent-forwarder ${new Date().toISOString()}] ${msg}`);
}

function loadSeen() {
  try {
    const ids = JSON.parse(readFileSync(STATE_FILE, 'utf8'));
    return new Set(Array.isArray(ids) ? ids : []);
  } catch {
    return new Set(); // missing or corrupt state file: start fresh
  }
}

function saveSeen(seen) {
  try {
    writeFileSync(STATE_FILE, JSON.stringify([...seen].slice(-MAX_SEEN)));
  } catch (e) {
    log(`could not persist state file ${STATE_FILE}: ${e.message}`);
  }
}

function shortText(msg) {
  const body = String(msg.body || '').replace(/\s+/g, ' ').trim();
  return body.length > 200 ? `${body.slice(0, 197)}...` : body;
}

async function forwardToTelegram(msg) {
  const text = `URGENT from @${msg.sender_handle}: ${shortText(msg)}`;
  const url = `https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/sendMessage`;
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ chat_id: TELEGRAM_CHAT_ID, text }),
  });
  if (!res.ok) {
    log(`Telegram sendMessage failed: HTTP ${res.status}`);
  }
}

async function handleUrgent(msg) {
  if (!TELEGRAM_BOT_TOKEN || !TELEGRAM_CHAT_ID) {
    log(
      `urgent message #${msg.id} from @${msg.sender_handle}: ${shortText(msg)} ` +
        '(TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID not set; supply them to enable Telegram forwarding)'
    );
    return;
  }
  try {
    await forwardToTelegram(msg);
  } catch (e) {
    log(`Telegram forward failed: ${e.message}`);
  }
}

async function pollOnce(seen) {
  const res = await fetch(`${HUB_URL}/v1/inbox?unread=true`, {
    headers: { Authorization: `Bearer ${AGHUB_TOKEN}` },
  });
  if (res.status === 401) {
    log('hub rejected the token (401): set a valid AGHUB_TOKEN');
    return;
  }
  if (!res.ok) {
    log(`inbox poll failed: HTTP ${res.status}`);
    return;
  }
  const v = await res.json();
  for (const item of v.items || []) {
    const msg = item.message || {};
    if (!msg.urgent || seen.has(msg.id)) continue;
    seen.add(msg.id); // dedupe before acting, so a failed forward never spams
    saveSeen(seen);
    await handleUrgent(msg);
  }
}

async function main() {
  if (!AGHUB_TOKEN) {
    log('AGHUB_TOKEN is not set: continuing, but every poll will fail until it is provided');
  }
  log(`polling ${HUB_URL}/v1/inbox?unread=true every ${POLL_MS}ms`);
  const seen = loadSeen();
  for (;;) {
    try {
      await pollOnce(seen);
    } catch (e) {
      // Network down, hub restarting, etc.: log and keep polling.
      log(`poll error: ${e.message}`);
    }
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
}

main();
