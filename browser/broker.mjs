// gridterm browser broker
//
// One Node process owns TWO Chrome instances:
//
//   1) IDENTITY (headed): a normal, visible Chrome with a persistent profile.
//      This is where YOU log in (Gmail, your app, AWS). It is the only browser
//      that ever shows a window. The gridterm main agent surfaces it for login.
//
//   2) WORKER (headless): a headless Chrome the PANEL agents (Claude Code, Codex)
//      drive. It never shows a window. Each panel agent gets its OWN tab here
//      (isolated like two browser tabs), and they all SHARE the worker context.
//
// Cookies flow IDENTITY -> WORKER continuously (and on demand), so whatever you
// log into in the visible browser is available to the headless panel agents
// within a few seconds, without any window ever popping up for them.
//
// Why two browsers: Chrome is single-instance per profile and cannot mix headed
// and headless in one process, so "panels headless using the main session's
// cookies" requires a separate headless browser kept in sync. Cookie sync is
// cheap (CDP Storage.getCookies/setCookies, a few KB, no rendering).
//
// Socket protocol (first line from the client selects the channel):
//   "MCP\n" or "MCP PANEL\n"  -> MCP stdio stream for a PANEL agent (-> WORKER)
//   "MCP MAIN\n"              -> MCP stdio stream for the MAIN agent  (-> IDENTITY)
//   "CTL <json>\n"            -> one-shot control command from gridterm (Rust)

import net from 'node:net';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { chromium } from 'playwright-core';
import { createConnection } from '@playwright/mcp';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';

const HOME = os.homedir();
const GT_DIR = path.join(HOME, '.gridterm');
const SOCK_PATH = path.join(GT_DIR, 'browser-broker.sock');
const LOG_PATH = path.join(GT_DIR, 'browser-broker.log');

// Identity (headed) browser: visible, holds your logins.
const IDENTITY_PROFILE = path.join(GT_DIR, 'chrome-profile');
const IDENTITY_PORT = Number(process.env.GRIDTERM_CDP_PORT || 9333);
// Worker (headless) browser: panels drive this, never visible.
const WORKER_PROFILE = path.join(GT_DIR, 'chrome-worker');
const WORKER_PORT = Number(process.env.GRIDTERM_WORKER_PORT || 9334);

const CHROME_PATH =
  process.env.GRIDTERM_CHROME ||
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

const IDLE_TAB_MS = Number(process.env.GRIDTERM_IDLE_TAB_MS || 5 * 60 * 1000);
const IDLE_SWEEP_MS = 30 * 1000;
const COOKIE_SYNC_MS = Number(process.env.GRIDTERM_COOKIE_SYNC_MS || 4000);
const FOCUS_APP = process.env.GRIDTERM_FOCUS_APP || 'Terminal';

fs.mkdirSync(GT_DIR, { recursive: true });

function log(...args) {
  const line = `[${new Date().toISOString()}] ${args.join(' ')}\n`;
  try {
    fs.appendFileSync(LOG_PATH, line);
  } catch {}
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// ----- single-instance guard ------------------------------------------------

function alreadyRunning() {
  return new Promise((resolve) => {
    if (!fs.existsSync(SOCK_PATH)) return resolve(false);
    const probe = net.connect(SOCK_PATH);
    probe.setTimeout(500);
    probe.on('connect', () => {
      probe.destroy();
      resolve(true);
    });
    probe.on('timeout', () => {
      probe.destroy();
      resolve(false);
    });
    probe.on('error', () => resolve(false));
  });
}

function portUp(port) {
  return new Promise((resolve) => {
    const s = net.connect(port, '127.0.0.1');
    s.setTimeout(400);
    s.on('connect', () => {
      s.destroy();
      resolve(true);
    });
    s.on('timeout', () => {
      s.destroy();
      resolve(false);
    });
    s.on('error', () => resolve(false));
  });
}

// ----- the two Chromes -------------------------------------------------------

// Each browser tier: { browser, context, proc, connecting }
const identity = { browser: null, context: null, proc: null, connecting: null };
const worker = { browser: null, context: null, proc: null, connecting: null };

function launchChrome(tier, { headless }) {
  const port = tier === 'identity' ? IDENTITY_PORT : WORKER_PORT;
  const profile = tier === 'identity' ? IDENTITY_PROFILE : WORKER_PROFILE;
  fs.mkdirSync(profile, { recursive: true });
  const args = [
    `--remote-debugging-port=${port}`,
    '--remote-debugging-address=127.0.0.1',
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-features=OptimizationGuideOnDeviceModel,CalculateNativeWinOcclusion',
    '--disable-backgrounding-occluded-windows',
    '--disable-background-timer-throttling',
    '--disable-renderer-backgrounding',
  ];
  if (headless) {
    // New headless mode: real rendering pipeline, never shows a window.
    args.push('--headless=new');
  } else {
    // Visible window, but don't auto-open an extra startup window.
    args.push('--no-startup-window');
  }
  args.push('about:blank');
  log(`launching ${tier} Chrome (headless=${!!headless}) on ${port}`);
  const proc = spawn(CHROME_PATH, args, { detached: true, stdio: 'ignore' });
  proc.unref();
  return proc;
}

async function ensureTier(tier, opts) {
  const t = tier === 'identity' ? identity : worker;
  if (t.context && t.browser && t.browser.isConnected()) return t.context;
  if (t.connecting) return t.connecting;
  const port = tier === 'identity' ? IDENTITY_PORT : WORKER_PORT;
  const profile = tier === 'identity' ? IDENTITY_PROFILE : WORKER_PROFILE;
  t.connecting = (async () => {
    // Try to connect; if the port is up but it's a foreign/incompatible Chrome
    // (e.g. a leftover from an older version), or connect fails, relaunch OUR
    // own Chrome for this profile and try again.
    const tryConnect = async () => {
      const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 20000 });
      const ctxs = browser.contexts();
      const context = ctxs.length ? ctxs[0] : await browser.newContext();
      browser.on('disconnected', () => {
        log(`${tier} browser disconnected`);
        t.browser = null;
        t.context = null;
      });
      t.browser = browser;
      t.context = context;
      return context;
    };

    const launchOurs = async () => {
      if (!fs.existsSync(CHROME_PATH)) throw new Error(`Chrome not found at ${CHROME_PATH}`);
      // Kill any Chrome we previously launched for THIS profile (never touches
      // the user's normal Chrome, which uses a different profile dir).
      await killByProfile(profile);
      await sleep(300);
      t.proc = launchChrome(tier, opts);
      const deadline = Date.now() + 15000;
      while (Date.now() < deadline && !(await portUp(port))) await sleep(150);
      if (!(await portUp(port))) throw new Error(`${tier} Chrome did not expose CDP`);
      if (tier === 'identity') sendChromeToBack();
    };

    if (await portUp(port)) {
      try {
        return await tryConnect();
      } catch (e) {
        log(`${tier} connect to existing failed (${e.message?.split('\n')[0]}); relaunching ours`);
      }
    }
    await launchOurs();
    return await tryConnect();
  })();
  try {
    return await t.connecting;
  } finally {
    t.connecting = null;
  }
}

// Kill any Chrome process started with the given --user-data-dir (our profile),
// so we never disturb the user's normal Chrome (different profile).
function killByProfile(profile) {
  return new Promise((resolve) => {
    try {
      const p = spawn('pkill', ['-f', `--user-data-dir=${profile}`], { stdio: 'ignore' });
      p.on('exit', () => resolve());
      p.on('error', () => resolve());
    } catch {
      resolve();
    }
  });
}

const ensureIdentity = () => ensureTier('identity', { headless: false });
const ensureWorker = () => ensureTier('worker', { headless: true });

// ----- cookie sync: identity -> worker --------------------------------------
//
// We copy cookies from the headed identity browser into the headless worker so
// panel agents inherit your logins. Uses CDP Storage.getCookies on identity and
// Storage.setCookies on worker (browser-wide, all domains). Cheap; runs on a
// timer and on demand. We only sync if both browsers are up.

let lastCookieSig = '';

async function browserCdp(t) {
  // A browser-level CDP session (not tied to a page) for Storage.* domain.
  return t.browser.newBrowserCDPSession();
}

async function syncCookies(force) {
  if (!identity.browser || !identity.browser.isConnected()) return;
  if (!worker.browser || !worker.browser.isConnected()) return;
  try {
    const isess = await browserCdp(identity);
    const { cookies } = await isess.send('Storage.getCookies');
    await isess.detach().catch(() => {});
    // Cheap change-detection: skip the write if nothing changed.
    const sig = `${cookies.length}:${cookies.reduce((a, c) => a + (c.value ? c.value.length : 0), 0)}`;
    if (!force && sig === lastCookieSig) return;
    lastCookieSig = sig;
    const wsess = await browserCdp(worker);
    // Replace the worker's cookies with the identity set.
    await wsess.send('Storage.clearCookies').catch(() => {});
    if (cookies.length) {
      await wsess.send('Storage.setCookies', { cookies });
    }
    await wsess.detach().catch(() => {});
    log(`cookie sync: ${cookies.length} cookies -> worker`);
  } catch (e) {
    log(`cookie sync failed: ${e?.message || e}`);
  }
}

function startCookieSync() {
  setInterval(() => {
    // Only sync once both tiers exist; never force-launch from the timer.
    if (identity.browser && worker.browser) syncCookies(false);
  }, COOKIE_SYNC_MS).unref();
}

// ----- per-client page ownership (isolated tabs in the WORKER) ---------------

let clientSeq = 0;
const pageOwner = new Map(); // page -> ownerId
const liveOwners = new Set();
const orphanedAt = new Map();

function clientContext(baseContext, ownerId) {
  const base = baseContext;
  const myPages = () =>
    base.pages().filter((p) => pageOwner.get(p) === ownerId && !p.isClosed());
  const handler = {
    get(target, prop, receiver) {
      if (prop === 'pages') return () => myPages();
      if (prop === 'newPage') {
        return async (...args) => {
          const page = await target.newPage(...args);
          try {
            await page.addInitScript((id) => {
              window.__gridtermOwner = id;
            }, ownerId);
          } catch {}
          pageOwner.set(page, ownerId);
          page.on('close', () => {
            pageOwner.delete(page);
            orphanedAt.delete(page);
          });
          log(`client ${ownerId} opened page`);
          return page;
        };
      }
      const v = Reflect.get(target, prop, receiver);
      return typeof v === 'function' ? v.bind(target) : v;
    },
  };
  return new Proxy(base, handler);
}

// ----- MCP connection handling ----------------------------------------------

async function handleMcp(socket, leftover, role) {
  const isMain = role === 'MAIN';
  const ownerId = `${isMain ? 'main' : 'panel'}-${++clientSeq}`;
  liveOwners.add(ownerId);
  log(`MCP client connect ${ownerId} (role=${role})`);
  let connection;
  try {
    // MAIN agent drives the visible identity browser; PANEL agents drive the
    // headless worker. Either way each connection gets its own isolated tab.
    const baseContext = isMain ? await ensureIdentity() : await ensureWorker();
    if (!isMain) {
      // Make sure the worker has the latest logins before the panel acts.
      await ensureIdentity().catch(() => {});
      await syncCookies(true);
    }
    connection = await createConnection({ capabilities: undefined }, async () =>
      clientContext(baseContext, ownerId)
    );
    const { PassThrough } = await import('node:stream');
    const inbound = new PassThrough();
    if (leftover && leftover.length) inbound.write(leftover);
    socket.pipe(inbound);
    socket.on('error', () => inbound.end());
    const transport = new StdioServerTransport(inbound, socket);
    await connection.connect(transport);
  } catch (e) {
    log(`MCP client ${ownerId} error: ${e?.stack || e}`);
    liveOwners.delete(ownerId);
    try {
      socket.destroy();
    } catch {}
    return;
  }
  socket.on('close', async () => {
    log(`MCP client disconnect ${ownerId}`);
    liveOwners.delete(ownerId);
    const now = Date.now();
    for (const [page, owner] of pageOwner) {
      if (owner === ownerId) orphanedAt.set(page, now);
    }
    try {
      await connection.close();
    } catch {}
  });
}

// ----- control channel (gridterm Rust) --------------------------------------

async function handleControl(socket, jsonLine) {
  let req;
  try {
    req = JSON.parse(jsonLine);
  } catch {
    return reply(socket, { ok: false, error: 'bad json' });
  }
  const cmd = req.cmd;
  try {
    switch (cmd) {
      case 'ping':
        return reply(socket, { ok: true, pong: true });
      case 'status': {
        return reply(socket, {
          ok: true,
          identity: await portUp(IDENTITY_PORT),
          worker: await portUp(WORKER_PORT),
          identityConnected: !!(identity.browser && identity.browser.isConnected()),
          workerConnected: !!(worker.browser && worker.browser.isConnected()),
          pages: worker.context ? worker.context.pages().length : 0,
          clients: clientSeq,
        });
      }
      case 'start': {
        // Warm both tiers.
        await ensureIdentity();
        await ensureWorker();
        return reply(socket, { ok: true });
      }
      // Surface the VISIBLE identity browser for the user to log in once.
      case 'surface_login': {
        await ensureIdentity();
        const url = req.url;
        let page = null;
        for (const [p, owner] of pageOwner) {
          if (owner === 'gridterm-login' && !p.isClosed()) {
            page = p;
            break;
          }
        }
        if (!page) {
          page = await identity.context.newPage();
          pageOwner.set(page, 'gridterm-login');
          liveOwners.add('gridterm-login');
          page.on('close', () => pageOwner.delete(page));
        }
        if (url) {
          try {
            await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 });
          } catch (e) {
            log(`surface_login goto failed: ${e}`);
          }
        }
        await raiseWindow(page);
        return reply(socket, { ok: true, url: url || null });
      }
      case 'hide': {
        sendChromeToBack();
        // After a login, immediately push the new cookies to the worker.
        await syncCookies(true);
        return reply(socket, { ok: true });
      }
      case 'sync_cookies': {
        await ensureIdentity();
        await ensureWorker();
        await syncCookies(true);
        return reply(socket, { ok: true });
      }
      // Navigate the VISIBLE identity browser to a URL and return the page's
      // text + title. This is what the gridterm main (cmd+J) agent uses to
      // actually browse and report what it sees (it has no MCP of its own).
      case 'nav': {
        await ensureIdentity();
        const url = req.url;
        let page = null;
        for (const [p, owner] of pageOwner) {
          if (owner === 'gridterm-main' && !p.isClosed()) {
            page = p;
            break;
          }
        }
        if (!page) {
          page = await identity.context.newPage();
          pageOwner.set(page, 'gridterm-main');
          liveOwners.add('gridterm-main');
          page.on('close', () => pageOwner.delete(page));
        }
        let status = null;
        try {
          const r = await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 30000 });
          status = r && r.status();
        } catch (e) {
          return reply(socket, { ok: false, error: String(e.message || e) });
        }
        const title = await page.title().catch(() => '');
        const finalUrl = page.url();
        let text = '';
        try {
          text = await page.evaluate(() => document.body ? document.body.innerText : '');
        } catch {}
        if (text.length > 8000) text = text.slice(0, 8000) + '\n... [truncated]';
        return reply(socket, { ok: true, status, title, url: finalUrl, text });
      }
      // Read the current main page again (no navigation).
      case 'read': {
        await ensureIdentity();
        let page = null;
        for (const [p, owner] of pageOwner) {
          if (owner === 'gridterm-main' && !p.isClosed()) {
            page = p;
            break;
          }
        }
        if (!page) return reply(socket, { ok: false, error: 'no page open; navigate first' });
        const title = await page.title().catch(() => '');
        let text = '';
        try {
          text = await page.evaluate(() => document.body ? document.body.innerText : '');
        } catch {}
        if (text.length > 8000) text = text.slice(0, 8000) + '\n... [truncated]';
        return reply(socket, { ok: true, title, url: page.url(), text });
      }
      case 'cookie_count': {
        await ensureIdentity();
        const cookies = await identity.context.cookies();
        return reply(socket, { ok: true, count: cookies.length });
      }
      case 'shutdown': {
        reply(socket, { ok: true });
        log('shutdown requested');
        setTimeout(() => process.exit(0), 50);
        return;
      }
      default:
        return reply(socket, { ok: false, error: `unknown cmd ${cmd}` });
    }
  } catch (e) {
    log(`control ${cmd} error: ${e?.stack || e}`);
    return reply(socket, { ok: false, error: String(e?.message || e) });
  }
}

function reply(socket, obj) {
  try {
    socket.write(JSON.stringify(obj) + '\n');
    socket.end();
  } catch {}
}

// ----- identity window focus management --------------------------------------

async function raiseWindow(page) {
  try {
    const session = await identity.context.newCDPSession(page);
    const { windowId } = await session.send('Browser.getWindowForTarget');
    await session.send('Browser.setWindowBounds', {
      windowId,
      bounds: { windowState: 'normal', left: 80, top: 80, width: 1100, height: 800 },
    });
    await session.detach().catch(() => {});
  } catch (e) {
    log(`raiseWindow cdp failed: ${e}`);
  }
  try {
    spawn('osascript', ['-e', 'tell application "Google Chrome" to activate'], {
      stdio: 'ignore',
    }).unref();
  } catch {}
}

function sendChromeToBack() {
  try {
    spawn('osascript', ['-e', `tell application "${FOCUS_APP}" to activate`], {
      stdio: 'ignore',
    }).unref();
  } catch {}
}

// ----- idle tab GC (worker tabs only) ----------------------------------------

function startIdleSweeper() {
  setInterval(async () => {
    const ctx = worker.context;
    if (!ctx) return;
    const now = Date.now();
    for (const page of ctx.pages()) {
      if (ctx.pages().length <= 1) break;
      const owner = pageOwner.get(page);
      if (owner && liveOwners.has(owner)) continue; // live client: never reap
      const since = orphanedAt.get(page);
      if (since === undefined) {
        orphanedAt.set(page, now);
        continue;
      }
      if (now - since > IDLE_TAB_MS) {
        log('GC orphaned tab');
        try {
          await page.close();
        } catch {}
      }
    }
  }, IDLE_SWEEP_MS).unref();
}

// ----- socket server ---------------------------------------------------------

function onConnection(socket) {
  socket.setNoDelay(true);
  socket.pause();
  let routed = false;
  let buf = Buffer.alloc(0);

  const onData = (chunk) => {
    if (routed) return;
    buf = Buffer.concat([buf, chunk]);
    const nl = buf.indexOf(0x0a);
    if (nl === -1) return;
    routed = true;
    const header = buf.subarray(0, nl).toString('utf8').trim();
    const rest = buf.subarray(nl + 1);
    socket.removeListener('data', onData);
    socket.pause();

    if (header.startsWith('CTL ')) {
      handleControl(socket, header.slice(4));
      return;
    }
    // MCP channel. Header may be "MCP", "MCP MAIN", "MCP PANEL", or empty.
    let role = 'PANEL';
    let leftover = rest;
    if (header === 'MCP MAIN') role = 'MAIN';
    else if (header === 'MCP PANEL' || header === 'MCP' || header === '') role = 'PANEL';
    else leftover = Buffer.concat([Buffer.from(header + '\n'), rest]); // unknown: treat as panel MCP body
    handleMcp(socket, leftover, role);
  };
  socket.on('data', onData);
  socket.resume();
  socket.on('error', (e) => log(`socket error: ${e}`));
}

async function main() {
  if (await alreadyRunning()) {
    log('another broker is already running; exiting');
    process.exit(0);
  }
  // The socket file may be stale (previous broker died). Only remove it if no
  // live broker answered above.
  try {
    if (fs.existsSync(SOCK_PATH)) fs.unlinkSync(SOCK_PATH);
  } catch {}

  const server = net.createServer(onConnection);
  server.on('error', (e) => {
    // Lost a startup race with another broker: exit quietly, the other wins.
    if (e && e.code === 'EADDRINUSE') {
      log('socket in use (another broker won the race); exiting');
      process.exit(0);
    }
    log(`server error: ${e}`);
    process.exit(1);
  });
  server.listen(SOCK_PATH, () => log(`broker listening on ${SOCK_PATH}`));

  startIdleSweeper();
  startCookieSync();

  const bye = () => {
    try {
      fs.existsSync(SOCK_PATH) && fs.unlinkSync(SOCK_PATH);
    } catch {}
    process.exit(0);
  };
  process.on('SIGINT', bye);
  process.on('SIGTERM', bye);
}

main().catch((e) => {
  log(`fatal: ${e?.stack || e}`);
  process.exit(1);
});
