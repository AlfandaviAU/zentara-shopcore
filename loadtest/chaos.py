"""Failure injection against the running compose stack: bank down, database restart, app restart.

    docker compose up -d --build
    python3 loadtest/chaos.py            # run from the repository root

Env: BASE_URL (default http://localhost:8000), COMPOSE_PROJECT (optional, for `docker compose -p`).
Needs Python 3 with `requests`. Results: docs/BENCHMARKS.md section 2.3c.
"""
import os, requests, uuid, subprocess, time
B = os.environ.get("BASE_URL", "http://localhost:8000")
PROJ = ["-p", os.environ["COMPOSE_PROJECT"]] if os.environ.get("COMPOSE_PROJECT") else []
def dc(*a): subprocess.run(["docker", "compose", *PROJ, *a], capture_output=True)
n="c_"+uuid.uuid4().hex[:8]
requests.post(B+"/api/register",json={"username":n,"email":n+"@x.io","password":"pw"})
t=requests.post(B+"/api/login",json={"username":n,"password":"pw"}).json()["token"]; H={"Authorization":"Bearer "+t}
requests.post(B+"/api/wallet/topup",headers=H,json={"amount":1000})
pid=requests.get(B+"/api/products?limit=1").json()[0]["id"]
def order(): return requests.post(B+"/api/orders",headers=H,json={"items":[{"product_id":pid}]}).json()["id"]
print("1) bank DOWN")
dc("stop","bank")
o=order(); t0=time.time()
r=requests.post(B+"/api/orders/%d/pay"%o,headers=H,json={"method":"bank"})
print("   bank pay ->",r.status_code,r.json(),"in %.1fs"%(time.time()-t0))
print("   order still pending:",requests.get(B+"/api/orders/%d"%o,headers=H).json()["status"])
print("   wallet pay still works ->",requests.post(B+"/api/orders/%d/pay"%o,headers=H,json={"method":"wallet"}).status_code)
print("   browsing still works ->",requests.get(B+"/api/products").status_code)
dc("start","bank"); time.sleep(2)
o=order(); print("   bank back, bank pay ->",requests.post(B+"/api/orders/%d/pay"%o,headers=H,json={"method":"bank"}).status_code)
print("2) DATABASE RESTART")
dc("restart","db"); codes={}
t0=time.time(); recovered=None
while time.time()-t0<30:
    try:
        c=requests.get(B+"/api/products",timeout=3).status_code
    except Exception as e: c="conn-err"
    codes[c]=codes.get(c,0)+1
    if c==200 and recovered is None and time.time()-t0>1: recovered=time.time()-t0
    if recovered and time.time()-t0>recovered+2: break
    time.sleep(0.2)
print("   statuses during restart:",codes," recovered after %.1fs"%(recovered or -1))
print("   session survived DB restart (Postgres-backed):",requests.get(B+"/api/users/me",headers=H).status_code)
o=order(); print("   order + pay after restart ->",requests.post(B+"/api/orders/%d/pay"%o,headers=H,json={"method":"wallet"}).status_code)
print("3) APP RESTART")
dc("restart","app"); t0=time.time()
while True:
    try:
        if requests.get(B+"/health",timeout=1).ok: break
    except Exception: pass
    time.sleep(0.05)
print("   app healthy again after %.2fs; session still valid:"%(time.time()-t0),requests.get(B+"/api/users/me",headers=H).status_code)
