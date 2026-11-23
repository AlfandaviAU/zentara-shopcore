// Breakpoint test: ramp one traffic class until the service breaks, to find its ceiling.
//
//   docker run --rm --network host -e MODE=read  -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/breakpoint.js
//   docker run --rm --network host -e MODE=auth  -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/breakpoint.js
//   docker run --rm --network host -e MODE=login -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/breakpoint.js
//   docker run --rm --network host -e MODE=order -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/breakpoint.js
//
// MODE=read  anonymous catalogue reads (no auth)
// MODE=auth  authenticated reads (session lookup + query)
// MODE=login register + login (Argon2id, CPU-bound by design)
// MODE=order create order + wallet pay (write path: transactions, row locks)
// PEAK sets the final arrival rate (iterations/s). The test aborts once p95 > 1 s or errors > 1%.
import http from 'k6/http';
import { check } from 'k6';

const BASE = __ENV.BASE_URL || 'http://localhost:8000';
const MODE = __ENV.MODE || 'read';
const PEAK = Number(__ENV.PEAK || { read: 40000, auth: 30000, login: 2000, order: 8000 }[MODE]);

http.setResponseCallback(http.expectedStatuses({ min: 200, max: 299 }, 409));

export const options = {
  discardResponseBodies: true,
  scenarios: {
    ramp: {
      executor: 'ramping-arrival-rate', startRate: Math.round(PEAK / 20), timeUnit: '1s',
      preAllocatedVUs: 500, maxVUs: 4000,
      stages: [{ target: PEAK, duration: '60s' }],
    },
  },
  thresholds: {
    http_req_failed: [{ threshold: 'rate<0.01', abortOnFail: true, delayAbortEval: '5s' }],
    http_req_duration: [{ threshold: 'p(95)<1000', abortOnFail: true, delayAbortEval: '5s' }],
  },
};

const hdr = (t) => ({ headers: Object.assign({ 'Content-Type': 'application/json' }, t ? { Authorization: `Bearer ${t}` } : {}) });
const rid = () => `${__VU}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 9)}`;

function user() {
  const u = `bp_${rid()}`;
  http.post(`${BASE}/api/register`, JSON.stringify({ username: u, email: `${u}@x.io`, password: 'pw' }), hdr());
  const r = http.post(`${BASE}/api/login`, JSON.stringify({ username: u, password: 'pw' }), Object.assign(hdr(), { responseType: 'text' }));
  return r.json('token');
}

export function setup() {
  const adm = http.post(`${BASE}/api/login`, JSON.stringify({ username: 'admin', password: 'admin123' }), Object.assign(hdr(), { responseType: 'text' })).json('token');
  // SKUS=1 models a flash sale on one product (hot row); SKUS=200 spreads orders over a catalogue
  const pids = [];
  for (let i = 0; i < Number(__ENV.SKUS || 1); i++) {
    const pr = http.post(`${BASE}/api/products`, JSON.stringify({ sku: `BP-${rid()}`, name: 'Breakpoint', price: 1.5, stock: 100000000 }), Object.assign(hdr(adm), { responseType: 'text' }));
    pids.push(pr.json('id'));
  }
  const tokens = [];
  if (MODE === 'auth' || MODE === 'order') {
    for (let i = 0; i < 200; i++) {
      const t = user();
      http.post(`${BASE}/api/wallet/topup`, JSON.stringify({ amount: 100000000 }), hdr(t));
      tokens.push(t);
    }
  }
  return { pid: pids[0], pids, tokens };
}

export default function (d) {
  let r;
  if (MODE === 'read') {
    r = Math.random() < 0.5 ? http.get(`${BASE}/api/products?limit=20`) : http.get(`${BASE}/api/products/${d.pid}`);
  } else if (MODE === 'auth') {
    const t = d.tokens[Math.floor(Math.random() * d.tokens.length)];
    r = Math.random() < 0.5 ? http.get(`${BASE}/api/users/me`, hdr(t)) : http.get(`${BASE}/api/orders?limit=5`, hdr(t));
  } else if (MODE === 'login') {
    const u = `bp_${rid()}`;
    r = http.post(`${BASE}/api/register`, JSON.stringify({ username: u, email: `${u}@x.io`, password: 'pw' }), hdr());
  } else {
    const t = d.tokens[Math.floor(Math.random() * d.tokens.length)];
    const o = http.post(`${BASE}/api/orders`, JSON.stringify({ items: [{ product_id: d.pids[Math.floor(Math.random() * d.pids.length)], quantity: 1 }] }), Object.assign(hdr(t), { responseType: 'text' }));
    if (o.status === 201) r = http.post(`${BASE}/api/orders/${o.json('id')}/pay`, JSON.stringify({ method: 'wallet' }), hdr(t));
    else r = o;
  }
  check(r, { ok: (x) => x.status >= 200 && x.status < 300 });
}
