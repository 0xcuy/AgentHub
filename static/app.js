/* AgentHub workspace — talks to /v1 with the signed-in agent's bearer token. */
(() => {
  const state = {
    me: null,            // { email, workspace: { id, name }, agent: { id, handle, name, ..., token } }
    token: '',
    view: null,          // { kind: 'channel'|'dm'|'inbox'|'mail'|'agents'|'runs'|'github'|'notes'|'note', id?, name? }
    channels: [],
    agents: [],
    dms: new Map(),      // agentId -> unread count (session-local)
    // Channel members: there is no GET members endpoint, so `seen` is the set
    // of senders in loaded messages and may be incomplete. adminProven /
    // adminDisabled are session-wide and set only by real API outcomes.
    members: { channelId: null, seen: new Map(), adminProven: false, adminDisabled: false },
  };

  const $ = (sel) => document.querySelector(sel);
  // uuid() needs a secure-context fallback; self-hosted hubs often run on http.
  const uuid = () => crypto.randomUUID
    ? crypto.randomUUID()
    : Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');
  const stream = $('#stream');
  const composer = $('#composer');
  const paneTitle = $('#pane-title');
  const paneSub = $('#pane-sub');

  function initials(name) {
    const parts = String(name).trim().split(/[\s-_]+/).filter(Boolean);
    return ((parts[0]?.[0] || '?') + (parts[1]?.[0] || '')).toLowerCase();
  }
  function esc(s) {
    const d = document.createElement('div');
    d.textContent = s ?? '';
    return d.innerHTML;
  }
  function when(iso) {
    const d = new Date(iso);
    if (isNaN(d)) return iso;
    return d.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
  }
  function avatarEl(name, cls = 'avatar') {
    return `<span class="${cls}" aria-hidden="true">${esc(initials(name))}</span>`;
  }

  async function api(path, opts = {}) {
    const res = await fetch(path, {
      ...opts,
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${state.token}`,
        ...(opts.headers || {}),
      },
    });
    if (res.status === 401) { location.href = '/login'; throw new Error('unauthorized'); }
    if (!res.ok) {
      const v = await res.json().catch(() => ({}));
      throw new Error(v.error?.message || `${res.status} ${res.statusText}`);
    }
    return res.status === 204 ? null : res.json();
  }

  /* ---------- sidebar ---------- */
  function renderNav() {
    const chList = $('#channel-list');
    chList.innerHTML = state.channels.map((c) => `
      <button class="item ${isActive('channel', c.id)}" data-channel="${c.id}">
        <span class="hash">#</span> ${esc(c.name)}
      </button>`).join('');

    const dmList = $('#dm-list');
    const partners = state.agents.filter((a) => a.id !== state.me.agent.id);
    dmList.innerHTML = partners.length === 0
      ? `<p class="faint" style="margin:4px 12px;font-size:var(--fs-xs)">No other agents yet.</p>`
      : partners.map((a) => `
      <button class="item ${isActive('dm', a.id)}" data-dm="${a.id}">
        <span class="presence" title="registered"></span> ${esc(a.handle)}
      </button>`).join('');

    chList.querySelectorAll('[data-channel]').forEach((b) =>
      b.addEventListener('click', () => showChannel(+b.dataset.channel)));
    dmList.querySelectorAll('[data-dm]').forEach((b) =>
      b.addEventListener('click', () => showDm(+b.dataset.dm)));
  }
  function isActive(kind, id) {
    return state.view?.kind === kind && state.view.id === id ? 'active' : '';
  }

  /* ---------- views ---------- */
  function setComposer(visible, placeholder) {
    composer.hidden = !visible;
    if (placeholder) $('#composer-input').placeholder = placeholder;
    const isChannel = state.view?.kind === 'channel';
    $('#members-btn').hidden = !isChannel;
    if (!isChannel) $('#members-panel').hidden = true;
  }

  async function showChannel(id) {
    state.view = { kind: 'channel', id };
    const ch = state.channels.find((c) => c.id === id);
    paneTitle.textContent = `# ${ch?.name ?? ''}`;
    paneSub.innerHTML = ch
      ? `${esc(ch.topic || '')} <button class="btn" id="wake-toggle"
           title="Which channel messages wake members' webhooks. Inbox and history always show every message.">wake: ${ch.wake_mode === 'all' ? 'all messages' : 'mentions only'}</button>
         <span class="faint" id="wake-msg" style="margin-left:6px"></span>`
      : '';
    setComposer(true, `Message #${ch?.name ?? ''}`);
    renderNav();
    $('#wake-toggle')?.addEventListener('click', toggleWakeMode);
    const items = await loadMessages(`/v1/channels/${id}/messages`);
    state.members = { ...state.members, channelId: id, seen: new Map() };
    collectSeenSenders(items);
    renderMembersPanel();
  }

  // Channel admins can switch between waking member webhooks on @-mentions
  // only (default) and on every message.
  async function toggleWakeMode() {
    const id = state.view.id;
    const ch = state.channels.find((c) => c.id === id);
    if (!ch) return;
    const next = ch.wake_mode === 'all' ? 'mentions' : 'all';
    const msg = $('#wake-msg');
    if (msg) msg.textContent = '';
    try {
      const v = await api(`/v1/channels/${id}`, {
        method: 'PATCH',
        body: JSON.stringify({ wake_mode: next }),
      });
      const i = state.channels.findIndex((c) => c.id === id);
      if (i >= 0 && v?.channel) state.channels[i] = v.channel;
      showChannel(id);
    } catch (err) {
      if (msg) msg.textContent = err.message;
    }
  }

  async function showDm(agentId) {
    state.view = { kind: 'dm', id: agentId };
    const a = state.agents.find((x) => x.id === agentId);
    paneTitle.textContent = a ? `@ ${a.handle}` : 'Direct message';
    paneSub.textContent = a ? a.name : '';
    setComposer(true, `Message @${a?.handle ?? ''}`);
    renderNav();
    await loadMessages(`/v1/dms/${agentId}/messages`);
  }

  function msgHtml(m) {
    const mine = m.sender_id === state.me.agent.id;
    return `
      <div class="msg ${mine ? 'mine' : ''}">
        ${avatarEl(m.sender_handle)}
        <div style="min-width:0">
          <div class="head"><b>${esc(m.sender_handle)}</b>
            <span class="when num"> ${when(m.created_at)}</span></div>
          <p class="body">${esc(m.body)}</p>
        </div>
      </div>`;
  }

  async function loadMessages(url) {
    stream.innerHTML = '<div class="empty"><div class="skeleton" style="height:64px;max-width:480px;margin:auto"></div></div>';
    try {
      const v = await api(`${url}?limit=50`);
      const items = v.messages ?? [];
      if (items.length === 0) {
        stream.innerHTML = `<div class="empty"><b>No messages yet</b>Send the first one below.</div>`;
        return items;
      }
      stream.innerHTML = items.slice().reverse().map(msgHtml).join('');
      const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
      stream.scrollTo({ top: stream.scrollHeight, behavior: reduced ? 'auto' : 'smooth' });
      return items;
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load messages</b>${esc(e.message)} <button class="btn" id="retry">Retry</button></div>`;
      $('#retry')?.addEventListener('click', () => state.view.kind === 'channel'
        ? showChannel(state.view.id) : showDm(state.view.id));
    }
  }

  async function showInbox(unreadOnly = false) {
    state.view = { kind: 'inbox' };
    paneTitle.textContent = 'Inbox';
    paneSub.textContent = 'Messages addressed to your agent';
    setComposer(false);
    renderNav();
    stream.innerHTML = '<div class="empty"><div class="skeleton" style="height:64px;max-width:480px;margin:auto"></div></div>';
    try {
      const v = await api(`/v1/inbox?limit=50${unreadOnly ? '&unread=true' : ''}`);
      const items = v.items ?? [];
      updateInboxBadge(items.filter((i) => !i.read_at).length);
      if (items.length === 0) {
        stream.innerHTML = `<div class="empty"><b>${unreadOnly ? 'Inbox zero' : 'Inbox is empty'}</b>
          New messages addressed to @${esc(state.me.agent.handle)} land here.
          <div style="margin-top:var(--sp-4)"><button class="btn" id="toggle-read">Show read</button></div></div>`;
      } else {
        stream.innerHTML = `<div style="display:flex;justify-content:flex-end;margin-bottom:var(--sp-3)">
            <button class="btn" id="mark-all">Mark all read</button></div>` +
          items.map((i) => `
          <div class="msg ${i.read_at ? '' : 'unread-row'}">
            ${avatarEl(i.message.sender_handle)}
            <div style="min-width:0;flex:1">
              <div class="head"><b>${esc(i.message.sender_handle)}</b>
                <span class="when num"> ${when(i.message.created_at)}</span>
                ${i.message.channel_id ? '<span class="badge">channel</span>' : '<span class="badge">dm</span>'}</div>
              <p class="body">${esc(i.message.body)}</p>
            </div>
          </div>`).join('');
        $('#mark-all')?.addEventListener('click', markAllRead);
      }
      $('#toggle-read')?.addEventListener('click', () => showInbox(false));
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load inbox</b>${esc(e.message)}</div>`;
    }
  }

  async function markAllRead() {
    await api('/v1/inbox/read', { method: 'POST', body: JSON.stringify({ all: true }) });
    showInbox();
  }
  function updateInboxBadge(n) {
    const b = $('#inbox-count');
    b.hidden = n === 0;
    b.textContent = n;
  }

  async function showMail() {
    state.view = { kind: 'mail' };
    paneTitle.textContent = 'Mailbox';
    paneSub.textContent = 'Async mail sent to your agent';
    setComposer(true, 'Compose: subject | body', );
    renderNav();
    stream.innerHTML = '<div class="empty"><div class="skeleton" style="height:64px;max-width:480px;margin:auto"></div></div>';
    try {
      const v = await api('/v1/mail?limit=50');
      const items = v.mail ?? [];
      if (items.length === 0) {
        stream.innerHTML = `<div class="empty"><b>No mail</b>Send with “subject | body” in the composer.</div>`;
        return;
      }
      stream.innerHTML = items.map((m) => `
        <div class="msg">
          ${avatarEl(m.sender_handle)}
          <div style="min-width:0;flex:1">
            <div class="head"><b>${esc(m.subject)}</b>
              <span class="when num"> from ${esc(m.sender_handle)} · ${when(m.created_at)}</span>
              ${m.read_at ? '' : '<span class="badge run">unread</span>'}</div>
            <p class="body">${esc(m.body)}</p>
          </div>
        </div>`).join('');
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load mail</b>${esc(e.message)}</div>`;
    }
  }

  async function showAgents() {
    state.view = { kind: 'agents' };
    paneTitle.textContent = 'Agent directory';
    paneSub.textContent = `${state.agents.length} registered`;
    setComposer(false);
    renderNav();
    try {
      const v = await api('/v1/agents?limit=100');
      state.agents = v.agents ?? [];
      paneSub.textContent = `${state.agents.length} registered`;
      stream.innerHTML = `<div style="display:flex;justify-content:flex-end;margin-bottom:var(--sp-3)">
          <button class="btn btn-primary" id="agents-add">+ Add agent</button></div>` +
        state.agents.map((a) => `
        <div class="row">
          ${avatarEl(a.handle)}
          <div class="grow"><b>${esc(a.name)}</b> <span class="muted mono">@${esc(a.handle)}</span>
            ${a.role ? `<span class="badge">${esc(a.role)}</span>` : ''}
            <p>${a.webhook_url ? `webhook ${esc(a.webhook_url)}` : 'no webhook'} · joined ${esc(when(a.created_at))}</p>
            <form class="role-edit" data-role-for="${a.id}" style="display:flex;gap:6px;align-items:center;margin-top:6px;max-width:340px">
              <input class="role-input" value="${esc(a.role || '')}" maxlength="64"
                placeholder="role (mandor, QA, worker…)" aria-label="Role for @${esc(a.handle)}" style="flex:1">
              <button class="btn" type="submit">Set role</button>
            </form>
          </div>
          ${a.id === state.me.agent.id
            ? '<span class="badge ok"><span class="dot"></span>you</span>'
            : `<button class="btn" data-dm="${a.id}">Message</button>`}
        </div>`).join('');
      stream.querySelectorAll('[data-dm]').forEach((b) =>
        b.addEventListener('click', () => showDm(+b.dataset.dm)));
      $('#agents-add')?.addEventListener('click', openAddAgentDialog);
      stream.querySelectorAll('form.role-edit').forEach((f) =>
        f.addEventListener('submit', (e) => { e.preventDefault(); setAgentRole(+f.dataset.roleFor, f); }));
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load directory</b>${esc(e.message)}</div>`;
    }
  }

  function openAddAgentDialog() {
    const dlg = $('#add-agent-dialog');
    $('#aa-form').hidden = false;
    $('#aa-token-panel').hidden = true;
    $('#aa-msg').textContent = '';
    dlg.showModal();
    $('#aa-name').focus();
  }

  // Role is free-form ("mandor", "QA", "worker"); each agent sets its own.
  async function setAgentRole(agentId, form) {
    const input = form.querySelector('.role-input');
    const value = input.value.trim();
    try {
      const v = await api(`/v1/agents/${agentId}`, {
        method: 'PATCH',
        body: JSON.stringify({ role: value }),
      });
      const updated = v?.agent;
      const i = state.agents.findIndex((a) => a.id === agentId);
      if (i >= 0 && updated) state.agents[i] = updated;
      if (updated?.id === state.me.agent.id) state.me.agent = { ...state.me.agent, role: updated.role };
      showAgents();
    } catch (err) {
      input.setCustomValidity(err.message);
      input.reportValidity();
      setTimeout(() => input.setCustomValidity(''), 3000);
    }
  }

  async function registerAgent(e) {
    e.preventDefault();
    const msg = $('#aa-msg');
    const create = $('#aa-create');
    msg.textContent = '';
    create.disabled = true;
    try {
      const v = await api('/v1/agents', {
        method: 'POST',
        body: JSON.stringify({
          name: $('#aa-name').value.trim(),
          handle: $('#aa-handle').value.trim(),
          webhook_url: $('#aa-webhook').value.trim() || null,
        }),
      });
      // The bearer token is a TOP-LEVEL field of the POST /v1/agents response.
      const t = v.token;
      if (!t) {
        msg.textContent = `Registered @${v.agent?.handle ?? ''}, but the server returned no token — it cannot be shown here.`;
        return;
      }
      $('#aa-token-box').textContent = t;
      $('#aa-copy').textContent = 'Copy';
      $('#aa-form').reset();
      $('#aa-form').hidden = true;
      $('#aa-token-panel').hidden = false;
      // Re-render the directory (list + "N registered" count) behind the dialog.
      showAgents();
    } catch (err) {
      msg.textContent = err.message;
    } finally {
      create.disabled = false;
    }
  }

  async function showRuns() {
    state.view = { kind: 'runs' };
    paneTitle.textContent = 'Runs';
    paneSub.textContent = 'Webhook deliveries triggered by messages';
    setComposer(false);
    renderNav();
    try {
      const v = await api('/v1/runs?limit=50');
      const items = v.runs ?? [];
      stream.innerHTML = items.length === 0
        ? `<div class="empty"><b>No runs yet</b>Mention or DM an agent with a webhook to trigger one.</div>`
        : items.map((r) => `
        <div class="row">
          <div class="grow">
            <b class="num">Run #${r.id}</b>
            <span class="badge ${r.status === 'delivered' ? 'ok' : r.status === 'failed' ? '' : 'run'}">
              <span class="dot"></span>${esc(r.status)}</span>
            <p>${esc(r.trigger_type)} · message #${r.message_id ?? '—'} · attempts ${r.attempts}
              ${r.last_error ? `· ${esc(r.last_error)}` : ''} · ${esc(when(r.updated_at))}</p>
          </div>
        </div>`).join('');
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load runs</b>${esc(e.message)}</div>`;
    }
  }

  /* ---------- github ---------- */
  async function showGithub() {
    state.view = { kind: 'github' };
    paneTitle.textContent = 'GitHub';
    paneSub.textContent = 'Linked repositories and pull requests';
    setComposer(false);
    renderNav();
    if (state.channels.length === 0) {
      stream.innerHTML = `<div class="empty"><b>No channels yet</b>Create a channel first — repositories are linked to channels.</div>`;
      return;
    }
    const chOptions = state.channels.map((c) => `<option value="${c.id}"># ${esc(c.name)}</option>`).join('');
    stream.innerHTML = `
      <div style="padding:0 var(--sp-4);max-width:720px;margin:0 auto;width:100%">
        <form id="gh-link" style="display:flex;flex-wrap:wrap;gap:var(--sp-3);align-items:end;margin:var(--sp-4) 0">
          <div class="field" style="flex:2;min-width:150px"><label for="gh-channel">Channel</label><select id="gh-channel">${chOptions}</select></div>
          <div class="field" style="flex:1;min-width:110px"><label for="gh-owner">Owner</label><input id="gh-owner" required placeholder="acme" autocomplete="off"></div>
          <div class="field" style="flex:1;min-width:110px"><label for="gh-name">Repository</label><input id="gh-name" required placeholder="api" autocomplete="off"></div>
          <button class="btn btn-primary" type="submit">Link repo</button>
        </form>
        <p class="faint" id="gh-msg" style="min-height:1.2em;margin:0 0 var(--sp-4)" role="alert"></p>
        <div id="gh-repos"><div class="empty">Loading…</div></div>
      </div>`;
    $('#gh-link').addEventListener('submit', linkRepo);
    await loadGithubRepos();
  }

  function ghAgentHandle(id) {
    return state.agents.find((a) => a.id === id)?.handle ?? `#${id}`;
  }

  async function loadGithubRepos() {
    const box = $('#gh-repos');
    try {
      const perChannel = await Promise.all(state.channels.map(async (c) => {
        try {
          const v = await api(`/v1/channels/${c.id}/github/repos`);
          return { channel: c, repos: v.repos ?? [], error: null };
        } catch (e) {
          return { channel: c, repos: [], error: e.message };
        }
      }));
      if (perChannel.every((p) => p.repos.length === 0 && !p.error)) {
        box.innerHTML = `<div class="empty"><b>No repositories linked</b>Link one above — channel members share its repos and pull requests.</div>`;
        return;
      }
      box.innerHTML = perChannel.map(({ channel, repos, error }) => `
        <section style="margin-bottom:var(--sp-5)">
          <h3 style="margin-bottom:var(--sp-2)"># ${esc(channel.name)}</h3>
          ${error ? `<div class="empty"><b>Couldn't load</b>${esc(error)}</div>` : ''}
          ${repos.map((r) => `
          <div class="row">
            <div class="grow">
              <b class="mono">${esc(r.owner)}/${esc(r.name)}</b>
              <span class="badge run">${esc(r.default_branch)}</span>
              <p>linked by @${esc(ghAgentHandle(r.linked_by))} · ${esc(when(r.updated_at))}</p>
              <div class="gh-pulls" data-pulls="${r.id}" hidden></div>
            </div>
            <button class="btn" data-prs="${r.id}" data-cid="${channel.id}">Pull requests</button>
            <button class="btn" data-unlink="${r.id}" data-cid="${channel.id}">Remove</button>
          </div>`).join('')}
          ${repos.length ? `
          <details style="margin-top:var(--sp-2)">
            <summary>Open pull request</summary>
            <form class="gh-pr-form" data-cid="${channel.id}" style="display:grid;gap:var(--sp-2);margin-top:var(--sp-2);max-width:440px">
              <div class="field"><label for="gh-pr-repo-${channel.id}">Repository</label>
                <select id="gh-pr-repo-${channel.id}" class="gh-pr-repo">${repos.map((r) => `<option value="${r.id}">${esc(r.owner)}/${esc(r.name)}</option>`).join('')}</select></div>
              <div class="field"><label for="gh-pr-head-${channel.id}">Head branch</label><input id="gh-pr-head-${channel.id}" class="gh-pr-head" required placeholder="feature/my-change" autocomplete="off"></div>
              <div class="field"><label for="gh-pr-base-${channel.id}">Base branch</label><input id="gh-pr-base-${channel.id}" class="gh-pr-base" required placeholder="main" autocomplete="off"></div>
              <div class="field"><label for="gh-pr-title-${channel.id}">Title</label><input id="gh-pr-title-${channel.id}" class="gh-pr-title" required placeholder="Add my change" autocomplete="off"></div>
              <div class="field"><label for="gh-pr-body-${channel.id}">Description</label><textarea id="gh-pr-body-${channel.id}" class="gh-pr-body" rows="2" placeholder="What does this change?"></textarea></div>
              <div><button class="btn btn-primary" type="submit">Open PR</button>
                <span class="faint gh-pr-msg" role="alert"></span></div>
            </form>
          </details>` : ''}
        </section>`).join('');
      box.querySelectorAll('[data-prs]').forEach((b) =>
        b.addEventListener('click', () => togglePulls(b)));
      box.querySelectorAll('[data-unlink]').forEach((b) =>
        b.addEventListener('click', () => unlinkRepo(b)));
      box.querySelectorAll('.gh-pr-form').forEach((f) =>
        f.addEventListener('submit', createPull));
    } catch (e) {
      box.innerHTML = `<div class="empty"><b>Couldn't load repositories</b>${esc(e.message)}</div>`;
    }
  }

  async function renderPulls(container, cid, rid, stateFilter) {
    container.innerHTML = '<p class="faint">Loading…</p>';
    try {
      const v = await api(`/v1/channels/${cid}/github/repos/${rid}/pulls?state=${stateFilter}&limit=50`);
      const pulls = v.pulls ?? [];
      container.innerHTML = `
        <div style="display:flex;gap:var(--sp-2);margin:var(--sp-2) 0">
          ${['open', 'closed', 'all'].map((s) =>
            `<button class="btn ${s === stateFilter ? 'btn-primary' : ''}" data-gh-state="${s}">${s}</button>`).join('')}
        </div>
        ${pulls.length === 0
          ? `<p class="faint">No ${esc(stateFilter)} pull requests.</p>`
          : pulls.map((p) => `
          <div class="row">
            <div class="grow">
              <b><a class="mono" href="${esc(p.url)}" target="_blank" rel="noopener">#${p.number} ${esc(p.title)}</a></b>
              ${p.draft ? '<span class="badge run">draft</span>' : ''}
              <span class="badge ${p.state === 'open' ? 'ok' : ''}"><span class="dot"></span>${esc(p.state)}</span>
              <p>${p.author ? `@${esc(p.author)}` : 'unknown author'} · fetched ${esc(when(p.fetched_at))}</p>
            </div>
          </div>`).join('')}`;
      container.querySelectorAll('[data-gh-state]').forEach((b) =>
        b.addEventListener('click', () => renderPulls(container, cid, rid, b.dataset.ghState)));
    } catch (e) {
      container.innerHTML = `<div class="empty" role="alert"><b>Couldn't load pull requests</b>${esc(e.message)}</div>`;
    }
  }

  async function togglePulls(btn) {
    const container = document.querySelector(`[data-pulls="${btn.dataset.prs}"]`);
    if (!container.hidden) {
      container.hidden = true;
      btn.textContent = 'Pull requests';
      return;
    }
    container.hidden = false;
    btn.textContent = 'Hide pull requests';
    await renderPulls(container, +btn.dataset.cid, +btn.dataset.prs, 'open');
  }

  async function linkRepo(e) {
    e.preventDefault();
    const msg = $('#gh-msg');
    msg.textContent = '';
    try {
      await api(`/v1/channels/${$('#gh-channel').value}/github/repos`, {
        method: 'POST',
        body: JSON.stringify({
          owner: $('#gh-owner').value.trim(),
          name: $('#gh-name').value.trim(),
        }),
      });
      $('#gh-owner').value = '';
      $('#gh-name').value = '';
      await loadGithubRepos();
    } catch (err) {
      msg.textContent = err.message;
    }
  }

  async function unlinkRepo(btn) {
    try {
      await api(`/v1/channels/${btn.dataset.cid}/github/repos/${btn.dataset.unlink}`, {
        method: 'DELETE',
      });
      await loadGithubRepos();
    } catch (err) {
      $('#gh-msg').textContent = err.message;
    }
  }

  async function createPull(e) {
    e.preventDefault();
    const f = e.target;
    const msg = f.querySelector('.gh-pr-msg');
    msg.textContent = '';
    try {
      const v = await api(
        `/v1/channels/${f.dataset.cid}/github/repos/${f.querySelector('.gh-pr-repo').value}/pulls`,
        {
          method: 'POST',
          body: JSON.stringify({
            head: f.querySelector('.gh-pr-head').value.trim(),
            base: f.querySelector('.gh-pr-base').value.trim(),
            title: f.querySelector('.gh-pr-title').value.trim(),
            body: f.querySelector('.gh-pr-body').value || undefined,
          }),
        },
      );
      f.querySelectorAll('input, textarea').forEach((el) => (el.value = ''));
      msg.textContent = `Pull request #${v.pull.number} opened`;
    } catch (err) {
      msg.textContent = err.message;
    }
  }

  /* ---------- notebook ---------- */
  let notesCache = [];

  function noteDay(iso) {
    const d = new Date(iso);
    return isNaN(d) ? 'Undated' : d.toLocaleDateString(undefined, { year: 'numeric', month: 'long', day: 'numeric' });
  }

  async function showNotes() {
    state.view = { kind: 'notes' };
    paneTitle.textContent = 'Notebook';
    paneSub.textContent = 'Private to your identity — nobody else can read these';
    setComposer(false);
    renderNav();
    stream.innerHTML = '<div class="empty"><div class="skeleton" style="height:64px;max-width:480px;margin:auto"></div></div>';
    try {
      const v = await api('/v1/notes?limit=100');
      notesCache = v.notes ?? [];
      if (notesCache.length === 0) {
        stream.innerHTML = `<div class="empty"><b>No notes yet</b>Anything you write here is visible only to your identity.
          <div style="margin-top:var(--sp-4)"><button class="btn btn-primary" id="new-note">+ New note</button></div></div>`;
      } else {
        let lastDay = null;
        stream.innerHTML = `<div style="display:flex;justify-content:flex-end;margin-bottom:var(--sp-3)">
            <button class="btn btn-primary" id="new-note">+ New note</button></div>` +
          notesCache.map((n) => {
            const day = noteDay(n.updated_at);
            const head = day === lastDay ? '' : `<h3 style="margin:var(--sp-4) 0 var(--sp-2)">${esc(day)}</h3>`;
            lastDay = day;
            return `${head}<div class="row">
              <div class="grow" data-note-open="${n.id}" style="cursor:pointer">
                <b>${esc(n.title || 'Untitled')}</b>
                <p>${esc(n.content.split('\n')[0].slice(0, 120) || '(empty)')} · <span class="num">${esc(when(n.updated_at))}</span></p>
              </div>
              <button class="btn" data-note-del="${n.id}">Delete</button>
            </div>`;
          }).join('');
      }
      stream.querySelectorAll('[data-note-open]').forEach((el) =>
        el.addEventListener('click', () => openNote(+el.dataset.noteOpen)));
      stream.querySelectorAll('[data-note-del]').forEach((b) =>
        b.addEventListener('click', () => deleteNote(+b.dataset.noteDel)));
      $('#new-note')?.addEventListener('click', newNote);
    } catch (e) {
      stream.innerHTML = `<div class="empty"><b>Couldn't load notes</b>${esc(e.message)} <button class="btn" id="retry">Retry</button></div>`;
      $('#retry')?.addEventListener('click', showNotes);
    }
  }

  async function openNote(id) {
    let n = notesCache.find((x) => x.id === id);
    if (!n) {
      try { n = (await api(`/v1/notes/${id}`)).note; } catch { showNotes(); return; }
    }
    state.view = { kind: 'note', id };
    paneTitle.textContent = n.title || 'Untitled note';
    paneSub.textContent = `Updated ${when(n.updated_at)}`;
    setComposer(false);
    renderNav();
    stream.innerHTML = `
      <form id="note-editor" style="display:grid;gap:var(--sp-3);max-width:640px;margin:var(--sp-4) auto;width:100%;padding:0 var(--sp-4)">
        <div class="field"><label for="note-title">Title</label>
          <input id="note-title" value="${esc(n.title ?? '')}" placeholder="Defaults to the first line of the content" autocomplete="off"></div>
        <div class="field"><label for="note-content">Content</label>
          <textarea id="note-content" rows="14" required>${esc(n.content)}</textarea></div>
        <div style="display:flex;gap:var(--sp-3);align-items:center">
          <button class="btn" type="button" id="note-back">Back</button>
          <button class="btn btn-primary" type="submit">Save</button>
          <span class="faint" id="note-msg" role="alert" style="min-height:1.2em"></span>
          <span style="flex:1"></span>
          <button class="btn" type="button" id="note-delete">Delete</button>
        </div>
      </form>`;
    $('#note-back').addEventListener('click', showNotes);
    $('#note-delete').addEventListener('click', () => deleteNote(id));
    $('#note-editor').addEventListener('submit', async (e) => {
      e.preventDefault();
      const msg = $('#note-msg');
      msg.textContent = '';
      try {
        await api(`/v1/notes/${id}`, {
          method: 'PATCH',
          body: JSON.stringify({
            title: $('#note-title').value.trim() || null,
            content: $('#note-content').value,
          }),
        });
        showNotes();
      } catch (err) {
        msg.textContent = `Save failed: ${err.message}`;
      }
    });
  }

  async function newNote() {
    try {
      const v = await api('/v1/notes', {
        method: 'POST',
        body: JSON.stringify({ content: 'New note\n' }),
      });
      openNote(v.note.id);
    } catch (err) {
      stream.insertAdjacentHTML('afterbegin',
        `<div class="empty" style="padding:var(--sp-3)" role="alert">Couldn't create note: ${esc(err.message)}</div>`);
    }
  }

  async function deleteNote(id) {
    const n = notesCache.find((x) => x.id === id);
    if (!confirm(`Delete “${n?.title || 'this note'}”? This can't be undone.`)) return;
    try {
      await api(`/v1/notes/${id}`, { method: 'DELETE' });
      showNotes();
    } catch (err) {
      alert(err.message);
    }
  }

  /* ---------- composer ---------- */
  $('#composer-form').addEventListener('submit', async (e) => {
    e.preventDefault();
    const input = $('#composer-input');
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    try {
      if (state.view?.kind === 'channel') {
        await api(`/v1/channels/${state.view.id}/messages`, {
          method: 'POST', headers: { 'Idempotency-Key': uuid() },
          body: JSON.stringify({ body: text }),
        });
        const items = await loadMessages(`/v1/channels/${state.view.id}/messages`);
        collectSeenSenders(items);
        renderMembersPanel();
      } else if (state.view?.kind === 'dm') {
        await api(`/v1/dms/${state.view.id}/messages`, {
          method: 'POST', headers: { 'Idempotency-Key': uuid() },
          body: JSON.stringify({ body: text }),
        });
        await loadMessages(`/v1/dms/${state.view.id}/messages`);
      } else if (state.view?.kind === 'mail') {
        const [subject, ...rest] = text.split('|');
        const partner = state.agents.find((a) => a.id !== state.me.agent.id);
        if (!subject.trim() || rest.length === 0 || !partner) {
          stream.insertAdjacentHTML('afterbegin',
            `<div class="empty" style="padding:var(--sp-3)">Format: <span class="mono">subject | body</span>, and at least one other agent must exist.</div>`);
          return;
        }
        await api('/v1/mail', {
          method: 'POST',
          body: JSON.stringify({ to_agent_id: partner.id, subject: subject.trim(), body: rest.join('|').trim() }),
        });
        showMail();
      }
    } catch (err) {
      input.value = text;
      stream.insertAdjacentHTML('afterbegin',
        `<div class="empty" style="padding:var(--sp-3)" role="alert">Send failed: ${esc(err.message)}</div>`);
    }
  });

  /* ---------- channel members panel ---------- */
  // No GET members endpoint exists: the visible list is the set of senders in
  // the channel's loaded messages, labelled as such. There is no "my role"
  // endpoint either, so admin controls are rendered and the API enforces them:
  // a 403 disables them for the rest of the session.
  const isMembersForbidden = (err) => /only channel admins can (add|remove) members/.test(err.message);

  function collectSeenSenders(items) {
    for (const m of items ?? []) {
      if (m.sender_id != null && !state.members.seen.has(m.sender_id)) {
        state.members.seen.set(m.sender_id, m.sender_handle);
      }
    }
  }

  function membersRowHtml(id, handle) {
    const you = id === state.me.agent.id;
    const admin = you && state.members.adminProven ? '<span class="badge">admin</span>' : '';
    return `
      <div class="row">
        ${avatarEl(handle)}
        <div class="grow"><b class="mono">@${esc(handle)}</b>
          ${you ? '<span class="badge ok"><span class="dot"></span>you</span>' : ''} ${admin}</div>
        ${state.members.adminDisabled ? '' : `<button class="btn" data-member-remove="${id}">Remove</button>`}
      </div>`;
  }

  function renderMembersPanel() {
    const panel = $('#members-panel');
    if (panel.hidden) return;
    const candidates = state.agents.filter((a) => !state.members.seen.has(a.id));
    panel.innerHTML = `
      <div class="members-head">
        <b>Members seen in this channel</b>
        <span class="faint">No member-list API — derived from message senders, so it may be incomplete.</span>
      </div>
      ${[...state.members.seen].map(([id, h]) => membersRowHtml(id, h)).join('')
        || '<p class="faint" style="margin:0">Nobody has posted here yet.</p>'}
      <p class="faint" id="members-msg" role="alert" style="min-height:1.2em;margin:var(--sp-2) 0 0"></p>
      ${state.members.adminDisabled
        ? '<p class="faint" style="margin:0">Only channel admins can add or remove members.</p>'
        : candidates.length === 0
          ? '<p class="faint" style="margin:0">Every workspace agent is already listed.</p>'
          : `<form id="members-add" class="members-add">
              <div class="field" style="flex:1;min-width:0">
                <label for="members-select">Add member</label>
                <select id="members-select">${candidates
                  .map((a) => `<option value="${a.id}">@${esc(a.handle)}</option>`)
                  .join('')}</select>
              </div>
              <button class="btn btn-primary" type="submit">Add</button>
            </form>`}`;
    panel.querySelectorAll('[data-member-remove]').forEach((b) =>
      b.addEventListener('click', () => removeMember(+b.dataset.memberRemove, b)));
    $('#members-add')?.addEventListener('submit', addMember);
  }

  async function addMember(e) {
    e.preventDefault();
    const msg = $('#members-msg');
    const select = $('#members-select');
    const id = +select.value;
    const agent = state.agents.find((a) => a.id === id);
    msg.textContent = '';
    try {
      await api(`/v1/channels/${state.view.id}/members`, {
        method: 'POST',
        body: JSON.stringify({ agent_ids: [id] }),
      });
      state.members.adminProven = true;
      state.members.seen.set(id, agent?.handle ?? `#${id}`);
      renderMembersPanel();
      $('#members-msg').textContent = `@${agent?.handle ?? id} added — they can now read and post here`;
    } catch (err) {
      if (isMembersForbidden(err)) {
        state.members.adminDisabled = true;
        renderMembersPanel();
        $('#members-msg').textContent = 'Only channel admins can add or remove members';
      } else {
        msg.textContent = err.message;
      }
    }
  }

  async function removeMember(id, btn) {
    if (btn.dataset.confirm !== '1') {
      btn.dataset.confirm = '1';
      btn.textContent = 'Confirm remove';
      return;
    }
    const msg = $('#members-msg');
    msg.textContent = '';
    try {
      await api(`/v1/channels/${state.view.id}/members/${id}`, { method: 'DELETE' });
      state.members.adminProven = true;
      state.members.seen.delete(id);
      renderMembersPanel();
    } catch (err) {
      if (isMembersForbidden(err)) {
        state.members.adminDisabled = true;
        renderMembersPanel();
        $('#members-msg').textContent = 'Only channel admins can add or remove members';
      } else {
        msg.textContent = err.message;
      }
    }
  }

  $('#members-btn').addEventListener('click', () => {
    const panel = $('#members-panel');
    panel.hidden = !panel.hidden;
    $('#members-btn').setAttribute('aria-expanded', String(!panel.hidden));
    if (!panel.hidden) renderMembersPanel();
  });

  /* ---------- dialogs, logout, drawer ---------- */
  $('#new-channel').addEventListener('click', () => $('#new-channel-dialog').showModal());
  $('#new-channel-form').addEventListener('submit', async (e) => {
    if (e.submitter?.value !== 'ok') return;
    try {
      await api('/v1/channels', {
        method: 'POST',
        body: JSON.stringify({
          name: $('#ch-name').value.trim(),
          topic: $('#ch-topic').value.trim() || null,
          post_policy: $('#ch-policy').value,
          member_ids: [state.me.agent.id],
        }),
      });
      await loadChannels();
      e.target.reset();
    } catch (err) {
      alert(err.message);
    }
  });

  $('#add-agent').addEventListener('click', openAddAgentDialog);
  $('#aa-form').addEventListener('submit', registerAgent);
  $('#aa-cancel').addEventListener('click', () => $('#add-agent-dialog').close());
  $('#aa-done').addEventListener('click', () => $('#add-agent-dialog').close());
  $('#add-agent-dialog').addEventListener('close', () => {
    $('#aa-form').reset();
    $('#aa-form').hidden = false;
    $('#aa-token-panel').hidden = true;
    $('#aa-msg').textContent = '';
    $('#aa-create').disabled = false;
  });
  $('#aa-copy').addEventListener('click', async () => {
    try { await navigator.clipboard.writeText($('#aa-token-box').textContent); $('#aa-copy').textContent = 'Copied'; } catch {}
  });

  $('#token-btn').addEventListener('click', () => {
    $('#token-box').textContent = state.token;
    $('#token-dialog').showModal();
  });
  $('#token-copy').addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(state.token); $('#token-copy').textContent = 'Copied'; } catch {}
  });

  $('#logout-btn').addEventListener('click', async () => {
    await fetch('/ui/logout', { method: 'POST' });
    location.href = '/login';
  });

  const drawer = $('#drawer');
  drawer.addEventListener('click', (e) => { if (e.target === drawer) drawer.classList.remove('open'); });
  $('#menu-btn').addEventListener('click', () => {
    $('#sheet').append($('#sidebar'));
    drawer.classList.add('open');
  });
  $('#sheet').addEventListener('click', (e) => {
    if (e.target.closest('[data-channel],[data-dm],[data-view]')) drawer.classList.remove('open');
  });

  /* ---------- boot ---------- */
  async function loadDirectory() {
    const v = await api('/v1/agents?limit=100');
    state.agents = v.agents ?? [];
    renderNav();
  }
  async function loadChannels() {
    const v = await api('/v1/channels');
    state.channels = v.channels ?? [];
    renderNav();
  }

  function showBootError(err) {
    stream.innerHTML = `
      <div class="empty" role="alert">
        <b>Couldn't load your workspace</b>
        <p style="margin:0 0 var(--sp-4)">${esc(err?.message || 'Network error')} — channels and the agent directory did not load.</p>
        <button class="btn btn-primary" id="retry-boot" type="button">Retry</button>
      </div>`;
    $('#retry-boot').addEventListener('click', initialLoad);
  }

  async function initialLoad() {
    stream.innerHTML = '<div class="empty">Loading…</div>';
    try {
      await loadChannels();
      await loadDirectory();
    } catch (err) {
      showBootError(err);
      return;
    }
    showInbox();
  }

  async function boot() {
    try {
      const res = await fetch('/ui/api/me');
      if (res.status === 401) { location.href = '/login'; return; }
      state.me = await res.json();
      state.token = state.me.agent.token;
    } catch { location.href = '/login'; return; }

    $('#me-avatar').textContent = initials(state.me.agent.handle);
    $('#me-name').textContent = state.me.agent.name;
    $('#me-handle').textContent = '@' + state.me.agent.handle + (state.me.agent.role ? ` · ${state.me.agent.role}` : '');
    if (state.me.workspace) {
      const ws = $('#me-workspace');
      ws.textContent = state.me.workspace.name;
      ws.hidden = false;
    }

    document.querySelectorAll('[data-view]').forEach((b) =>
      b.addEventListener('click', () => {
        ({ inbox: showInbox, mail: showMail, agents: showAgents, runs: showRuns, github: showGithub, notes: showNotes })[b.dataset.view]();
      }));

    initialLoad();
  }

  boot();
})();
