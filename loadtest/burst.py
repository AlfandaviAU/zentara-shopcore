"""Concurrency burst: 200 simultaneous order + bank-pay flows against 150 units of stock,
with the bank dropping 30% of responses. Checks overselling, stock accuracy and double charges.

    python3 loadtest/burst.py http://localhost:8000 http://localhost:5001

Needs Python 3 with `requests`. Results: docs/BENCHMARKS.md section 2.2.
"""
import requests, uuid, time, sys, collections
from concurrent.futures import ThreadPoolExecutor
B = sys.argv[1]; BANK = sys.argv[2]
s = requests.Session(); s.mount("http://", requests.adapters.HTTPAdapter(pool_maxsize=100))
def user():
    n = "s_" + uuid.uuid4().hex[:10]
    s.post(B+"/api/register", json={"username": n, "email": n+"@x.io", "password": "pw"})
    return s.post(B+"/api/login", json={"username": n, "password": "pw"}).json()["token"]
adm = s.post(B+"/api/login", json={"username":"admin","password":"admin123"}).json()["token"]
pid = s.post(B+"/api/products", headers={"Authorization":"Bearer "+adm}, json={"sku":"ST-"+uuid.uuid4().hex[:5],"name":"Stress","price":9.99,"stock":150}).json()["id"]
requests.post(BANK+"/reset"); requests.post(BANK+"/config", json={"drop_rate": 0.3})
toks = list(ThreadPoolExecutor(50).map(lambda _: user(), range(200)))
t0 = time.time()
def flow(tok):
    h = {"Authorization": "Bearer "+tok}
    r = s.post(B+"/api/orders", headers=h, json={"items":[{"product_id": pid, "quantity": 1}]})
    if r.status_code != 201: return ("order", r.status_code)
    oid = r.json()["id"]
    p = s.post(B+"/api/orders/%d/pay" % oid, headers=h, json={"method":"bank"})
    return ("pay", p.status_code, oid)
res = list(ThreadPoolExecutor(100).map(flow, toks))
dt = time.time() - t0
print("elapsed %.2fs for 200 order+pay flows" % dt)
print(collections.Counter((r[0], r[1]) for r in res))
stock = s.get(B+"/api/products/%d" % pid).json()["stock"]
print("stock left (150 - orders created):", stock)
st = requests.get(BANK+"/stats").json()
per = collections.Counter(c["order_id"] for c in st["items"])
print("bank charges:", st["charges"], "orders charged twice:", sum(1 for v in per.values() if v > 1))
requests.post(BANK+"/config", json={"drop_rate": 0})
time.sleep(1)
paid = [r[2] for r in res if r[0]=="pay" and r[1]==200]
statuses = collections.Counter(s.get(B+"/api/orders/%d" % o, headers={"Authorization":"Bearer "+adm}).json()["status"] for o in paid)
print("statuses of paid orders:", statuses)
