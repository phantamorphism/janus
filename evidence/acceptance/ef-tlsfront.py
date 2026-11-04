#!/usr/bin/env python3
"""TLS 1.3 terminating front for the ef QA mock plugin (python's ssl sends proper TLS alerts,
which node's tls does not). Usage: ef-tlsfront.py LISTEN_PORT BACKEND_PORT KEY CERT CLIENT_CA MODE LOGFILE
MODE: require  -> client cert required and verified against CLIENT_CA
      optional -> client cert optional, but a presented cert must verify against CLIENT_CA
Accepted connections are piped to the plain-h2 backend (the node mock plugin)."""
import socket, ssl, sys, threading, json, hashlib, time
lp, bp = int(sys.argv[1]), int(sys.argv[2]); key, cert, ca, mode, logf = sys.argv[3:8]
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)
ctx.load_verify_locations(ca)
ctx.verify_mode = ssl.CERT_REQUIRED if mode == 'require' else ssl.CERT_OPTIONAL
ctx.set_alpn_protocols(['h2'])
lock = threading.Lock()
def log(ev):
    ev['t'] = time.time()
    with lock, open(logf, 'a') as f: f.write(json.dumps(ev) + '\n')
def pump(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except Exception: pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except Exception: pass
def handle(c):
    try:
        s = ctx.wrap_socket(c, server_side=True)
    except Exception as e:
        log({'event': 'handshake_rejected', 'error': str(e)}); c.close(); return
    der = s.getpeercert(binary_form=True)
    log({'event': 'handshake_ok', 'version': s.version(), 'client_cert_sha256': hashlib.sha256(der).hexdigest() if der else None})
    b = socket.create_connection(('127.0.0.1', bp))
    t = threading.Thread(target=pump, args=(b, s), daemon=True); t.start()
    pump(s, b)
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); srv.bind(('127.0.0.1', lp)); srv.listen(64)
log({'event': 'listening'})
while True:
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
