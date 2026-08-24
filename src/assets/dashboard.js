// Dashboard: the agent registry, the spawn form, cloning and settings.
import { api, el, slugify, statusEl, fmtCost, fmtAgo, Socket, toast } from '/assets/common.js';

const state = {
  agents: new Map(),
  config: null,
  repos: { recent: [], all: [], errors: [] },
  selectedRepo: null,
  cloneId: null,
};

const $ = (id) => document.getElementById(id);

// -- agents -----------------------------------------------------------------

function agentCard(agent) {
  const where = agent.is_git
    ? `${agent.branch || 'detached'} · ${agent.uses_worktree ? 'worktree' : 'main checkout'}`
    : 'no VCS';
  const actions = el('div', { class: 'actions' });
  const running = agent.status !== 'stopped' && agent.status !== 'failed';

  actions.append(el('a', { class: 'btn', href: `/agent/${agent.slug}`, text: 'Open' }));
  if (running) {
    actions.append(
      el('button', { text: 'Interrupt', onclick: (e) => act(agent.id, 'interrupt', e.target) }),
    );
    actions.append(el('button', { text: 'Stop', onclick: (e) => act(agent.id, 'stop', e.target) }));
  } else {
    actions.append(
      el('button', { text: 'Resume', onclick: (e) => act(agent.id, 'resume', e.target) }),
    );
  }
  actions.append(el('button', { text: 'Rename', onclick: () => rename(agent) }));
  actions.append(el('button', { class: 'danger', text: 'Delete', onclick: () => remove(agent) }));

  return el('div', { class: 'card', 'data-id': agent.id }, [
    el('h3', {}, [
      el('a', { href: `/agent/${agent.slug}`, text: agent.name }),
    ]),
    statusEl(agent.status, agent.status_detail),
    el('div', { class: 'meta' }, [
      `${agent.repo_path}`,
      el('br'),
      `${where} · ${agent.permission_mode} · ${fmtCost(agent.cost_usd)} · ${fmtAgo(agent.last_active_at)}`,
    ]),
    agent.status === 'failed' && agent.last_stderr
      ? el('div', { class: 'errbox small', text: agent.last_stderr })
      : null,
    actions,
  ]);
}

function renderAgents() {
  const cards = $('cards');
  cards.replaceChildren();
  const list = [...state.agents.values()].sort((a, b) => b.last_active_at - a.last_active_at);
  for (const agent of list) cards.append(agentCard(agent));
  $('empty').classList.toggle('hidden', list.length > 0);
  const running = list.filter((a) => a.status !== 'stopped' && a.status !== 'failed').length;
  $('agent-count').textContent = list.length ? `— ${running} running of ${list.length}` : '';
}

async function loadAgents() {
  const data = await api('/api/agents');
  state.agents = new Map(data.agents.map((a) => [a.id, a]));
  renderAgents();
}

// One verb per agent at a time. A double-clicked Resume would otherwise put
// two requests in flight for one session.
const inFlight = new Set();

async function act(id, verb, button) {
  const key = `${id}:${verb}`;
  if (inFlight.has(key)) return;
  inFlight.add(key);
  if (button) button.disabled = true;
  try {
    await api(`/api/agents/${id}/${verb}`, { method: 'POST', body: '{}' });
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    inFlight.delete(key);
    if (button && button.isConnected) button.disabled = false;
  }
}

async function rename(agent) {
  const name = prompt('New display name (the slug and branch never change):', agent.name);
  if (!name) return;
  try {
    const data = await api(`/api/agents/${agent.id}/rename`, {
      method: 'POST',
      body: JSON.stringify({ name }),
    });
    state.agents.set(data.agent.id, { ...agent, ...data.agent });
    renderAgents();
  } catch (err) {
    toast(err.message, 'error');
  }
}

async function remove(agent) {
  if (!confirm(`Delete "${agent.name}"? The branch is kept by default.`)) return;
  const deleteBranch = agent.branch
    ? confirm(`Also delete the branch ${agent.branch}?`)
    : false;
  const qs = `?delete_branch=${deleteBranch}`;
  try {
    await api(`/api/agents/${agent.id}${qs}`, { method: 'DELETE' });
  } catch (err) {
    if (err.status === 409 && err.body && err.body.report) {
      const report = err.body.report;
      const lost = [
        ...report.uncommitted.map((l) => `  uncommitted: ${l}`),
        ...report.unpushed.map((l) => `  unpushed: ${l}`),
      ].join('\n');
      if (!confirm(`${err.body.error}\n\nThis would be lost:\n${lost}\n\nDelete anyway?`)) return;
      try {
        await api(`/api/agents/${agent.id}${qs}&force=true`, { method: 'DELETE' });
      } catch (inner) {
        toast(inner.message, 'error');
      }
    } else {
      toast(err.message, 'error');
    }
  }
}

// -- repo picker ------------------------------------------------------------

function repoRow(repo) {
  const badges = [
    el('span', { class: `badge ${repo.is_git ? 'git' : 'plain'}`, text: repo.is_git ? 'git' : 'plain' }),
  ];
  if (repo.branch) badges.push(el('span', { class: 'badge', text: repo.branch }));
  if (repo.dirty) badges.push(el('span', { class: 'badge dirty', text: 'dirty' }));
  // Its own git config declares commands, so nothing was run in it — and
  // spawning there is refused for the same reason.
  if (repo.refused) {
    badges.push(el('span', {
      class: 'badge dirty',
      text: 'not inspected',
      title: repo.refused,
    }));
  }
  return el(
    'div',
    {
      class: `repo${state.selectedRepo && state.selectedRepo.path === repo.path ? ' selected' : ''}`,
      onclick: () => selectRepo(repo),
    },
    [el('span', { class: 'name', text: repo.name }), ...badges],
  );
}

function renderRepos() {
  const list = $('repo-list');
  list.replaceChildren();
  if (state.repos.recent.length) {
    list.append(el('div', { class: 'group-label', text: 'Recent' }));
    for (const repo of state.repos.recent) list.append(repoRow(repo));
  }
  list.append(el('div', { class: 'group-label', text: 'All' }));
  for (const repo of state.repos.all) list.append(repoRow(repo));
  $('repo-errors').textContent = state.repos.errors.join(' · ');
}

async function selectRepo(repo) {
  if (repo.refused) {
    toast(repo.refused, 'error');
    return;
  }
  state.selectedRepo = repo;
  renderRepos();
  if (!$('task-name').value.trim()) $('task-name').value = repo.name;
  updateBranchPreview();

  const info = await api(`/api/repos/branches?path=${encodeURIComponent(repo.path)}`).catch(() => null);
  const select = $('base-ref');
  select.replaceChildren();
  if (info && info.is_git) {
    for (const branch of info.branches) {
      select.append(el('option', { value: branch, text: branch }));
    }
    if (info.current) select.value = info.current;
  } else {
    select.append(el('option', { value: '', text: '(no VCS)' }));
  }
  renderSpawnWarnings(info);
}

function renderSpawnWarnings(info) {
  const host = $('spawn-warnings');
  host.replaceChildren();
  if (!info) return;
  if (info.dirty && !$('in-place').checked) {
    host.append(el('div', {
      class: 'warnbox small',
      text: 'This checkout has uncommitted changes. A new worktree starts from HEAD, so those changes will be invisible to the agent.',
    }));
  }
  if (!info.is_git) {
    host.append(el('div', {
      class: 'warnbox small',
      text: 'Not a git repository. The agent runs in place with no branch — nothing is ever git init-ed for you.',
    }));
  }
}

function updateBranchPreview() {
  const prefix = state.config ? state.config.branch_prefix : 'sw_';
  const repo = state.selectedRepo;
  const name = $('task-name').value.trim();
  if (!repo || !repo.is_git) {
    $('branch-preview').value = repo ? '(no VCS — no branch)' : '';
    return;
  }
  $('branch-preview').value = prefix + slugify(name || repo.name);
}

async function loadRepos() {
  state.repos = await api('/api/repos');
  renderRepos();
  const rootSelect = $('clone-root');
  rootSelect.replaceChildren();
  const roots = state.config ? state.config.repo_roots : [];
  for (const root of roots) rootSelect.append(el('option', { value: root, text: root }));
}

// -- spawn ------------------------------------------------------------------

async function spawn() {
  if (!state.selectedRepo) {
    toast('Pick a repository first', 'warn');
    return;
  }
  const budget = parseFloat($('budget').value);
  const body = {
    repo_path: state.selectedRepo.path,
    task_name: $('task-name').value.trim() || state.selectedRepo.name,
    base_ref: $('base-ref').value || null,
    model: $('model').value.trim() || null,
    effort: $('effort').value.trim() || null,
    max_budget_usd: Number.isFinite(budget) ? budget : null,
    permission_mode: $('permission-mode').value,
    in_place: $('in-place').checked,
    add_dirs: $('add-dirs').value.split('\n').map((s) => s.trim()).filter(Boolean),
    first_message: $('first-message').value.trim() || null,
  };
  const button = $('spawn-btn');
  button.disabled = true;
  try {
    const data = await api('/api/agents', { method: 'POST', body: JSON.stringify(body) });
    if (data.warning) toast(data.warning, 'warn');
    $('first-message').value = '';
    location.href = `/agent/${data.agent.slug}`;
  } catch (err) {
    toast(err.message, 'error');
  } finally {
    button.disabled = false;
  }
}

// -- clone ------------------------------------------------------------------

async function startClone() {
  const url = $('clone-url').value.trim();
  if (!url) return;
  $('clone-log').textContent = '';
  try {
    const data = await api('/api/repos/clone', {
      method: 'POST',
      body: JSON.stringify({
        url,
        root: $('clone-root').value || null,
        folder: $('clone-folder').value.trim() || null,
      }),
    });
    state.cloneId = data.clone_id;
  } catch (err) {
    toast(err.message, 'error');
  }
}

// -- settings ---------------------------------------------------------------

function fillSettings() {
  const cfg = state.config;
  if (!cfg) return;
  $('cfg-roots').value = cfg.repo_roots.join('\n');
  $('cfg-prefix').value = cfg.branch_prefix;
  $('cfg-max').value = cfg.max_agents;
  $('cfg-model').value = cfg.default_model;
  $('cfg-bin').value = cfg.claude_bin;
  $('cfg-mode').value = cfg.default_permission_mode;
  $('cfg-pin').value = cfg.pinned_cli_version;
  $('cfg-open').checked = cfg.open_browser;
}

async function saveSettings() {
  const cfg = {
    ...state.config,
    repo_roots: $('cfg-roots').value.split('\n').map((s) => s.trim()).filter(Boolean),
    branch_prefix: $('cfg-prefix').value,
    max_agents: parseInt($('cfg-max').value, 10) || 8,
    default_model: $('cfg-model').value,
    claude_bin: $('cfg-bin').value,
    default_permission_mode: $('cfg-mode').value,
    pinned_cli_version: $('cfg-pin').value,
    open_browser: $('cfg-open').checked,
  };
  try {
    state.config = await api('/api/config', { method: 'PUT', body: JSON.stringify(cfg) });
    toast('Settings saved');
    await loadRepos();
    updateBranchPreview();
  } catch (err) {
    toast(err.message, 'error');
  }
}

// -- wiring -----------------------------------------------------------------

function togglePanel(id) {
  const panel = $(id);
  panel.classList.toggle('hidden');
  if (id === 'spawn-panel' && !panel.classList.contains('hidden')) loadRepos();
}

async function main() {
  state.config = await api('/api/config').catch(() => null);
  fillSettings();
  await loadAgents();
  await loadRepos();

  $('toggle-spawn').onclick = () => togglePanel('spawn-panel');
  $('toggle-clone').onclick = () => togglePanel('clone-panel');
  $('toggle-settings').onclick = () => togglePanel('settings-panel');
  $('task-name').oninput = updateBranchPreview;
  $('in-place').onchange = () => state.selectedRepo && selectRepo(state.selectedRepo);
  $('spawn-btn').onclick = spawn;
  $('clone-btn').onclick = startClone;
  $('cfg-save').onclick = saveSettings;
  $('fetch-btn').onclick = async () => {
    if (!state.selectedRepo) return;
    try {
      await api('/api/repos/fetch', {
        method: 'POST',
        body: JSON.stringify({ path: state.selectedRepo.path }),
      });
      toast('Fetched');
      await selectRepo(state.selectedRepo);
    } catch (err) {
      toast(err.message, 'error');
    }
  };
  $('clone-url').oninput = () => {
    if ($('clone-folder').dataset.touched) return;
    const match = $('clone-url').value.trim().replace(/\/$/, '').split(/[/:]/).pop();
    $('clone-folder').value = (match || '').replace(/\.git$/, '');
  };
  $('clone-folder').oninput = () => { $('clone-folder').dataset.touched = '1'; };

  const socket = new Socket();
  socket.onopen = () => { $('conn').textContent = 'live'; };
  socket
    .on('status', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      if (!agent) return;
      Object.assign(agent, {
        status: msg.status,
        status_detail: msg.status_detail,
        exit_code: msg.exit_code,
        last_stderr: msg.last_stderr ?? agent.last_stderr,
        cost_usd: msg.cost_usd,
        last_active_at: Date.now(),
      });
      renderAgents();
    })
    .on('agent_added', (msg) => {
      state.agents.set(msg.agent.id, msg.agent);
      renderAgents();
    })
    .on('agent_removed', (msg) => {
      state.agents.delete(msg.agent_id);
      renderAgents();
    })
    .on('permission_request', (msg) => {
      const agent = state.agents.get(msg.agent_id);
      toast(`${agent ? agent.name : msg.agent_id} needs approval for ${msg.request.tool_name}`, 'warn');
    })
    .on('notice', (msg) => toast(msg.text, msg.level))
    .on('clone_progress', (msg) => {
      const log = $('clone-log');
      log.textContent = `${log.textContent}${msg.line}\n`.split('\n').slice(-12).join('\n');
    })
    .on('clone_done', async (msg) => {
      if (msg.error) {
        $('clone-log').textContent += `\n${msg.error}\n`;
        toast('Clone failed', 'error');
      } else {
        toast(`Cloned into ${msg.path}`);
        await loadRepos();
      }
    });

  // Keep relative timestamps honest without a full re-render storm.
  setInterval(renderAgents, 30000);
}

main().catch((err) => toast(err.message, 'error'));
