#!/usr/bin/env node
/**
 * unanswered-dm-watcher — warns about DMs nobody answered in time.
 *
 * A DM can sit unread in an agent's inbox while the owner is away (no
 * webhook, nobody watching the stream). This worker polls the hub on an
 * interval and warns about unread DMs (messages with no channel) that are
 * still unread after WARN_AFTER_MINUTES minutes, so the owner gets pinged
 * instead of discovering the message days later.
 *
 * Environment variables:
 *   AGHUB_TOKEN          Bearer token of the agent whose inbox to poll (required).
 *   AGHUB_URL            Hub base URL (default: http://127.0.0.1:3001).
 *   POLL_MS              Poll interval in milliseconds (default: 30000).
 *   WARN_AFTER_MINUTES   Minutes a DM may stay unread before warning
 *                        (default: 30).
 *   TELEGRAM_BOT_TOKEN   Bot token from @BotFather. Supplied by the owner
 *                        later; nothing is hardcoded here. While unset, the
 *                        worker logs warnings instead of forwarding.
 *   TELEGRAM_CHAT_ID     Chat/group id the warnings go to. While unset, DMs
 *                        are only logged.
 *   DRY_RUN              Set to 1 to list what WOULD be warned about and exit
 *                        without sending anything and without writing the
 *                        state file. Same as the --dry-run flag.
 *
 * Usage:
 *   AGHUB_TOKEN=... node scripts/unanswered-dm-watcher.mjs
 *   AGHUB_TOKEN=... node scripts/unanswered-dm-watcher.mjs --dry-run
 *
 * Notes:
 *   - Node.js >= 18 (uses the built-in fetch). Zero npm dependencies.
 *   - Warned message ids are persisted to
 *     scripts/unanswered-dm-watcher.state.json (next to this script) so
 *     restarts do not re-warn about the same DM.
 *   - Never crashes: poll errors and missing Telegram credentials are
 *     logged and the loop continues.
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const STATE_FILE = join(HERE, 'unanswered-dm-watcher.state.json');

const HUB_URL = (process.env.AGHUB_URL || 'http://127.0.0.1:3001').replace(/\/+$/, '');
const AGHUB_TOKEN = process.env.AGHUB_TOKEN || '';
const POLL_MS = Number(process.env.POLL_MS || 30000);
const WARN_AFTER_MINUTES = Number(process.env.WARN_AFTER_MINUTES || 30);
const TELEGRAM_BOT_TOKEN = process.env.TELEGRAM_BOT_TOKEN || '';
const TELEGRAM_CHAT_ID = process.env.TELEGRAM_CHAT_ID || '';
const DRY_RUN = process.env.DRY_RUN === '1' || process.argv.includes('--dry-run');

// Keep the persisted id list bounded: only the most recent ids are retained.
const MAX_SEEN = 5000;

function log(msg) {
  console.log(`[unanswered-dm-watcher ${new Date().toISOString()}] ${msg}`);
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

// Age of a message in minutes, judged by its created_at timestamp.
function ageMinutes(msg) {
  const created = Date.parse(msg.created_at || '');
  if (Number.isNaN(created)) return null; // unparsable timestamp: never warn
  return (Date.now() - created) / 60000;
}

async function sendToTelegram(text) {
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

async function handleWarning(msg) {
  const text = `UNANSWERED DM from @${msg.sender_handle}: ${shortText(msg)}`;
  if (DRY_RUN) {
    log(`dry-run: would warn about DM #${msg.id} (unread for too long): ${text}`);
    return;
  }
  if (!TELEGRAM_BOT_TOKEN || !TELEGRAM_CHAT_ID) {
    log(
      `unanswered DM #${msg.id} from @${msg.sender_handle}: ${shortText(msg)} ` +
        '(TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID not set; supply them to enable Telegram warnings)'
    );
    return;
  }
  try {
    await sendToTelegram(text);
  } catch (e) {
    log(`Telegram warning failed: ${e.message}`);
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
    // DMs only: channel_id null means the message was sent directly to this agent.
    if (msg.channel_id != null || seen.has(msg.id)) continue;
    const age = ageMinutes(msg);
    if (age == null || age < WARN_AFTER_MINUTES) continue;
    if (!DRY_RUN) {
      seen.add(msg.id); // dedupe before acting, so a failed send never spams
      saveSeen(seen);
    }
    await handleWarning(msg);
  }
}

async function main() {
  if (!AGHUB_TOKEN) {
    if (DRY_RUN) {
      log('AGHUB_TOKEN is not set: cannot poll the inbox in dry-run');
      return;
    }
    log('AGHUB_TOKEN is not set: continuing, but every poll will fail until it is provided');
  }
  if (DRY_RUN) {
    log(`dry run against ${HUB_URL}/v1/inbox?unread=true (warn after ${WARN_AFTER_MINUTES} minutes)`);
    try {
      await pollOnce(loadSeen());
    } catch (e) {
      // Network down, hub restarting, etc.: log; the state file is untouched.
      log(`poll error: ${e.message}`);
    }
    return;
  }
  log(`polling ${HUB_URL}/v1/inbox?unread=true every ${POLL_MS}ms (warn after ${WARN_AFTER_MINUTES} minutes)`);
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
