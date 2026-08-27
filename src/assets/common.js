// Shared helpers: API calls, the multiplexed socket, small DOM utilities.
// No build step, no dependencies — plain ES modules (§7).

// The per-boot session token. It arrives in the URL the server opens, is kept
// out of the address bar afterwards, and is required on every API call and on
// the socket upgrade. Loopback is not an authentication boundary — everything
// on this machine can reach the server, agents included — so this raises the
// bar in front of the control plane.
//
// sessionStorage, not localStorage: it is scoped to this tab and does not
// outlive the browser session, which shortens the window in which the token
// sits in the browser profile on disk. It cannot be made unreadable to a
// process running as the same user; see DESIGN §7.
const TOKEN_KEY = 'claude-web-token';

function readToken() {
  try {
    const url = new URL(location.href);
    const fromUrl = url.searchParams.get('t');
    if (fromUrl) {
      sessionStorage.setItem(TOKEN_KEY, fromUrl);
      url.searchParams.delete('t');
      history.replaceState(null, '', url.pathname + url.search + url.hash);
      return fromUrl;
    }
    return sessionStorage.getItem(TOKEN_KEY) || '';
  } catch {
    return '';
  }
}

export const token = readToken();

export async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: {
      'content-type': 'application/json',
      'x-claude-web-token': token,
      ...(options.headers || {}),
    },
  });
  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { error: text };
    }
  }
  if (!response.ok) {
    if (response.status === 401) needToken();
    const err = new Error((body && body.error) || `${response.status} ${response.statusText}`);
    err.status = response.status;
    err.body = body;
    throw err;
  }
  return body;
}

// Mirrors repo::git::slugify so the branch preview matches what the server
// will actually create.
export function slugify(name) {
  let out = '';
  let underscore = false;
  for (const ch of String(name)) {
    if (/[a-zA-Z0-9]/.test(ch)) {
      out += ch.toLowerCase();
      underscore = false;
    } else if (!underscore) {
      out += '_';
      underscore = true;
    }
    if (out.length >= 40) break;
  }
  out = out.replace(/^_+|_+$/g, '');
  return out || 'agent';
}

export function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key === 'class') node.className = value;
    // Deliberately no `html` escape hatch: every node in this app is built
    // through here, and much of the content is attacker-influenced.
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? '' : value);
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

export function statusLabel(status, detail) {
  const base = {
    starting: 'Starting',
    idle: 'Idle',
    working: 'Working',
    awaiting_approval: 'Awaiting approval',
    stopped: 'Stopped',
    failed: 'Failed',
  }[status] || status;
  return detail ? `${base} — ${detail}` : base;
}

export function statusEl(status, detail) {
  return el('span', { class: `status s-${status}` }, [
    el('span', { class: 'dot' }),
    el('span', { text: statusLabel(status, detail) }),
  ]);
}

export function fmtCost(usd) {
  const n = Number(usd || 0);
  return n < 0.01 ? `$${n.toFixed(4)}` : `$${n.toFixed(2)}`;
}

export function fmtTime(ms) {
  if (!ms) return '';
  return new Date(ms).toLocaleTimeString();
}

export function fmtAgo(ms) {
  if (!ms) return '';
  const secs = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
  return `${Math.round(secs / 86400)}d ago`;
}

// One socket, one reconnect path, one schema. Handlers are keyed by the
// envelope's `type`.
export class Socket {
  constructor() {
    this.handlers = new Map();
    this.queue = [];
    this.ws = null;
    this.retry = 0;
    this.onopen = null;
    /** Set once reconnecting has been given up on, and never unset. */
    this.stopped = false;
    this.connect();
  }

  connect() {
    if (this.stopped) return;
    if (!token) {
      // Nothing to present, so the upgrade would be refused every time.
      this.stopped = true;
      needToken();
      return;
    }
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    // A browser cannot set headers on an upgrade, so the token rides in the
    // query string for this one request.
    this.ws = new WebSocket(`${proto}://${location.host}/ws?token=${encodeURIComponent(token)}`);
    this.ws.addEventListener('open', () => {
      this.retry = 0;
      for (const msg of this.queue.splice(0)) this.ws.send(msg);
      if (this.onopen) this.onopen();
    });
    this.ws.addEventListener('message', (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      const handler = this.handlers.get(msg.type);
      if (handler) handler(msg);
      const any = this.handlers.get('*');
      if (any) any(msg);
    });
    this.ws.addEventListener('close', () => {
      this.reconnect();
    });
  }

  /// Decide whether reconnecting can ever work, and back off if it can.
  ///
  /// A browser deliberately hides the HTTP status of a rejected upgrade: a
  /// refused token and a server that is merely down both arrive here as a bare
  /// close with code 1006. Untangled, that made a page whose token had gone
  /// stale — the usual cause being a tab left open across a restart, since the
  /// token is minted per boot — retry every 16s for as long as it stayed open,
  /// showing the operator nothing and filling the server log with refusals.
  ///
  /// So ask over HTTP, which does report the status. A 401 is a verdict, not a
  /// hiccup: no number of retries will make a dead token live, and only opening
  /// the new link will. Anything else — including an unreachable server, which
  /// is exactly the transient case — backs off and tries again.
  async reconnect() {
    if (this.stopped) return;
    if (await tokenRefused()) {
      this.stopped = true;
      needToken();
      return;
    }
    // Reconnect with a cursor, so only the delta is replayed.
    this.retry = Math.min(this.retry + 1, 6);
    setTimeout(() => this.connect(), 250 * 2 ** this.retry);
  }

  on(type, handler) {
    this.handlers.set(type, handler);
    return this;
  }

  send(msg) {
    const text = JSON.stringify(msg);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(text);
    else this.queue.push(text);
  }
}

/// Does the server refuse our token outright? `false` for anything else,
/// including not being able to ask — a server that is down is not a verdict on
/// the token.
async function tokenRefused() {
  try {
    const response = await fetch('/api/health', {
      headers: { 'x-claude-web-token': token },
    });
    return response.status === 401;
  } catch {
    return false;
  }
}

/// Tell the operator their link is stale, once, and stop pretending to work.
let toldAboutToken = false;
export function needToken() {
  if (toldAboutToken) return;
  toldAboutToken = true;
  const banner = el('div', { class: 'errbox' }, [
    el('strong', { text: 'This page has no valid session token. ' }),
    'Open the link claude-web printed when it started — the token changes every '
      + 'time the server restarts.',
  ]);
  document.querySelector('main')?.prepend(banner);
}

export function toast(text, level = 'info') {
  let host = document.querySelector('.toasts');
  if (!host) {
    host = el('div', { class: 'toasts' });
    document.body.append(host);
  }
  const node = el('div', { class: `toast ${level}`, text });
  host.append(node);
  setTimeout(() => node.remove(), level === 'error' ? 12000 : 6000);
}
