// Agent detail: transcript, approvals, composer, slash commands.
import { api, el, statusEl, fmtCost, Socket, toast } from '/assets/common.js';

const slug = decodeURIComponent(location.pathname.replace(/^\/agent\//, ''));
const $ = (id) => document.getElementById(id);

const state = {
  agent: null,
  cursor: 0,
  earliest: null,
  commands: [],
  pending: new Map(),
  partial: null,
  queued: [],
  acIndex: 0,
  acItems: [],
};

// -- transcript -------------------------------------------------------------

function textOf(message) {
  const content = message && message.content;
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content
    .filter((b) => b.type === 'text')
    .map((b) => b.text)
    .join('');
}

function toolBlocks(message) {
  const content = (message && message.content) || [];
  return Array.isArray(content) ? content.filter((b) => b.type === 'tool_use') : [];
}

function toolResults(message) {
  const content = (message && message.content) || [];
  return Array.isArray(content) ? content.filter((b) => b.type === 'tool_result') : [];
}

function pretty(value) {
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function renderEvent(event) {
  const p = event.payload || {};
  switch (event.kind) {
    case 'user': {
      const text = textOf(p.message) || (typeof p.message === 'string' ? p.message : '');
      if (!text) return null;
      return el('div', { class: 'ev user' }, [
        el('div', { class: 'who', text: 'you' }),
        el('div', { class: 'body', text }),
      ]);
    }
    case 'assistant': {
      const text = textOf(p.message);
      if (!text.trim() && toolBlocks(p.message).length) return null; // rendered as tool_use
      return el('div', { class: 'ev assistant' }, [
        el('div', { class: 'who', text: 'claude' }),
        el('div', { class: 'body', text }),
      ]);
    }
    case 'tool_use':
      return el('div', { class: 'ev tool' }, [
        el('details', { class: 'tool' }, [
          el('summary', { text: `⚒ ${p.name || 'tool'}` }),
          el('pre', { text: pretty(p.input) }),
        ]),
      ]);
    case 'tool_result': {
      const results = toolResults(p.message);
      const body = results
        .map((r) => (typeof r.content === 'string' ? r.content : pretty(r.content)))
        .join('\n');
      const failed = results.some((r) => r.is_error);
      return el('div', { class: 'ev tool' }, [
        el('details', { class: 'tool' }, [
          el('summary', { text: failed ? '↩ tool result (error)' : '↩ tool result' }),
          el('pre', { text: body.slice(0, 20000) }),
        ]),
      ]);
    }
    case 'result':
      return el('div', { class: 'ev result' }, [
        el('div', {
          class: 'body',
          text: `— turn finished${p.total_cost_usd != null ? ` · ${fmtCost(p.total_cost_usd)} total` : ''}${p.is_error ? ' · error' : ''}`,
        }),
      ]);
    case 'stderr':
      return el('div', { class: 'ev stderr' }, [el('div', { class: 'body', text: p.text || '' })]);
    case 'error':
      return el('div', { class: 'ev error' }, [el('div', { class: 'body', text: pretty(p) })]);
    case 'permission_request':
      return el('div', { class: 'ev system' }, [
        el('div', { class: 'body', text: `⏸ asked to use ${p.tool_name}` }),
      ]);
    case 'permission_decision':
      return el('div', { class: 'ev system' }, [
        el('div', { class: 'body', text: `${p.behavior === 'allow' ? '✓ approved' : '✕ denied'}` }),
      ]);
    case 'system': {
      if (p._unrecognised) {
        return el('div', { class: 'ev system' }, [
          el('details', { class: 'tool' }, [
            el('summary', { text: `? unrecognised protocol event (${p.type || 'unknown'})` }),
            el('pre', { text: pretty(p) }),
          ]),
        ]);
      }
      if (p.subtype === 'init') return null;
      if (p.subtype === 'process_exit') {
        return el('div', { class: 'ev system' }, [
          el('div', { class: 'body', text: `— process exited (code ${p.code ?? '?'}${p.signal ? `, signal ${p.signal}` : ''})` }),
        ]);
      }
      if (p.type === 'control_response') return null;
      return el('div', { class: 'ev system' }, [
        el('div', { class: 'body', text: `${p.subtype || 'system'}` }),
      ]);
    }
    default:
      return null;
  }
}

function atBottom() {
  const node = $('transcript');
  return node.scrollHeight - node.scrollTop - node.clientHeight < 60;
}

function appendEvents(events, prepend = false) {
  const host = $('transcript');
  const stick = atBottom();
  // Replay and the live stream overlap: an event persisted between the replay
  // query and the socket catching up arrives twice. The cursor is the filter.
  if (!prepend) events = events.filter((e) => e.seq > state.cursor);
  if (!events.length) return;
  const nodes = events.map(renderEvent).filter(Boolean);
  if (prepend) host.prepend(...nodes);
  else host.append(...nodes);
  for (const event of events) {
    if (!prepend) state.cursor = Math.max(state.cursor, event.seq);
    if (state.earliest === null || event.seq < state.earliest) state.earliest = event.seq;
  }
  if (!prepend && stick) host.scrollTop = host.scrollHeight;
}

// Live typing from --include-partial-messages. Never persisted, so it is
// dropped as soon as the completed block arrives.
function onPartial(msg) {
  const event = msg.payload && msg.payload.event;
  const delta = event && event.delta;
  const text = delta && (delta.text || delta.partial_json);
  if (!text) return;
  if (!state.partial) {
    state.partial = el('div', { class: 'ev assistant partial' }, [
      el('div', { class: 'who', text: 'claude' }),
      el('div', { class: 'body', text: '' }),
    ]);
    $('transcript').append(state.partial);
  }
  const body = state.partial.querySelector('.body');
  body.textContent += text;
  if (atBottom()) $('transcript').scrollTop = $('transcript').scrollHeight;
}

function clearPartial() {
  if (state.partial) {
    state.partial.remove();
    state.partial = null;
  }
}

// -- approvals --------------------------------------------------------------

function suggestionButtons(request) {
  const buttons = [];
  const suggestions = request.permission_suggestions;
  if (!Array.isArray(suggestions)) return buttons;
  for (const suggestion of suggestions) {
    const mode = suggestion && (suggestion.mode || suggestion.permissionMode);
    if (mode === 'acceptEdits') {
      buttons.push(el('button', {
        text: 'Accept edits for this session',
        onclick: async () => {
          await decide(request.request_id, 'allow');
          await api(`/api/agents/${state.agent.id}/permission_mode`, {
            method: 'POST',
            body: JSON.stringify({ mode: 'acceptEdits' }),
          }).catch((err) => toast(err.message, 'error'));
        },
      }));
    }
  }
  return buttons;
}

function renderApprovals() {
  const host = $('approvals');
  host.replaceChildren();
  for (const request of state.pending.values()) {
    host.append(el('div', { class: 'approval' }, [
      el('h3', { text: `Allow ${request.display_name || request.tool_name}?` }),
      request.description ? el('div', { class: 'small muted', text: request.description }) : null,
      el('pre', { text: pretty(request.input) }),
      el('div', { class: 'actions' }, [
        el('button', { class: 'primary', text: 'Approve', onclick: () => decide(request.request_id, 'allow') }),
        el('button', { class: 'danger', text: 'Deny', onclick: () => decide(request.request_id, 'deny') }),
        ...suggestionButtons(request),
      ]),
    ]));
  }
}

function decide(requestId, behavior) {
  socket.send({
    type: 'permission_decision',
    agent_id: state.agent.id,
    request_id: requestId,
    behavior,
    message: behavior === 'deny' ? 'Denied by the operator' : null,
  });
  state.pending.delete(requestId);
  renderApprovals();
  return Promise.resolve();
}

// -- header -----------------------------------------------------------------

function renderHeader() {
  const agent = state.agent;
  if (!agent) return;
  $('agent-name').textContent = agent.name;
  $('agent-status').replaceChildren(statusEl(agent.status, agent.status_detail));
  const where = agent.is_git
    ? `${agent.branch || 'detached'} · base ${agent.base_ref || '?'} · ${agent.uses_worktree ? 'worktree' : 'main checkout'}`
    : 'no VCS';
  $('agent-meta').textContent = `${where} · ${agent.permission_mode} · ${fmtCost(agent.cost_usd)} · ${agent.work_path}`;
  const running = agent.status !== 'stopped' && agent.status !== 'failed';
  $('btn-interrupt').disabled = !running;
  $('btn-stop').disabled = !running;
  $('btn-resume').disabled = running;
  document.title = `${agent.name} · claude-web`;
}

// -- slash command autocomplete --------------------------------------------

function updateAutocomplete() {
  const box = $('autocomplete');
  const value = $('input').value;
  const match = /(^|\n)\/([\w:.-]*)$/.exec(value);
  if (!match || !state.commands.length) {
    box.classList.add('hidden');
    state.acItems = [];
    return;
  }
  const query = match[2].toLowerCase();
  state.acItems = state.commands
    .filter((c) => c.name.replace(/^\//, '').toLowerCase().startsWith(query))
    .slice(0, 12);
  if (!state.acItems.length) {
    box.classList.add('hidden');
    return;
  }
  state.acIndex = Math.min(state.acIndex, state.acItems.length - 1);
  box.replaceChildren(...state.acItems.map((cmd, i) => el('div', {
    class: i === state.acIndex ? 'active' : '',
    onclick: () => applyCommand(cmd),
  }, [
    el('span', { text: cmd.name }),
    cmd.argument_hint ? el('span', { class: 'hint', text: ` ${cmd.argument_hint}` }) : null,
    cmd.description ? el('span', { class: 'hint', text: ` — ${cmd.description}` }) : null,
  ])));
  box.classList.remove('hidden');
}

function applyCommand(cmd) {
  const input = $('input');
  input.value = input.value.replace(/(^|\n)\/[\w:.-]*$/, `$1${cmd.name} `);
  $('autocomplete').classList.add('hidden');
  input.focus();
}

// -- composer ---------------------------------------------------------------

function renderQueued() {
  const host = $('queued');
  host.replaceChildren();
  if (!state.queued.length) return;
  host.append(document.createTextNode('queued: '));
  for (const item of state.queued) {
    host.append(el('span', { class: 'q' }, [
      el('span', { text: item.text.slice(0, 40) }),
      el('button', {
        text: '✕',
        title: 'Removes it from this list only. Interrupt cancels the in-flight turn; the CLI reports what stays queued.',
        onclick: () => {
          state.queued = state.queued.filter((q) => q !== item);
          renderQueued();
        },
      }),
    ]));
  }
}

function send() {
  const input = $('input');
  const text = input.value.trim();
  if (!text || !state.agent) return;
  socket.send({ type: 'send_message', agent_id: state.agent.id, text });
  // The CLI queues messages received during a turn (F6); show that.
  if (state.agent.status === 'working' || state.agent.status === 'awaiting_approval') {
    state.queued.push({ text });
    renderQueued();
  }
  input.value = '';
  updateAutocomplete();
}

// -- wiring -----------------------------------------------------------------

const socket = new Socket();

async function loadEarlier() {
  if (state.earliest === null || state.earliest <= 1) return;
  const after = Math.max(0, state.earliest - 201);
  const data = await api(`/api/agents/${state.agent.id}/events?after=${after}&limit=200`);
  const older = data.events.filter((e) => e.seq < state.earliest);
  if (!older.length) return;
  appendEvents(older, true);
  $('load-earlier').classList.toggle('hidden', state.earliest <= 1);
}

async function main() {
  const data = await api(`/api/agents/${encodeURIComponent(slug)}`);
  state.agent = data.agent;
  state.commands = data.agent.commands || [];
  for (const request of data.agent.pending_permissions || []) {
    state.pending.set(request.request_id, request);
  }
  renderHeader();
  renderApprovals();

  $('btn-interrupt').onclick = () => socket.send({ type: 'interrupt', agent_id: state.agent.id });
  $('btn-stop').onclick = () => api(`/api/agents/${state.agent.id}/stop`, { method: 'POST', body: '{}' }).catch((e) => toast(e.message, 'error'));
  // The button stays disabled until the request settles: two resumes in flight
  // would otherwise race for one session.
  $('btn-resume').onclick = async () => {
    const button = $('btn-resume');
    if (button.disabled) return;
    button.disabled = true;
    try {
      await api(`/api/agents/${state.agent.id}/resume`, { method: 'POST', body: '{}' });
    } catch (err) {
      toast(err.message, 'error');
      button.disabled = false;
    }
  };
  $('send').onclick = send;
  $('load-earlier').onclick = () => loadEarlier().catch((e) => toast(e.message, 'error'));
  $('input').addEventListener('input', updateAutocomplete);
  $('input').addEventListener('keydown', (event) => {
    const box = $('autocomplete');
    if (!box.classList.contains('hidden') && state.acItems.length) {
      if (event.key === 'ArrowDown') {
        state.acIndex = (state.acIndex + 1) % state.acItems.length;
        updateAutocomplete();
        event.preventDefault();
        return;
      }
      if (event.key === 'ArrowUp') {
        state.acIndex = (state.acIndex - 1 + state.acItems.length) % state.acItems.length;
        updateAutocomplete();
        event.preventDefault();
        return;
      }
      if (event.key === 'Tab' || (event.key === 'Enter' && !event.shiftKey)) {
        applyCommand(state.acItems[state.acIndex]);
        event.preventDefault();
        return;
      }
      if (event.key === 'Escape') {
        box.classList.add('hidden');
        return;
      }
    }
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      send();
    }
  });

  // Subscribe with the cursor we hold, so a reconnect replays only the delta.
  const subscribe = () => socket.send({
    type: 'subscribe',
    agent_id: state.agent.id,
    after_seq: state.cursor || null,
  });
  socket.onopen = subscribe;
  subscribe();

  socket
    .on('replay', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      if (state.earliest === null) state.earliest = msg.after + 1;
      const before = state.cursor;
      appendEvents(msg.events);
      state.pending = new Map((msg.pending_permissions || []).map((r) => [r.request_id, r]));
      renderApprovals();
      $('load-earlier').classList.toggle('hidden', (state.earliest || 1) <= 1);
      // A replay page is capped. If the head is further on, ask for the next
      // page from the cursor we now hold, or the events between this page and
      // the live stream would be lost for good.
      if (msg.has_more && msg.cursor > before) {
        socket.send({ type: 'subscribe', agent_id: state.agent.id, after_seq: msg.cursor });
      }
    })
    .on('event', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      clearPartial();
      appendEvents([{ seq: msg.seq, ts: msg.ts, kind: msg.kind, payload: msg.payload }]);
      if (msg.kind === 'result') {
        state.queued = [];
        renderQueued();
      }
    })
    .on('partial', (msg) => {
      if (msg.agent_id === state.agent.id) onPartial(msg);
    })
    .on('status', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      Object.assign(state.agent, {
        status: msg.status,
        status_detail: msg.status_detail,
        cost_usd: msg.cost_usd,
        last_stderr: msg.last_stderr,
      });
      renderHeader();
    })
    .on('permission_request', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      state.pending.set(msg.request.request_id, msg.request);
      renderApprovals();
    })
    .on('permission_resolved', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      state.pending.delete(msg.request_id);
      renderApprovals();
    })
    .on('commands', (msg) => {
      if (msg.agent_id === state.agent.id) state.commands = msg.commands;
    })
    .on('queued', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      const still = Array.isArray(msg.still_queued) ? msg.still_queued.length : 0;
      toast(`Turn interrupted. ${still} message(s) still queued.`, 'warn');
    })
    .on('notice', (msg) => toast(msg.text, msg.level))
    .on('agent_removed', (msg) => {
      if (msg.agent_id === state.agent.id) location.href = '/';
    });
}

main().catch((err) => toast(err.message, 'error'));
