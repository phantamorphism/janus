#!/usr/bin/env python3
# TLS 1.3 mTLS terminator (OpenSSL alerts are authentic) piping plaintext h2 to a local h2c backend.
import ssl, socket, sys, threading
listen_port, backend_port, tls_dir = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
mode = sys.argv[4] if len(sys.argv) > 4 else "required"
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ctx.maximum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(f"{tls_dir}/plugin.pem", f"{tls_dir}/plugin-key.pem")
ctx.verify_mode = ssl.CERT_REQUIRED if mode == "required" else ssl.CERT_OPTIONAL
ctx.load_verify_locations(f"{tls_dir}/daemon.pem")
ctx.set_alpn_protocols(["h2"])
ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
ls.bind(("127.0.0.1", listen_port)); ls.listen(32)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    except Exception: pass
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except Exception: pass
def handle(raw):
    try:
        c = ctx.wrap_socket(raw, server_side=True)
    except Exception:
        raw.close(); return
    try:
        b = socket.create_connection(("127.0.0.1", backend_port))
    except Exception:
        c.close(); return
    threading.Thread(target=pipe, args=(c, b), daemon=True).start()
    pipe(b, c)
while True:
    r, _ = ls.accept()
    threading.Thread(target=handle, args=(r,), daemon=True).start()
