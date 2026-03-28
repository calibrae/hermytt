const { test, expect } = require('@playwright/test');
const http = require('http');

const TOKEN = 'hermytt-test-token';
const HERMYTT = 'http://localhost:7777';

async function login(page) {
  await page.goto('/login');
  await page.evaluate((token) => {
    sessionStorage.setItem('hermytt-token', token);
  }, TOKEN);
}

function authHeaders() {
  return { 'Content-Type': 'application/json', 'X-Hermytt-Key': TOKEN };
}

// Minimal mock grytti server
function createMockGrytti(port) {
  let sessions = [
    { session_id: 'test-session-1', claude_state: 'idle', telegram_connected: true, telegram_chat_id: 123, messages_processed: 5, debounce_ms: 200 },
  ];
  const server = http.createServer((req, res) => {
    res.setHeader('Content-Type', 'application/json');
    const url = new URL(req.url, `http://localhost:${port}`);

    if (req.method === 'GET' && url.pathname === '/status') {
      res.end(JSON.stringify({ session_id: sessions[0]?.session_id || '', uptime_secs: 120, claude_state: 'idle', messages_processed: 5 }));
    } else if (req.method === 'GET' && url.pathname === '/config') {
      res.end(JSON.stringify({ session_id: sessions[0]?.session_id || '', debounce_ms: 200, mqtt_host: '10.11.0.7', mqtt_port: 1883, telegram_connected: true }));
    } else if (req.method === 'GET' && url.pathname === '/sessions') {
      res.end(JSON.stringify({ sessions }));
    } else if (req.method === 'POST' && url.pathname === '/sessions') {
      let body = '';
      req.on('data', c => body += c);
      req.on('end', () => {
        const data = JSON.parse(body);
        sessions.push({ session_id: data.session_id, claude_state: 'unknown', telegram_connected: false, telegram_chat_id: null, messages_processed: 0, debounce_ms: data.debounce_ms || 200 });
        res.end(JSON.stringify({ ok: true }));
      });
      return;
    } else if (req.method === 'PUT' && url.pathname.startsWith('/sessions/')) {
      const sid = decodeURIComponent(url.pathname.split('/sessions/')[1]);
      let body = '';
      req.on('data', c => body += c);
      req.on('end', () => {
        const data = JSON.parse(body);
        const s = sessions.find(s => s.session_id === sid);
        if (s) {
          if (data.session_id) s.session_id = data.session_id;
          if (data.debounce_ms) s.debounce_ms = data.debounce_ms;
          res.end(JSON.stringify({ ok: true }));
        } else { res.statusCode = 404; res.end(''); }
      });
      return;
    } else if (req.method === 'DELETE' && url.pathname.startsWith('/sessions/')) {
      const sid = decodeURIComponent(url.pathname.split('/sessions/')[1]);
      const idx = sessions.findIndex(s => s.session_id === sid);
      if (idx >= 0) { sessions.splice(idx, 1); res.end(JSON.stringify({ ok: true })); }
      else { res.statusCode = 404; res.end(''); }
    } else if (req.method === 'POST' && url.pathname === '/session/send') {
      res.end(JSON.stringify({ ok: true }));
    } else {
      res.statusCode = 404;
      res.end('');
    }
  });
  return { server, getSessions: () => sessions, resetSessions: () => { sessions = [{ session_id: 'test-session-1', claude_state: 'idle', telegram_connected: true, telegram_chat_id: 123, messages_processed: 5, debounce_ms: 200 }]; } };
}

let mockGrytti;
const MOCK_PORT = 17780;

test.beforeAll(async () => {
  mockGrytti = createMockGrytti(MOCK_PORT);
  await new Promise(resolve => mockGrytti.server.listen(MOCK_PORT, resolve));

  // Register mock grytti with hermytt
  await fetch(`${HERMYTT}/registry/announce`, {
    method: 'POST',
    headers: authHeaders(),
    body: JSON.stringify({ name: 'grytti-test', role: 'parser', endpoint: `http://localhost:${MOCK_PORT}`, meta: { host: 'test', version: '0.1.0' } }),
  });
});

test.afterAll(async () => {
  await fetch(`${HERMYTT}/registry/grytti-test`, { method: 'DELETE', headers: authHeaders() });
  mockGrytti.server.close();
});

test.beforeEach(async () => {
  mockGrytti.resetSessions();
  // Re-announce to keep alive
  await fetch(`${HERMYTT}/registry/announce`, {
    method: 'POST',
    headers: authHeaders(),
    body: JSON.stringify({ name: 'grytti-test', role: 'parser', endpoint: `http://localhost:${MOCK_PORT}`, meta: { host: 'test' } }),
  });
});

test.describe('Admin — Service Panel', () => {

  test('grytti appears in family table with parser badge', async ({ page }) => {
    await login(page);
    await page.goto('/admin');
    await page.waitForTimeout(2000);

    const row = page.locator('#family-table tr', { hasText: 'grytti-test' });
    await expect(row).toBeVisible();
    await expect(row.locator('.role-parser')).toBeVisible();
  });

  test('clicking grytti opens wide parser panel', async ({ page }) => {
    await login(page);
    await page.goto('/admin');
    await page.waitForTimeout(2000);

    await page.locator('#family-table tr', { hasText: 'grytti-test' }).click();
    await page.waitForTimeout(1000);

    const modal = page.locator('#modal-bg.active');
    await expect(modal).toBeVisible();
    await expect(page.locator('#modal-box')).toHaveClass(/wide/);
    await expect(page.locator('#modal-title')).toContainText('PTY Parser');
  });

  test('parser panel shows sessions table', async ({ page }) => {
    await login(page);
    await page.goto('/admin');
    await page.waitForTimeout(2000);

    await page.locator('#family-table tr', { hasText: 'grytti-test' }).click();
    await page.waitForTimeout(1000);

    await expect(page.locator('#modal-fields td', { hasText: 'test-sessio' })).toBeVisible();
    await expect(page.locator('#modal-fields .svc-status', { hasText: 'idle' })).toBeVisible();
  });

  test('proxy forwards GET /sessions to grytti', async () => {
    const res = await fetch(`${HERMYTT}/registry/grytti-test/proxy/sessions`, { headers: authHeaders() });
    expect(res.ok).toBeTruthy();
    const data = await res.json();
    expect(data.sessions).toHaveLength(1);
    expect(data.sessions[0].session_id).toBe('test-session-1');
  });

  test('proxy forwards POST /sessions to add session', async () => {
    const res = await fetch(`${HERMYTT}/registry/grytti-test/proxy/sessions`, {
      method: 'POST',
      headers: authHeaders(),
      body: JSON.stringify({ session_id: 'new-sess', bot_token: '123:ABC', debounce_ms: 300 }),
    });
    expect(res.ok).toBeTruthy();
    expect(mockGrytti.getSessions()).toHaveLength(2);
  });

  test('proxy forwards PUT /sessions/{id} to update session', async () => {
    const res = await fetch(`${HERMYTT}/registry/grytti-test/proxy/sessions/test-session-1`, {
      method: 'PUT',
      headers: authHeaders(),
      body: JSON.stringify({ debounce_ms: 500 }),
    });
    expect(res.ok).toBeTruthy();
    expect(mockGrytti.getSessions()[0].debounce_ms).toBe(500);
  });

  test('proxy forwards DELETE /sessions/{id} to remove session', async () => {
    const res = await fetch(`${HERMYTT}/registry/grytti-test/proxy/sessions/test-session-1`, {
      method: 'DELETE',
      headers: authHeaders(),
    });
    expect(res.ok).toBeTruthy();
    expect(mockGrytti.getSessions()).toHaveLength(0);
  });

  test('proxy returns 404 for unknown service', async () => {
    const res = await fetch(`${HERMYTT}/registry/nonexistent/proxy/status`, { headers: authHeaders() });
    expect(res.status).toBe(404);
  });

  test('proxy forwards POST /session/send', async () => {
    const res = await fetch(`${HERMYTT}/registry/grytti-test/proxy/session/send`, {
      method: 'POST',
      headers: authHeaders(),
      body: JSON.stringify({ session_id: 'test-session-1', text: 'ls -la' }),
    });
    expect(res.ok).toBeTruthy();
  });
});
