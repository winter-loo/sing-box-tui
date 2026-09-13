"""Loopback-only telemetry experiment using the installed core; no live config access."""
import argparse, contextlib, http.server, json, pathlib, select, socket, socketserver, subprocess, tempfile, threading, time, urllib.request

class Http(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'x' * (65536 if self.path == '/slow' else 4096)
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Connection', 'close')
        self.end_headers()
        for offset in range(0, len(body), 1024):
            self.wfile.write(body[offset:offset+1024]); self.wfile.flush()
            if self.path == '/slow': time.sleep(.05)
    def log_message(self, *args): pass

def exact(sock, count):
    data = b''
    while len(data) < count:
        chunk = sock.recv(count-len(data))
        if not chunk: raise EOFError()
        data += chunk
    return data

class Socks(socketserver.BaseRequestHandler):
    transferred = 0
    def handle(self):
        client = self.request
        try:
            version, count = exact(client, 2); exact(client, count); client.sendall(b'\x05\x00')
            version, command, reserved, kind = exact(client, 4)
            if kind == 1: host = socket.inet_ntoa(exact(client, 4))
            elif kind == 3: host = exact(client, exact(client, 1)[0]).decode()
            else: raise ValueError('unsupported address')
            port = int.from_bytes(exact(client, 2), 'big')
            if host != '127.0.0.1': raise ValueError('non-loopback forbidden')
            with socket.create_connection((host, port)) as remote:
                client.sendall(b'\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00')
                while True:
                    readable, _, _ = select.select([client, remote], [], [], 10)
                    if not readable: return
                    for source in readable:
                        data = source.recv(65536)
                        if not data: return
                        Socks.transferred += len(data)
                        (remote if source is client else client).sendall(data)
        except (OSError, EOFError, ValueError): pass

class Tcp(socketserver.ThreadingTCPServer):
    daemon_threads = True

def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0)); return sock.getsockname()[1]

def main():
    parser = argparse.ArgumentParser(); parser.add_argument('--core', default=r'C:\Users\Administrator\AppData\Local\sing-box-tui\core\sing-box.exe'); args = parser.parse_args()
    directory = pathlib.Path(tempfile.mkdtemp(prefix='sing-box-clash-contract-'))
    servers = [http.server.ThreadingHTTPServer(('127.0.0.1', 0), Http) for _ in range(2)]
    servers += [Tcp(('127.0.0.1', 0), Socks) for _ in range(2)]
    for server in servers: threading.Thread(target=server.serve_forever, daemon=True).start()
    destination, direct_destination, socks_a, socks_b = [s.server_address[1] for s in servers]
    mixed, controller = free_port(), free_port()
    config = {'log': {'level':'warn'}, 'inbounds':[{'type':'mixed','tag':'probe-in','listen':'127.0.0.1','listen_port':mixed}], 'outbounds':[{'type':'socks','tag':tag,'server':'127.0.0.1','server_port':port} for tag,port in [('node-a',socks_a),('node-b',socks_b)]] + [{'type':'direct','tag':'direct'},{'type':'selector','tag':'select','outbounds':['node-a','node-b'],'default':'node-a','interrupt_exist_connections':False}], 'route':{'rules':[{'port':[direct_destination],'action':'route','outbound':'direct'}],'final':'select'},'experimental':{'clash_api':{'external_controller':f'127.0.0.1:{controller}'}}}
    config_path = directory/'config.json'; config_path.write_text(json.dumps(config), encoding='utf-8')
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    def api(path, body=None):
        request = urllib.request.Request(f'http://127.0.0.1:{controller}{path}', data=None if body is None else json.dumps(body).encode(), method='GET' if body is None else 'PUT', headers={'Content-Type':'application/json'})
        with opener.open(request, timeout=4) as response:
            raw = response.read(); return json.loads(raw) if raw else None
    def transfer(port, path='/'):
        with socket.create_connection(('127.0.0.1',mixed)) as sock:
            sock.sendall(f'GET http://127.0.0.1:{port}{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n'.encode())
            total = 0
            while data := sock.recv(65536): total += len(data)
            return total
    output = {'directory':str(directory),'config':config,'samples':{}}
    with (directory/'core.log').open('wb') as log:
        process = subprocess.Popen([args.core,'run','--config',str(config_path)], stdout=log, stderr=log, creationflags=getattr(subprocess,'CREATE_NO_WINDOW',0))
        output['owned_pid'] = process.pid
        try:
            for _ in range(80):
                try: api('/connections'); break
                except Exception:
                    if process.poll() is not None: raise RuntimeError('core exited')
                    time.sleep(.05)
            def sample(name):
                time.sleep(.1); output['samples'][name] = api('/connections')
            sample('baseline')
            output['short_response_bytes'] = transfer(destination); sample('short_closed')
            output['direct_response_bytes'] = transfer(direct_destination); sample('direct_closed')
            output['socks_bytes_before_delay'] = Socks.transferred
            # HTTPS avoids this core's HTTP-to-default-external-target rewriting.
            # A plain local HTTP server rejects the TLS handshake: the failed probe
            # still transfers bytes through SOCKS, providing a coverage counterexample.
            try:
                output['delay_result'] = api('/proxies/node-a/delay?timeout=2000&url=' + urllib.parse.quote(f'https://127.0.0.1:{destination}/',safe=''))
            except urllib.error.HTTPError as error:
                output['delay_result'] = {'status':error.code,'body':error.read().decode()}
            sample('delay_closed'); output['socks_bytes_after_delay'] = Socks.transferred
            worker = threading.Thread(target=lambda: output.update(slow_response_bytes=transfer(destination,'/slow'))); worker.start()
            time.sleep(.3); sample('old_node_active_before_switch')
            api('/proxies/select',{'name':'node-b'}); sample('old_node_active_after_switch')
            transfer(destination); sample('new_node_short_closed')
            worker.join(timeout=8)
            if worker.is_alive(): raise RuntimeError('slow request did not finish')
            sample('all_closed_final')
        finally:
            process.terminate(); process.wait(timeout=5)
            output['owned_process_exit_code'] = process.returncode
            for server in servers: server.shutdown(); server.server_close()
            (directory/'result.json').write_text(json.dumps(output,indent=2),encoding='utf-8')
    print(json.dumps(output,indent=2))

if __name__ == '__main__': main()
