// k6 load test: realistic mixed traffic with SLO thresholds.
//
//   docker compose up -d --build
//   docker run --rm --network host -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/shopcore.js
//
// Env: BASE_URL (default http://localhost:8000), BANK_URL (default http://localhost:5001),
//      SCALE (multiplies every arrival rate, default 1).
//
// Scenarios (open model: arrival rate does not slow down when the server does):
//   browse   - anonymous catalogue traffic (list, search, sort, product detail)  ~80% of requests
//   checkout - register, login, top up, order, pay by wallet or bank (bank drops 10%, 50ms latency)
//   account  - logged-in users reading orders, invoices, wallet, transfers
//   backoffice - admin sales reports and exports
import http from 'k6/http';
import { check, fail } from 'k6';
import { Counter } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://localhost:8000';
const BANK = __ENV.BANK_URL || 'http://localhost:5001';
const SCALE = Number(__ENV.SCALE || 1);
const r = (n) => Math.max(1, Math.round(n * SCALE));

// 402/409 are business outcomes (no funds, out of stock), not failures.
http.setResponseCallback(http.expectedStatuses({ min: 200, max: 299 }, 402, 409));

const invariantViolations = new Counter('invariant_violations');

export const options = {
  discardResponseBodies: false,
  scenarios: {
    browse: {
      executor: 'ramping-arrival-rate', exec: 'browse', startRate: r(200), timeUnit: '1s',
      preAllocatedVUs: 200, maxVUs: 1000,
      stages: [
        { target: r(1500), duration: '20s' },
        { target: r(3000), duration: '40s' },
        { target: r(3000), duration: '30s' },
        { target: 0, duration: '10s' },
      ],
    },
    checkout: {
      executor: 'ramping-arrival-rate', exec: 'checkout', startRate: r(10), timeUnit: '1s',
      preAllocatedVUs: 100, maxVUs: 600,
      stages: [
        { target: r(80), duration: '20s' },
        { target: r(150), duration: '40s' },
        { target: r(150), duration: '30s' },
        { target: 0, duration: '10s' },
      ],
    },
    account: {
      executor: 'constant-arrival-rate', exec: 'account', rate: r(300), timeUnit: '1s',
      duration: '90s', startTime: '10s', preAllocatedVUs: 100, maxVUs: 400,
    },
    backoffice: {
      executor: 'constant-arrival-rate', exec: 'backoffice', rate: r(5), timeUnit: '1s',
      duration: '90s', startTime: '10s', preAllocatedVUs: 10, maxVUs: 50,
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.005'],
    'http_req_duration{scenario:browse}': ['p(95)<100', 'p(99)<250'],
    'http_req_duration{scenario:account}': ['p(95)<150', 'p(99)<400'],
    'http_req_duration{name:pay_wallet}': ['p(95)<150'],
    'http_req_duration{name:pay_bank}': ['p(95)<400'],
    'http_req_duration{name:create_order}': ['p(95)<200'],
    'http_req_duration{name:report}': ['p(95)<500'],
    checks: ['rate>0.99'],
    invariant_violations: ['count==0'],
  },
};

const json = (body, token) => ({
  headers: Object.assign({ 'Content-Type': 'application/json' }, token ? { Authorization: `Bearer ${token}` } : {}),
});
const post = (path, body, token, name) =>
  http.post(BASE + path, JSON.stringify(body), Object.assign(json(body, token), { tags: { name: name || path } }));
const get = (path, token, name) =>
  http.get(BASE + path, Object.assign(json(null, token), { tags: { name: name || path } }));

function rid() {
  return `${__VU}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

function newUser() {
  const u = `lt_${rid()}`;
  const reg = post('/api/register', { username: u, email: `${u}@load.test`, password: 'Passw0rd!' }, null, 'register');
  if (!check(reg, { 'register 201': (x) => x.status === 201 })) return null;
  const login = post('/api/login', { username: u, password: 'Passw0rd!' }, null, 'login');
  if (!check(login, { 'login 200': (x) => x.status === 200 })) return null;
  return { id: reg.json('id'), token: login.json('token') };
}

export function setup() {
  const adm = post('/api/login', { username: 'admin', password: 'admin123' }, null, 'login');
  if (adm.status !== 200) fail('admin login failed: ' + adm.body);
  const admin = adm.json('token');
  const products = [];
  for (let i = 0; i < 20; i++) {
    const p = post('/api/products', { sku: `LT-${i}-${rid()}`, name: `Load Product ${i}`, price: 5 + i * 1.25, stock: 10000000 }, admin);
    products.push(p.json('id'));
  }
  // a pool of logged-in users for the account scenario
  const users = [];
  for (let i = 0; i < 50; i++) {
    const u = newUser();
    post('/api/wallet/topup', { amount: 100000 }, u.token);
    users.push(u);
  }
  // realistic, flaky bank: 50ms latency, 10% of responses lost after the charge is recorded
  http.post(`${BANK}/reset`);
  http.post(`${BANK}/config`, JSON.stringify({ latency_ms: 50, drop_rate: 0.1 }), json());
  return { admin, products, users };
}

export function browse(data) {
  const roll = Math.random();
  let res;
  if (roll < 0.4) res = get(`/api/products?limit=20&page=${1 + Math.floor(Math.random() * 3)}`, null, 'list_products');
  else if (roll < 0.6) res = get(`/api/products?sort=${['price', 'name', 'stock', 'sku'][Math.floor(Math.random() * 4)]}&limit=10`, null, 'list_sorted');
  else if (roll < 0.75) res = get('/api/products?q=load', null, 'search');
  else res = get(`/api/products/${data.products[Math.floor(Math.random() * data.products.length)]}`, null, 'product');
  check(res, { 'browse 200': (x) => x.status === 200 });
}

export function checkout(data) {
  const u = newUser();
  if (!u) return;
  const lines = 1 + Math.floor(Math.random() * 3);
  const items = [];
  for (let i = 0; i < lines; i++) {
    items.push({ product_id: data.products[Math.floor(Math.random() * data.products.length)], quantity: 1 + Math.floor(Math.random() * 3) });
  }
  const useCoupon = Math.random() < 0.2;
  const order = post('/api/orders', useCoupon ? { items, coupon: 'SAVE10' } : { items }, u.token, 'create_order');
  if (!check(order, { 'order 201': (x) => x.status === 201 })) return;
  const o = order.json();
  const expected = o.items.reduce((s, it) => s + it.price * it.quantity, 0) * (useCoupon ? 0.9 : 1);
  if (Math.abs(o.total - Math.round(expected * 100) / 100) > 0.011) invariantViolations.add(1);

  if (Math.random() < 0.5) {
    post('/api/wallet/topup', { amount: 1000 }, u.token, 'topup');
    const pay = post(`/api/orders/${o.id}/pay`, { method: 'wallet' }, u.token, 'pay_wallet');
    check(pay, { 'wallet pay 200': (x) => x.status === 200 });
    const bal = get('/api/wallet', u.token, 'wallet').json('balance');
    if (Math.abs(bal - (1000 - o.total)) > 0.011) invariantViolations.add(1);
    // a double click must never charge twice
    const again = post(`/api/orders/${o.id}/pay`, { method: 'wallet' }, u.token, 'pay_wallet');
    check(again, { 'second pay 409': (x) => x.status === 409 });
  } else {
    const pay = post(`/api/orders/${o.id}/pay`, { method: 'bank' }, u.token, 'pay_bank');
    check(pay, { 'bank pay 200': (x) => x.status === 200 && String(x.json('bank_ref')).startsWith('BK-') });
  }
  if (Math.random() < 0.1) {
    const c = post(`/api/orders/${o.id}/cancel`, {}, u.token, 'cancel');
    check(c, { 'cancel 200': (x) => x.status === 200 });
  }
}

export function account(data) {
  const u = data.users[Math.floor(Math.random() * data.users.length)];
  const roll = Math.random();
  if (roll < 0.35) {
    check(get('/api/orders?limit=10', u.token, 'list_orders'), { 'orders 200': (x) => x.status === 200 });
  } else if (roll < 0.6) {
    check(get('/api/users/me', u.token, 'me'), { 'me 200': (x) => x.status === 200 });
  } else if (roll < 0.85) {
    check(get('/api/wallet', u.token, 'wallet'), { 'wallet 200': (x) => x.status === 200 });
  } else {
    const to = data.users[Math.floor(Math.random() * data.users.length)];
    if (to.id === u.id) return;
    const t = post('/api/wallet/transfer', { to_user_id: to.id, amount: 1 }, u.token, 'transfer');
    check(t, { 'transfer 200/402': (x) => x.status === 200 || x.status === 402 });
  }
}

export function backoffice(data) {
  const g = ['day', 'status', 'user'][Math.floor(Math.random() * 3)];
  const rep = get(`/api/reports/sales?from=2000-01-01&to=2999-12-31&group_by=${g}`, data.admin, 'report');
  check(rep, { 'report 200': (x) => x.status === 200 });
  if (Math.random() < 0.1) {
    const ex = post('/api/reports/export', { filename: `lt_${rid()}.csv` }, data.admin, 'export');
    check(ex, { 'export 200': (x) => x.status === 200 });
  }
}

export function teardown(data) {
  http.post(`${BANK}/config`, JSON.stringify({ latency_ms: 0, drop_rate: 0 }), json());
  // money conservation: transfers inside the pool never create or destroy money
  let total = 0;
  for (const u of data.users) total += get('/api/wallet', u.token).json('balance');
  console.log(`pool wallet total after run: ${total.toFixed(2)} (users also paid for nothing; expected <= ${50 * 100000})`);
  if (total > 50 * 100000 + 0.01) invariantViolations.add(1);
  const stats = http.get(`${BANK}/stats`).json();
  const seen = {};
  let dup = 0;
  for (const c of stats.items) {
    if (seen[c.order_id]) dup++;
    seen[c.order_id] = true;
  }
  console.log(`bank charges: ${stats.charges}, duplicate charges for one order: ${dup}`);
  if (dup > 0) invariantViolations.add(dup);
}
