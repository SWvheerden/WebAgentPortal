// Agent detail: transcript, approvals, composer, slash commands.
import { api, el, statusEl, fmtCost, setAttention, setTitle, Socket, toast } from '/assets/common.js';
import { Transcript, nextWalkCursor } from '/assets/transcript.js';

const slug = decodeURIComponent(location.pathname.replace(/^\/agent\//, ''));
const $ = (id) => document.getElementById(id);

const state = {
  agent: null,
  transcript: new Transcript(),
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

// Tool input and results are written by the model. Two rules: never hide any
// of it behind a horizontal scroll (see app.css), and never render more than
// this without saying so.
const MAX_SHOWN = 4000;

function pretty(value, limit = MAX_SHOWN) {
  let text;
  if (typeof value === 'string') {
    text = value;
  } else {
    try {
      text = JSON.stringify(value, null, 2);
    } catch {
      text = String(value);
    }
    // JSON.stringify escapes newlines inside strings, which turns a multi-line
    // shell command into one very long line. Show the real lines.
    text = text.replace(/\\n/g, '\n').replace(/\\t/g, '  ');
  }
  if (text.length <= limit) return text;
  return `${text.slice(0, limit)}\n… ${text.length - limit} more characters not shown`;
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
    case 'result': {
      const cost = p.total_cost_usd != null ? ` · ${fmtCost(p.total_cost_usd)} total` : '';
      // A turn cut short by a rate limit used to read "— turn finished · error"
      // in the same green as a turn that did the job. It is a failure: say so,
      // in the failure colour, with the reason the CLI gave.
      if (p.is_error) {
        const status = p.api_error_status ? ` (HTTP ${p.api_error_status})` : '';
        const why = p.result ? `: ${p.result}` : '';
        return el('div', { class: 'ev error' }, [
          el('div', { class: 'body', text: `✕ turn ended in an error${status}${why}${cost}` }),
        ]);
      }
      return el('div', { class: 'ev result' }, [
        el('div', { class: 'body', text: `— turn finished${cost}` }),
      ]);
    }
    case 'stderr':
      return el('div', { class: 'ev stderr' }, [el('div', { class: 'body', text: p.text || '' })]);
    case 'error': {
      // An API failure the CLI dressed as an `assistant` line: show the text it
      // carries, attributed to the API rather than to Claude. Anything else
      // keeps the raw dump, which is all there is to show.
      const text = textOf(p.message);
      if (!text.trim()) {
        return el('div', { class: 'ev error' }, [el('div', { class: 'body', text: pretty(p) })]);
      }
      return el('div', { class: 'ev error' }, [
        el('div', { class: 'who', text: p.error ? `api error · ${p.error}` : 'api error' }),
        el('div', { class: 'body', text }),
      ]);
    }
    case 'permission_request':
      return el('div', { class: 'ev system' }, [
        el('div', { class: 'body', text: `⏸ asked to use ${p.tool_name}` }),
      ]);
    case 'permission_decision': {
      const label = { allow: '✓ approved', deny: '✕ denied', expired: '⌁ expired' }[p.behavior]
        || p.behavior;
      const line = el('div', { class: 'ev system' }, [
        el('div', {
          class: 'body',
          text: `${label}${p.tool_name ? ` ${p.tool_name}` : ''}${p.input_modified ? ' (input edited before sending)' : ''}`,
        }),
      ]);
      // What was actually sent, not merely that something was approved.
      if (p.input !== undefined && p.input !== null) {
        line.append(el('details', { class: 'tool' }, [
          el('summary', { text: p.input_modified ? 'input as sent (edited)' : 'input as sent' }),
          el('pre', { text: pretty(p.input) }),
        ]));
      }
      return line;
    }
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
      if (p.subtype === 'permission_mode_change') {
        return el('div', { class: 'ev system' }, [
          el('div', {
            class: 'body',
            text: `⚙ permission mode: ${p.from} → ${p.to}${p.relaxed ? ' (more freedom)' : ''} · by ${p.initiator || 'operator'}`,
          }),
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

/// Put a node where its seq belongs. Appending is the common case and stays
/// O(1); an event that arrives out of order — a live frame during a catch-up
/// walk, or an older page — slots into place instead of being dropped.
function insertBySeq(host, node, seq) {
  let cursor = host.lastElementChild;
  while (cursor && Number(cursor.dataset.seq) > seq) {
    cursor = cursor.previousElementSibling;
  }
  if (cursor) cursor.after(node);
  else host.prepend(node);
}

// Render whatever is new. Duplicates are dropped by the transcript, so replay
// and the live stream can overlap freely.
function appendEvents(events) {
  const host = $('transcript');
  const stick = atBottom();
  const fresh = state.transcript.accept(events);
  if (!fresh.length) return;
  for (const event of fresh) {
    const node = renderEvent(event);
    if (!node) continue;
    node.dataset.seq = event.seq;
    insertBySeq(host, node, event.seq);
  }
  updateLoadEarlier();
  if (stick) host.scrollTop = host.scrollHeight;
}

function updateLoadEarlier() {
  const earliest = state.transcript.earliest;
  $('load-earlier').classList.toggle('hidden', earliest === null || earliest <= 1);
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

// Tools for which "accept edits for this session" is a meaningful, bounded
// offer. The suggestion list is written by the agent's side of the protocol, so
// it decides nothing on its own.
const EDIT_TOOLS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit', 'Update']);

function suggestionButtons(request) {
  const buttons = [];
  const suggestions = request.permission_suggestions;
  if (!Array.isArray(suggestions)) return buttons;
  if (!EDIT_TOOLS.has(request.tool_name)) return buttons;
  for (const suggestion of suggestions) {
    const mode = suggestion && (suggestion.mode || suggestion.permissionMode);
    if (mode === 'acceptEdits') {
      buttons.push(el('button', {
        text: 'Accept edits for this session',
        onclick: async () => {
          if (!confirm('Auto-approve every edit for the rest of this session?')) return;
          await decide(request.request_id, 'allow');
          // Relaxing the permission mode is an operator decision and is
          // recorded in the agent's log as one.
          await api(`/api/agents/${state.agent.id}/permission_mode`, {
            method: 'POST',
            body: JSON.stringify({ mode: 'acceptEdits', confirm: true }),
          }).catch((err) => toast(err.message, 'error'));
        },
      }));
    }
  }
  return buttons;
}

// `AskUserQuestion` arrives on the permission channel but is not a permission:
// it is the model asking the operator something, and it reaches the queue in
// every mode, `bypass` and `dangerous` included. Approving it with the input
// untouched answers nothing — the CLI reports "the user did not answer the
// questions" and the model asks again. The answer rides back in `updatedInput`
// as `answers`, keyed by the question text and valued with the chosen label.
function isQuestion(request) {
  return request.tool_name === 'AskUserQuestion'
    && Array.isArray(request.input && request.input.questions)
    && request.input.questions.length > 0;
}

function questionCard(request) {
  const questions = request.input.questions;
  const chosen = questions.map(() => new Set());
  const typed = questions.map(() => '');
  const fields = questions.map((question, index) => {
    const name = `q${index}-${request.request_id}`;
    const options = (question.options || []).map((option) => {
      const input = el('input', {
        type: question.multiSelect ? 'checkbox' : 'radio',
        name,
        value: option.label,
        onchange: (event) => {
          if (!question.multiSelect) chosen[index].clear();
          if (event.target.checked) chosen[index].add(option.label);
          else chosen[index].delete(option.label);
        },
      });
      return el('label', { class: 'option' }, [
        input,
        el('span', { text: option.label }),
        option.description ? el('span', { class: 'small muted', text: ` — ${option.description}` }) : null,
      ]);
    });
    return el('div', { class: 'question' }, [
      question.header ? el('div', { class: 'small muted', text: question.header }) : null,
      el('div', { class: 'body', text: question.question || '' }),
      ...options,
      // The real picker always offers "Other"; typing here wins over the boxes.
      el('input', {
        class: 'other',
        placeholder: 'Other — type an answer instead',
        oninput: (event) => { typed[index] = event.target.value; },
      }),
    ]);
  });

  const answer = () => {
    const answers = {};
    for (const [index, question] of questions.entries()) {
      // Multi-select answers go back as one string: the field is a string in
      // the tool's own schema, so the labels are joined rather than nested.
      const value = typed[index].trim() || [...chosen[index]].join(', ');
      if (!value) {
        toast('Every question needs an answer.', 'warn');
        return;
      }
      answers[question.question] = value;
    }
    decide(request.request_id, 'allow', { ...request.input, answers });
  };

  return el('div', { class: 'approval' }, [
    el('h3', { text: 'The agent is asking you a question' }),
    ...fields,
    el('div', { class: 'actions' }, [
      el('button', { class: 'primary', text: 'Answer', onclick: answer }),
      el('button', {
        class: 'danger',
        text: 'Decline to answer',
        onclick: () => decide(request.request_id, 'deny'),
      }),
    ]),
  ]);
}

function renderApprovals() {
  const host = $('approvals');
  host.replaceChildren();
  for (const request of state.pending.values()) {
    if (isQuestion(request)) {
      host.append(questionCard(request));
      continue;
    }
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
  // Called after every change to `state.pending`, so this is the one place the
  // tab needs to be told.
  setAttention(state.pending.size);
}

function decide(requestId, behavior, updatedInput) {
  socket.send({
    type: 'permission_decision',
    agent_id: state.agent.id,
    request_id: requestId,
    behavior,
    message: behavior === 'deny' ? 'Denied by the operator' : null,
    updated_input: updatedInput || null,
  });
  state.pending.delete(requestId);
  renderApprovals();
  return Promise.resolve();
}

// -- header -----------------------------------------------------------------

// Mirrors PermissionMode::strictness: higher constrains the agent more. Only
// used to tell a tightening from a relaxation, which is the one the operator is
// asked to confirm.
const MODE_STRICTNESS = { ask: 3, acceptEdits: 2, bypass: 1, dangerous: 0 };

function modeLabel(mode) {
  const option = [...$('agent-mode').options].find((o) => o.value === mode);
  return option ? option.textContent : mode;
}

// The mode shown is the mode the server holds. It is put back on every failure
// and reasserted from the broadcast, so the picker never claims a freedom the
// agent was not actually given.
async function changeMode(mode) {
  const agent = state.agent;
  const current = agent.permission_mode;
  const picker = $('agent-mode');
  if (!agent || mode === current) return;
  const relaxes = MODE_STRICTNESS[mode] < MODE_STRICTNESS[current];
  if (relaxes
    && !confirm(`Switch from ${modeLabel(current)} to ${modeLabel(mode)}? That gives the agent more freedom for the rest of this session.`)) {
    picker.value = current;
    return;
  }
  picker.disabled = true;
  try {
    await api(`/api/agents/${agent.id}/permission_mode`, {
      method: 'POST',
      body: JSON.stringify({ mode, confirm: relaxes }),
    });
    agent.permission_mode = mode;
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    picker.disabled = false;
    renderHeader();
  }
}

// The display name only: the slug, the branch and this page's URL are fixed at
// launch, so a rename never moves the agent out from under the open window.
async function rename() {
  const agent = state.agent;
  if (!agent) return;
  const name = prompt('New display name (the slug and branch never change):', agent.name);
  if (name === null) return;
  const button = $('btn-rename');
  button.disabled = true;
  try {
    const data = await api(`/api/agents/${agent.id}/rename`, {
      method: 'POST',
      body: JSON.stringify({ name }),
    });
    Object.assign(state.agent, data.agent);
    renderHeader();
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    button.disabled = false;
  }
}

function renderHeader() {
  const agent = state.agent;
  if (!agent) return;
  $('agent-name').textContent = agent.name;
  $('agent-status').replaceChildren(statusEl(agent.status, agent.status_detail));
  const where = agent.is_git
    ? `${agent.branch || 'detached'} · base ${agent.base_ref || '?'} · ${agent.uses_worktree ? 'worktree' : 'main checkout'}`
    : 'no VCS';
  // The mode is the picker's job now; repeating it in the meta line would let
  // the two disagree.
  $('agent-meta').textContent = `${where} · ${fmtCost(agent.cost_usd)} · ${agent.work_path}`;
  const running = agent.status !== 'stopped' && agent.status !== 'failed';
  $('btn-interrupt').disabled = !running;
  $('btn-stop').disabled = !running;
  $('btn-resume').disabled = running;
  const picker = $('agent-mode');
  picker.value = agent.permission_mode;
  for (const option of picker.options) {
    // `--dangerously-skip-permissions` is a launch flag with no runtime
    // equivalent: the server refuses it for a running agent, so don't offer it.
    option.disabled = option.value === 'dangerous'
      && running
      && agent.permission_mode !== 'dangerous';
  }
  setTitle(`${agent.name} · claude-web`);
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
  const earliest = state.transcript.earliest;
  if (earliest === null || earliest <= 1) return;
  const after = Math.max(0, earliest - 201);
  const data = await api(`/api/agents/${state.agent.id}/events?after=${after}&limit=200`);
  appendEvents(data.events);
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
  $('btn-rename').onclick = rename;
  $('agent-mode').onchange = (event) => changeMode(event.target.value);
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

  // Subscribe with the contiguous cursor: everything at or below it is
  // accounted for. It never runs ahead of a gap, so a reconnect mid-walk picks
  // up exactly where the walk had got to.
  const subscribe = (after) => socket.send({
    type: 'subscribe',
    agent_id: state.agent.id,
    after_seq: after === undefined ? state.transcript.replayFrom || null : after,
  });
  socket.onopen = () => subscribe();
  subscribe();

  socket
    .on('replay', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      state.transcript.seed(msg.after);
      appendEvents(msg.events);
      state.pending = new Map((msg.pending_permissions || []).map((r) => [r.request_id, r]));
      renderApprovals();
      // A replay page is capped. Walk forward from the page's own cursor until
      // the server stops saying there is more. The walk deliberately ignores
      // the render state: a live event arriving mid-walk must not end it, or
      // everything between here and the head is lost for good.
      const next = nextWalkCursor(msg);
      if (next !== null) subscribe(next);
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
    .on('agent_renamed', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      state.agent.name = msg.name;
      renderHeader();
    })
    .on('permission_mode_changed', (msg) => {
      if (msg.agent_id !== state.agent.id) return;
      state.agent.permission_mode = msg.mode;
      renderHeader();
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
