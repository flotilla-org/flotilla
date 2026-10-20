"""ProxyCommand adapter for OpenSSH, without TLS interception."""
import os,select,socket,sys
s=socket.socket(socket.AF_UNIX);s.connect('/tmp/envoy-spike/a.sock');s.sendall(b'CONNECT github.com:22 HTTP/1.1\r\nHost: github.com:22\r\n\r\n');h=b''
while b'\r\n\r\n' not in h:h+=s.recv(1)
if b' 200 ' not in h:raise RuntimeError(h)
while True:
    ready,_,_=select.select([s,sys.stdin.buffer],[],[])
    for source in ready:
        data=os.read(source.fileno(),65536)
        if not data:sys.exit(0)
        if source is s:os.write(sys.stdout.fileno(),data)
        else:s.sendall(data)
