// ── Tab Navigation ─────────────────────────────────────
const tabBtns = document.querySelectorAll('.tab-btn');
const tabContents = document.querySelectorAll('.tab-content');

tabBtns.forEach(btn => {
  btn.addEventListener('click', () => {
    tabBtns.forEach(b => b.classList.remove('active'));
    tabContents.forEach(c => c.classList.remove('active'));
    btn.classList.add('active');
    document.getElementById('tab-' + btn.dataset.tab).classList.add('active');
    if (btn.dataset.tab === 'files' && !filesLoaded) {
      loadFiles('.');
    }
  });
});

// ── Chat ───────────────────────────────────────────────
const messagesEl = document.getElementById('messages');
const composerEl = document.getElementById('composer');
const inputEl    = document.getElementById('messageInput');

marked.setOptions({ breaks: true, gfm: true });

function renderMarkdown(text) {
  try { return marked.parse(text); }
  catch {
    const d = document.createElement('div');
    d.textContent = text;
    return d.innerHTML;
  }
}

let lastMessageCount = 0;
let lastTypingState  = false;
let lastStreamText   = '';
let userIsScrolledUp = false;

messagesEl.addEventListener('scroll', () => {
  const atBottom = messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 40;
  userIsScrolledUp = !atBottom;
});

function scrollToBottomIfNeeded() {
  if (!userIsScrolledUp) messagesEl.scrollTop = messagesEl.scrollHeight;
}

function createBubble(message) {
  const bubble = document.createElement('div');
  const src = message.source === 'you' ? 'you'
            : message.source === 'assistant' ? 'assistant' : 'system';
  bubble.className = 'bubble ' + src;
  if (src === 'assistant') {
    bubble.innerHTML = renderMarkdown(message.text);
    bubble.querySelectorAll('a').forEach(a => {
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
    });
  } else {
    bubble.textContent = message.text;
  }
  return bubble;
}

function createTypingBubble() {
  const b = document.createElement('div');
  b.className = 'bubble system typing';
  b.id = 'typing-indicator';
  b.innerHTML = '<span class="dot-pulse"></span> Denkt nach\u2026';
  return b;
}

function removeEl(id) { document.getElementById(id)?.remove(); }

async function refreshMessages() {
  try {
    const res = await fetch('/api/messages', { credentials: 'same-origin' });
    if (!res.ok) return;
    const data = await res.json();
    const messages = data.messages || [];
    let changed = false;

    if (messages.length !== lastMessageCount) {
      removeEl('typing-indicator');
      removeEl('stream-bubble');
      for (const msg of messages.slice(lastMessageCount)) {
        messagesEl.appendChild(createBubble(msg));
      }
      lastMessageCount = messages.length;
      changed = true;
    }

    const streamText = data.assistant_stream_text || '';
    if (streamText !== lastStreamText) {
      removeEl('stream-bubble');
      if (streamText) {
        const sb = document.createElement('div');
        sb.className = 'bubble assistant streaming';
        sb.id = 'stream-bubble';
        sb.innerHTML = renderMarkdown(streamText);
        sb.querySelectorAll('a').forEach(a => {
          a.target = '_blank'; a.rel = 'noopener noreferrer';
        });
        messagesEl.appendChild(sb);
      }
      lastStreamText = streamText;
      changed = true;
    }

    const showTyping = data.assistant_typing && !streamText;
    if (showTyping !== lastTypingState) {
      removeEl('typing-indicator');
      if (showTyping) messagesEl.appendChild(createTypingBubble());
      lastTypingState = showTyping;
      changed = true;
    }

    if (changed) scrollToBottomIfNeeded();
  } catch { /* network error — ignore */ }
}

composerEl.addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = inputEl.value.trim();
  if (!text) return;
  const res = await fetch('/api/messages', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ text }),
    credentials: 'same-origin',
  });
  if (res.ok) {
    inputEl.value = '';
    messagesEl.appendChild(createBubble({ text, source: 'you' }));
    lastMessageCount++;
    userIsScrolledUp = false;
    scrollToBottomIfNeeded();
  }
});

setInterval(refreshMessages, 400);
refreshMessages();

// ── Files ──────────────────────────────────────────────
let currentPath = '.';
let filesLoaded = false;

const fileList       = document.getElementById('file-list');
const fileBrowser    = document.getElementById('file-browser');
const fileEditor     = document.getElementById('file-editor');
const breadcrumbEl   = document.getElementById('breadcrumb');
const editorFilename = document.getElementById('editor-filename');
const editorContent  = document.getElementById('editor-content');

function escapeHtml(text) {
  const d = document.createElement('div');
  d.textContent = text;
  return d.innerHTML;
}

function showToast(msg, type) {
  type = type || 'success';
  const t = document.createElement('div');
  t.className = 'toast ' + type;
  t.textContent = msg;
  document.body.appendChild(t);
  setTimeout(() => t.remove(), 3000);
}

function updateBreadcrumb(path) {
  breadcrumbEl.innerHTML = '';
  const parts = path === '.' ? ['.'] : ['.'].concat(path.split('/').filter(Boolean));
  parts.forEach((part, i) => {
    if (i > 0) {
      const sep = document.createElement('span');
      sep.className = 'crumb-sep';
      sep.textContent = ' / ';
      breadcrumbEl.appendChild(sep);
    }
    const crumb = document.createElement('span');
    crumb.className = 'crumb';
    crumb.textContent = i === 0 ? 'workspace' : part;
    const crumbPath = i === 0 ? '.' : parts.slice(1, i + 1).join('/');
    crumb.addEventListener('click', () => loadFiles(crumbPath));
    breadcrumbEl.appendChild(crumb);
  });
}

async function loadFiles(path) {
  currentPath = path;
  filesLoaded = true;
  updateBreadcrumb(path);
  fileList.innerHTML = '<div class="empty-state">Lade\u2026</div>';
  fileBrowser.style.display = '';
  fileEditor.classList.add('hidden');

  try {
    const res = await fetch('/api/files/list?path=' + encodeURIComponent(path), {
      credentials: 'same-origin',
    });
    if (!res.ok) throw new Error('Fehler ' + res.status);
    const result = await res.json();

    if (!result.ok) {
      fileList.innerHTML = '<div class="empty-state">Fehler: '
        + escapeHtml(result.error || 'Unbekannt') + '</div>';
      return;
    }

    const entries = result.entries || [];
    if (entries.length === 0) {
      fileList.innerHTML = '<div class="empty-state">Ordner ist leer</div>';
      return;
    }

    entries.sort((a, b) => {
      if (a.type === 'dir' && b.type !== 'dir') return -1;
      if (a.type !== 'dir' && b.type === 'dir') return 1;
      return a.name.localeCompare(b.name);
    });

    fileList.innerHTML = '';

    if (path !== '.') {
      const parentPath = path.includes('/')
        ? path.split('/').slice(0, -1).join('/') || '.'
        : '.';
      fileList.appendChild(createFileEntry({ name: '..', type: 'dir', size: null }, parentPath));
    }

    for (const entry of entries) {
      const entryPath = path === '.' ? entry.name : path + '/' + entry.name;
      fileList.appendChild(createFileEntry(entry, entryPath));
    }
  } catch (err) {
    fileList.innerHTML = '<div class="empty-state">Fehler: '
      + escapeHtml(err.message) + '</div>';
  }
}

function createFileEntry(entry, entryPath) {
  const el = document.createElement('div');
  el.className = 'file-entry' + (entry.type === 'dir' ? ' directory' : '');

  const icon = document.createElement('span');
  icon.className = 'file-icon';
  icon.textContent = entry.type === 'dir' ? '\uD83D\uDCC1' : getFileIcon(entry.name);

  const name = document.createElement('span');
  name.className = 'file-name';
  name.textContent = entry.name;

  el.appendChild(icon);
  el.appendChild(name);

  if (entry.size != null && entry.type !== 'dir') {
    const size = document.createElement('span');
    size.className = 'file-size';
    size.textContent = formatSize(entry.size);
    el.appendChild(size);
  }

  el.addEventListener('click', () => {
    if (entry.type === 'dir') loadFiles(entryPath);
    else openFile(entryPath);
  });

  return el;
}

function getFileIcon(name) {
  const ext = (name.split('.').pop() || '').toLowerCase();
  const icons = {
    rs: '\uD83E\uDD80', py: '\uD83D\uDC0D', js: '\uD83D\uDCDC', ts: '\uD83D\uDCDC',
    json: '\uD83D\uDCCB', md: '\uD83D\uDCDD', txt: '\uD83D\uDCC4',
    toml: '\u2699\uFE0F', yml: '\u2699\uFE0F', yaml: '\u2699\uFE0F',
    sh: '\uD83D\uDD27', html: '\uD83C\uDF10', css: '\uD83C\uDFA8', sql: '\uD83D\uDDC3\uFE0F',
  };
  return icons[ext] || '\uD83D\uDCC4';
}

function formatSize(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
  return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
}

async function openFile(path) {
  const binExts = [
    'png','jpg','jpeg','gif','webp','bmp','tiff',
    'mp3','wav','mp4','mov',
    'zip','gz','tar','7z','exe','bin','pdf',
  ];
  const ext = (path.split('.').pop() || '').toLowerCase();
  if (binExts.includes(ext)) {
    showToast('Bin\u00e4rdateien k\u00f6nnen nicht bearbeitet werden', 'error');
    return;
  }

  try {
    const res = await fetch('/api/files/read?path=' + encodeURIComponent(path), {
      credentials: 'same-origin',
    });
    if (!res.ok) throw new Error('Fehler ' + res.status);
    const result = await res.json();

    if (!result.ok) {
      showToast('Fehler: ' + (result.error || 'Unbekannt'), 'error');
      return;
    }

    editorFilename.textContent = path;
    editorContent.value = result.content || '';
    fileBrowser.style.display = 'none';
    fileEditor.classList.remove('hidden');
  } catch (err) {
    showToast('Fehler: ' + err.message, 'error');
  }
}

// Save
document.getElementById('btn-save').addEventListener('click', async () => {
  const path = editorFilename.textContent;
  const content = editorContent.value;
  try {
    const res = await fetch('/api/files/write', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path, content }),
      credentials: 'same-origin',
    });
    if (!res.ok) throw new Error('Fehler ' + res.status);
    const result = await res.json();
    if (result.ok) showToast('Gespeichert!');
    else showToast('Fehler: ' + (result.error || 'Unbekannt'), 'error');
  } catch (err) {
    showToast('Fehler: ' + err.message, 'error');
  }
});

// Delete
document.getElementById('btn-delete').addEventListener('click', async () => {
  const path = editorFilename.textContent;
  if (!confirm('Datei "' + path + '" wirklich l\u00f6schen?')) return;
  try {
    const res = await fetch('/api/files/delete', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
      credentials: 'same-origin',
    });
    if (!res.ok) throw new Error('Fehler ' + res.status);
    showToast('Gel\u00f6scht!');
    document.getElementById('btn-back').click();
  } catch (err) {
    showToast('Fehler: ' + err.message, 'error');
  }
});

// Back
document.getElementById('btn-back').addEventListener('click', () => {
  fileEditor.classList.add('hidden');
  fileBrowser.style.display = '';
  loadFiles(currentPath);
});

// Refresh
document.getElementById('btn-refresh').addEventListener('click', () => {
  loadFiles(currentPath);
});

// New File
document.getElementById('btn-new-file').addEventListener('click', () => {
  showModal('Neue Datei', 'Dateiname', async (name) => {
    if (!name) return;
    const path = currentPath === '.' ? name : currentPath + '/' + name;
    try {
      const res = await fetch('/api/files/write', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path, content: '' }),
        credentials: 'same-origin',
      });
      if (res.ok) { showToast('Datei erstellt!'); loadFiles(currentPath); }
    } catch (err) { showToast('Fehler: ' + err.message, 'error'); }
  });
});

// New Directory
document.getElementById('btn-new-dir').addEventListener('click', () => {
  showModal('Neuer Ordner', 'Ordnername', async (name) => {
    if (!name) return;
    const path = currentPath === '.' ? name : currentPath + '/' + name;
    try {
      const res = await fetch('/api/files/mkdir', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path }),
        credentials: 'same-origin',
      });
      if (res.ok) { showToast('Ordner erstellt!'); loadFiles(currentPath); }
    } catch (err) { showToast('Fehler: ' + err.message, 'error'); }
  });
});

// Ctrl+S / Cmd+S
document.addEventListener('keydown', (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === 's') {
    e.preventDefault();
    if (!fileEditor.classList.contains('hidden')) {
      document.getElementById('btn-save').click();
    }
  }
});

// Tab key in editor inserts spaces
editorContent.addEventListener('keydown', (e) => {
  if (e.key === 'Tab') {
    e.preventDefault();
    const start = editorContent.selectionStart;
    const end = editorContent.selectionEnd;
    editorContent.value =
      editorContent.value.substring(0, start) + '    ' + editorContent.value.substring(end);
    editorContent.selectionStart = editorContent.selectionEnd = start + 4;
  }
});

function showModal(title, placeholder, onConfirm) {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';

  const modal = document.createElement('div');
  modal.className = 'modal';

  const h3 = document.createElement('h3');
  h3.textContent = title;

  const input = document.createElement('input');
  input.type = 'text';
  input.placeholder = placeholder;

  const actions = document.createElement('div');
  actions.className = 'modal-actions';

  const cancelBtn = document.createElement('button');
  cancelBtn.className = 'toolbar-btn';
  cancelBtn.textContent = 'Abbrechen';

  const confirmBtn = document.createElement('button');
  confirmBtn.className = 'toolbar-btn primary';
  confirmBtn.textContent = 'Erstellen';

  actions.appendChild(cancelBtn);
  actions.appendChild(confirmBtn);
  modal.appendChild(h3);
  modal.appendChild(input);
  modal.appendChild(actions);
  overlay.appendChild(modal);
  document.body.appendChild(overlay);

  input.focus();

  const close = () => overlay.remove();
  cancelBtn.addEventListener('click', close);
  overlay.addEventListener('click', (e) => { if (e.target === overlay) close(); });

  const confirm = () => { onConfirm(input.value.trim()); close(); };
  confirmBtn.addEventListener('click', confirm);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') confirm();
    if (e.key === 'Escape') close();
  });
}
