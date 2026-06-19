/* global window */

const { invoke } = window.__TAURI__.core;
const { open: openDialog, ask } = window.__TAURI__.dialog;
const { listen } = window.__TAURI__.event;

// ── State ─────────────────────────────────────────────────────────────────────

let selectedFolder = '';     // chosen via "Select Folder…"
let selectedFiles = [];      // chosen via "Select Files…"
let resolvedFiles = [];      // flat file list resolved by the backend

let queue = [];              // queued items (see makeQueueItem)
let nextItemId = 1;
let processing = false;      // queue is actively uploading
let stopRequested = false;   // stop after the current item finishes

let loggedIn = false;        // credentials verified via ia configure
let loggedInUser = '';       // username/email of the signed-in account

// ── DOM refs ──────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);

const usernameInput  = $('username');
const passwordInput  = $('password');
const loginStatus    = $('login-status');
const loginBtn       = $('login-btn');
const logoutBtn      = $('logout-btn');
const accountForm    = $('account-form');
const accountSignedin = $('account-signedin');
const accountWho     = $('account-who');

const pickFolderBtn  = $('pick-folder-btn');
const pickFilesBtn   = $('pick-files-btn');
const clearSourceBtn = $('clear-source-btn');
const sourceCount    = $('source-count');
const sourceList     = $('source-list');

const identifierInput = $('identifier');
const deriveIdBtn     = $('derive-id-btn');
const checkIdBtn      = $('check-id-btn');
const idStatus        = $('id-status');
const titleInput      = $('title');
const descriptionInput = $('description');
const subjectsInput   = $('subjects');
const mediatypeSelect = $('mediatype');

const optionalToggle = $('optional-toggle');
const optionalBody   = $('optional-body');
const creatorInput    = $('creator');
const collectionInput = $('collection');
const dateInput       = $('date');
const languageInput   = $('language');
const licenseSelect   = $('license');

const addQueueBtn   = $('add-queue-btn');
const queueCount    = $('queue-count');
const queueEmpty    = $('queue-empty');
const queueListEl   = $('queue-list');
const queueActions  = $('queue-actions');
const startQueueBtn = $('start-queue-btn');
const stopQueueBtn  = $('stop-queue-btn');
const clearDoneBtn  = $('clear-done-btn');
const clearQueueBtn = $('clear-queue-btn');

const clearLogBtn = $('clear-log-btn');
const logEl       = $('log');

// ── Log helpers ───────────────────────────────────────────────────────────────

function logLine(text, cls = 'line-info') {
  const el = document.createElement('div');
  el.className = cls;
  el.textContent = text;
  logEl.appendChild(el);
  logEl.scrollTop = logEl.scrollHeight;
}
const logHeading = (t) => logLine(t, 'line-heading');
const logOk      = (t) => logLine(t, 'line-ok');
const logWarn    = (t) => logLine(t, 'line-warn');
const logError   = (t) => logLine(t, 'line-error');
const logDim     = (t) => logLine(t, 'line-dim');
const logSep     = ()  => logDim('─'.repeat(54));

// ── Tauri log events ──────────────────────────────────────────────────────────

let progressEl = null; // reused element for live, in-place upload progress

listen('log', (event) => {
  const msg = event.payload;
  progressEl = null; // a committed line finalizes any in-place progress line
  if (msg.startsWith('Upload complete'))  logOk(msg);
  else if (/error|fail/i.test(msg))       logWarn(msg);
  else                                    logLine(msg);
});

listen('upload-progress', (event) => {
  if (!progressEl) {
    progressEl = document.createElement('div');
    progressEl.className = 'line-dim';
    logEl.appendChild(progressEl);
  }
  progressEl.textContent = event.payload;
  logEl.scrollTop = logEl.scrollHeight;
});

// ── Misc helpers ──────────────────────────────────────────────────────────────

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let n = bytes / 1024, i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

// ── Source selection ──────────────────────────────────────────────────────────

async function refreshSources() {
  try {
    resolvedFiles = await invoke('collect_sources', {
      files: selectedFiles,
      folder: selectedFolder || null,
    });
  } catch (err) {
    resolvedFiles = [];
    logError(`Could not read source: ${err}`);
  }

  if (resolvedFiles.length === 0) {
    sourceCount.textContent = 'no files';
    sourceCount.className = 'badge badge-dim';
    sourceList.classList.add('hidden');
    sourceList.innerHTML = '';
  } else {
    const total = resolvedFiles.reduce((s, f) => s + f.size, 0);
    sourceCount.textContent = `${resolvedFiles.length} file${resolvedFiles.length !== 1 ? 's' : ''} · ${formatSize(total)}`;
    sourceCount.className = 'badge badge-info';
    sourceList.innerHTML = resolvedFiles
      .map((f) => `<div class="src-row"><span class="src-name">${escapeHtml(f.name)}</span><span class="src-size">${formatSize(f.size)}</span></div>`)
      .join('');
    sourceList.classList.remove('hidden');
  }
}

pickFolderBtn.addEventListener('click', async () => {
  const selected = await openDialog({ directory: true, multiple: false, title: 'Select a folder to upload' });
  if (!selected) return;
  selectedFolder = selected;
  selectedFiles = [];
  await refreshSources();
});

pickFilesBtn.addEventListener('click', async () => {
  const selected = await openDialog({ directory: false, multiple: true, title: 'Select file(s) to upload' });
  if (!selected) return;
  selectedFiles = Array.isArray(selected) ? selected : [selected];
  selectedFolder = '';
  await refreshSources();
});

clearSourceBtn.addEventListener('click', () => {
  selectedFolder = '';
  selectedFiles = [];
  resolvedFiles = [];
  refreshSources();
});

// ── Identifier helpers ────────────────────────────────────────────────────────

function sanitizeIdentifier(s) {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^[-.]+|[-.]+$/g, '')
    .slice(0, 100);
}

function setIdStatus(text, cls) {
  idStatus.textContent = text;
  idStatus.className = `hint ${cls}`;
  idStatus.classList.remove('hidden');
}

identifierInput.addEventListener('input', () => idStatus.classList.add('hidden'));
identifierInput.addEventListener('blur', () => {
  if (identifierInput.value) identifierInput.value = sanitizeIdentifier(identifierInput.value);
});

deriveIdBtn.addEventListener('click', () => {
  const id = sanitizeIdentifier(titleInput.value.trim());
  if (id) { identifierInput.value = id; idStatus.classList.add('hidden'); }
  else logWarn('Enter a title first to derive an identifier.');
});

checkIdBtn.addEventListener('click', async () => {
  const id = sanitizeIdentifier(identifierInput.value.trim());
  if (!id) { setIdStatus('Enter an identifier first.', 'id-taken'); return; }
  identifierInput.value = id;
  setIdStatus('Checking availability…', 'id-busy');
  checkIdBtn.disabled = true;
  try {
    const res = await invoke('check_identifier', { identifier: id });
    setIdStatus(res.message, res.available ? 'id-ok' : 'id-taken');
  } catch (err) {
    setIdStatus(`Check failed: ${err}`, 'id-taken');
  } finally {
    checkIdBtn.disabled = false;
  }
});

// ── Optional section collapse ─────────────────────────────────────────────────

optionalToggle.addEventListener('click', () => {
  const open = optionalBody.classList.toggle('hidden') === false;
  optionalToggle.setAttribute('aria-expanded', String(open));
});

// ── Queue ─────────────────────────────────────────────────────────────────────

const STATUS_LABEL = {
  pending:   'Queued',
  uploading: 'Uploading…',
  done:      'Done',
  failed:    'Failed',
  cancelled: 'Cancelled',
};

function renderQueue() {
  if (queue.length === 0) {
    queueCount.classList.add('hidden');
    queueEmpty.classList.remove('hidden');
    queueListEl.classList.add('hidden');
    queueActions.style.display = 'none';
    queueListEl.innerHTML = '';
    return;
  }

  const pending = queue.filter((i) => i.status === 'pending').length;
  const done = queue.filter((i) => i.status === 'done').length;
  queueCount.textContent = `${queue.length} item${queue.length !== 1 ? 's' : ''} · ${done} done`;
  queueCount.className = 'badge badge-info';
  queueCount.classList.remove('hidden');

  queueEmpty.classList.add('hidden');
  queueListEl.classList.remove('hidden');
  queueActions.style.display = 'flex';

  queueListEl.innerHTML = queue.map((item) => {
    // An uploading item gets a Cancel control; all others get Remove (✕).
    const actionBtn = item.status === 'uploading'
      ? `<button class="q-cancel" data-id="${item.id}" title="Cancel this upload">Cancel</button>`
      : `<button class="q-remove" data-id="${item.id}" title="Remove from queue">✕</button>`;
    return `
    <div class="queue-item is-${item.status}">
      <div class="q-main">
        <div class="q-title">${escapeHtml(item.meta.title)}</div>
        <div class="q-sub">${escapeHtml(item.meta.identifier)} · ${item.fileCount} file${item.fileCount !== 1 ? 's' : ''} · ${formatSize(item.totalSize)}${item.error ? ' · ' + escapeHtml(item.error) : ''}</div>
      </div>
      <span class="q-status">${STATUS_LABEL[item.status]}</span>
      ${actionBtn}
    </div>
  `;
  }).join('');

  queueListEl.querySelectorAll('.q-remove').forEach((btn) => {
    btn.addEventListener('click', () => removeFromQueue(Number(btn.dataset.id)));
  });
  queueListEl.querySelectorAll('.q-cancel').forEach((btn) => {
    btn.addEventListener('click', () => cancelUpload(Number(btn.dataset.id)));
  });

  // Start button enabled only when there's pending work and we're idle.
  startQueueBtn.disabled = processing || pending === 0;
  startQueueBtn.textContent = pending > 0 ? `Start Upload (${pending})` : 'Start Upload';
  stopQueueBtn.style.display = processing ? 'inline-flex' : 'none';
  startQueueBtn.style.display = processing ? 'none' : 'inline-flex';
}

function removeFromQueue(id) {
  const item = queue.find((i) => i.id === id);
  if (!item || item.status === 'uploading') return;
  queue = queue.filter((i) => i.id !== id);
  renderQueue();
}

// Cancel the in-flight upload: flag the item and kill the `ia` process. The
// upload loop's catch block then marks it 'cancelled' and moves on.
async function cancelUpload(id) {
  const item = queue.find((i) => i.id === id);
  if (!item || item.status !== 'uploading') return;
  item.cancelRequested = true;
  logWarn(`Cancelling '${item.meta.identifier}'…`);
  try {
    await invoke('cancel_upload');
  } catch (err) {
    logWarn(String(err));
  }
}

function validateItem() {
  const missing = [];
  if (!identifierInput.value.trim())  missing.push('Identifier');
  if (!titleInput.value.trim())       missing.push('Title');
  if (!descriptionInput.value.trim()) missing.push('Description');
  if (!subjectsInput.value.trim())    missing.push('Topics');
  if (!mediatypeSelect.value)         missing.push('Media Type');
  if (resolvedFiles.length === 0)     missing.push('at least one file');
  if (missing.length) {
    logError(`Missing required field(s): ${missing.join(', ')}.`);
    return false;
  }
  return true;
}

// Clear the per-item fields after queuing, keeping account + "sticky" metadata
// (collection, license, language, creator) for the next item. Media Type is
// deliberately reset so the user must consciously pick it for every item.
function resetItemForm() {
  identifierInput.value = '';
  titleInput.value = '';
  descriptionInput.value = '';
  subjectsInput.value = '';
  mediatypeSelect.value = '';
  idStatus.classList.add('hidden');
  selectedFolder = '';
  selectedFiles = [];
  resolvedFiles = [];
  refreshSources();
}

addQueueBtn.addEventListener('click', () => {
  if (!validateItem()) return;

  const id = sanitizeIdentifier(identifierInput.value.trim());
  if (queue.some((i) => i.meta.identifier === id && i.status !== 'failed')) {
    logWarn(`An item with identifier '${id}' is already in the queue.`);
  }

  const meta = {
    identifier: id,
    title: titleInput.value.trim(),
    description: descriptionInput.value.trim(),
    mediatype: mediatypeSelect.value,
    subjects: subjectsInput.value.split(',').map((s) => s.trim()).filter(Boolean),
    creator: creatorInput.value.trim(),
    collection: collectionInput.value.trim(),
    date: dateInput.value.trim(),
    licenseUrl: licenseSelect.value,
    language: languageInput.value.trim(),
  };

  queue.push({
    id: nextItemId++,
    meta,
    files: resolvedFiles.map((f) => f.path),
    fileCount: resolvedFiles.length,
    totalSize: resolvedFiles.reduce((s, f) => s + f.size, 0),
    status: 'pending',
    error: '',
  });

  logOk(`Added '${meta.identifier}' to the queue (${queue.length} item${queue.length !== 1 ? 's' : ''}).`);
  resetItemForm();
  renderQueue();
});

clearDoneBtn.addEventListener('click', () => {
  // Drop all finished items (done / failed / cancelled); keep active ones.
  queue = queue.filter((i) => i.status === 'pending' || i.status === 'uploading');
  renderQueue();
});

clearQueueBtn.addEventListener('click', () => {
  if (processing) return;
  queue = [];
  renderQueue();
});

stopQueueBtn.addEventListener('click', () => {
  stopRequested = true;
  stopQueueBtn.disabled = true;
  logWarn('Stop requested — will halt after the current item finishes.');
});

function setAccountStatus(text, cls) {
  loginStatus.textContent = text;
  loginStatus.className = `badge ${cls}`;
}

// ── Login / logout ────────────────────────────────────────────────────────────

function applyLoggedInUI() {
  if (loggedIn) {
    accountForm.classList.add('hidden');
    accountSignedin.classList.remove('hidden');
    accountWho.textContent = `Signed in as ${loggedInUser}`;
    setAccountStatus('signed in', 'badge-ok');
  } else {
    accountForm.classList.remove('hidden');
    accountSignedin.classList.add('hidden');
    setAccountStatus('not signed in', 'badge-dim');
  }
}

loginBtn.addEventListener('click', async () => {
  const user = usernameInput.value.trim();
  if (!user || !passwordInput.value.trim()) {
    logError('Enter your archive.org username and password.');
    return;
  }
  loginBtn.disabled = true;
  loginBtn.textContent = 'Signing in…';
  setAccountStatus('signing in…', 'badge-info');
  try {
    await invoke('configure_account', { username: user, password: passwordInput.value });
    loggedIn = true;
    loggedInUser = user;
    // Remember the working credentials so the fields are pre-filled next launch.
    localStorage.setItem('account.username', user);
    localStorage.setItem('account.password', passwordInput.value);
    applyLoggedInUI();        // collapses the section
    logOk('Logged in.');
  } catch (err) {
    loggedIn = false;
    setAccountStatus('sign-in failed', 'badge-error');
    logError(String(err));
  } finally {
    loginBtn.disabled = false;
    loginBtn.textContent = 'Log In';
  }
});

logoutBtn.addEventListener('click', () => {
  if (processing) { logWarn('Cannot log out while the queue is uploading.'); return; }
  loggedIn = false;
  loggedInUser = '';
  passwordInput.value = '';
  // Explicit sign-out forgets the saved credentials.
  localStorage.removeItem('account.username');
  localStorage.removeItem('account.password');
  applyLoggedInUI();          // re-expands the section
  logDim('Signed out.');
});

// If the identifier already exists on archive.org, only proceed silently when the
// signed-in account owns it (legitimately adding files to a pre-existing item);
// otherwise ask the user to confirm. Returns true to upload, false to skip.
async function confirmExistingItem(item, me) {
  let info;
  try {
    info = await invoke('inspect_item', { identifier: item.meta.identifier });
  } catch (err) {
    logWarn(`Could not verify '${item.meta.identifier}' on archive.org (${err}) — proceeding.`);
    return true; // don't block uploads on a failed ownership probe
  }
  if (!info.exists) return true; // brand-new identifier

  const owner = (info.uploader || '').trim().toLowerCase();
  const mine = (me || '').trim().toLowerCase();
  if (owner && mine && owner === mine) {
    logDim(`'${item.meta.identifier}' already exists — you (${info.uploader}) own it; adding files to it.`);
    return true;
  }

  const ownerTxt = info.uploader ? `created by ${info.uploader}` : 'owner not disclosed';
  const titleTxt = info.title ? `\nTitle: ${info.title}` : '';
  logWarn(`'${item.meta.identifier}' already exists on archive.org (${ownerTxt}); you are signed in as ${me || '(none)'}.`);
  return ask(
    `The identifier "${item.meta.identifier}" already exists on archive.org (${ownerTxt}).${titleTxt}\n\n` +
    `You are signed in as ${me || '(no account)'}. Files can only be added to an item your own account created — ` +
    `uploading to someone else's item will be rejected.\n\n` +
    `Add these files to the existing item anyway?`,
    { title: 'Identifier already exists', kind: 'warning', okLabel: 'Upload anyway', cancelLabel: 'Skip item' }
  );
}

startQueueBtn.addEventListener('click', async () => {
  if (processing) return;
  if (!queue.some((i) => i.status === 'pending')) return;

  if (!loggedIn) {
    logError('Log in to your archive.org account before uploading.');
    return;
  }

  processing = true;
  stopRequested = false;
  stopQueueBtn.disabled = false;
  renderQueue();

  const me = loggedInUser;
  let uploaded = 0, failed = 0, skipped = 0, cancelled = 0;
  // Re-query each pass (rather than for…of) so items added — or removed —
  // while the queue is running are reliably picked up.
  while (true) {
    if (stopRequested) { logWarn('Queue stopped.'); break; }
    const item = queue.find((i) => i.status === 'pending');
    if (!item) break;

    // Guard pre-existing identifiers: confirm the signed-in account owns the item.
    if (!await confirmExistingItem(item, me)) {
      item.status = 'failed';
      item.error = 'skipped — existing item not owned by this account';
      skipped++;
      logWarn(`'${item.meta.identifier}' skipped — not uploaded.`);
      renderQueue();
      continue;
    }

    item.status = 'uploading';
    item.error = '';
    renderQueue();

    logSep();
    logHeading(`Uploading '${item.meta.identifier}'…`);
    try {
      await invoke('upload_to_archive', { meta: item.meta, files: item.files });
      item.status = 'done';
      uploaded++;
    } catch (err) {
      if (item.cancelRequested || String(err) === 'cancelled') {
        item.status = 'cancelled';
        item.error = 'upload cancelled';
        cancelled++;
        logWarn(`'${item.meta.identifier}' cancelled.`);
      } else {
        item.status = 'failed';
        item.error = String(err);
        failed++;
        logError(`'${item.meta.identifier}' failed: ${err}`);
      }
    }
    item.cancelRequested = false;
    renderQueue();
  }

  processing = false;
  stopRequested = false;
  renderQueue();

  logSep();
  logOk(`Queue finished — ${uploaded} uploaded, ${failed} failed, ${skipped} skipped, ${cancelled} cancelled.`);
});

clearLogBtn.addEventListener('click', () => { logEl.innerHTML = ''; });

// ── Media Type — never persisted; always starts unset so the user must pick
// it deliberately for each item (and each launch). ────────────────────────────
localStorage.removeItem('mediatype');

// ── Account — pre-fill the last working credentials for quick re-login ────────
const savedUser = localStorage.getItem('account.username');
const savedPass = localStorage.getItem('account.password');
if (savedUser) usernameInput.value = savedUser;
if (savedPass) passwordInput.value = savedPass;

applyLoggedInUI();
renderQueue();
