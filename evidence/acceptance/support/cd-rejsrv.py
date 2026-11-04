# TLS server that rejects the client's cert, then lingers before closing so the alert is not lost to a TCP RST.
import ssl, socket, sys, time, threading
port, cert, key, ca = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.load_cert_chain(cert, key)
ctx.verify_mode = ssl.CERT_REQUIRED
ctx.load_verify_locations(ca)
ctx.set_alpn_protocols(['h2'])
ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
ls.bind(('127.0.0.1', port)); ls.listen(16)
def h(c):
    s = ctx.wrap_socket(c, server_side=True, do_handshake_on_connect=False)
    try:
        s.do_handshake()
        s.close()
    except ssl.SSLError as e:
        print('handshake rejected:', e, flush=True)
        time.sleep(1.5)
        c.close()
while True:
    c, _ = ls.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
