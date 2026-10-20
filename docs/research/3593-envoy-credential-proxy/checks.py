import base64, concurrent.futures, http.client, json, pathlib, socket, ssl, statistics, subprocess, sys, threading, time
from spike import ROOT, generate
CA=ssl.create_default_context(cafile=str(ROOT/'ca.pem'))
def connection(v='a',host='forge.test',port=443):
    s=socket.socket(socket.AF_UNIX); s.settimeout(20); s.connect(str(ROOT/(v+'.sock')))
    s.sendall(f'CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n'.encode()); data=b''
    while b'\r\n\r\n' not in data: data+=s.recv(1)
    if b' 200 ' not in data: raise RuntimeError(data)
    s=CA.wrap_socket(s,server_hostname=host)
    c=http.client.HTTPConnection(host); c.sock=s; return c

def request(c,path='/',token='placeholder-a',body=None):
    c.request('POST' if body else 'GET',path,body=body,headers={'Authorization':'Bearer '+token}); r=c.getresponse(); return r.status,r.read()
def emit(**kw): print(json.dumps(kw),flush=True)
# Real socket scenarios exercise routing, identity, and body-policy boundaries from #3593.
def basics():
    for v,t,path,body in [('a','placeholder-a','/',None),('b','placeholder-a','/',None),('a','placeholder-a','/graphql','{"query":"query { viewer { login } }"}'),('a','placeholder-a','/graphql','{"query":"mutation { nope }"}')]:
        c=connection(v); result=request(c,path,t,body); emit(vessel=v,path=path,result=[result[0],result[1].decode()]);c.close()
    s=socket.socket(socket.AF_UNIX);s.connect(str(ROOT/'a.sock'));s.sendall(b'CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n');emit(unlisted=s.recv(4096).decode());s.close()
def lifecycle():
    generate(['a']);time.sleep(.3)
    def stream(v):
        start=time.monotonic();count=0;error=None
        try:
            c=connection(v);c.request('GET','/stream',headers={'Authorization':'Bearer placeholder-'+v});r=c.getresponse()
            while chunk:=r.read(65536):count+=len(chunk)
            c.close()
        except Exception as e:error=repr(e)
        return dict(vessel=v,bytes=count,seconds=time.monotonic()-start,error=error)
    with concurrent.futures.ThreadPoolExecutor() as pool:
        a=pool.submit(stream,'a');time.sleep(.5);t=time.monotonic();generate(['a','b'])
        while True:
            try:c=connection('b');request(c,token='placeholder-b');c.close();break
            except (OSError,RuntimeError):
                if time.monotonic()-t>5:raise
                time.sleep(.01)
        emit(add_listener_seconds=time.monotonic()-t)
        b=pool.submit(stream,'b');time.sleep(.5);t=time.monotonic();generate(['a'])
        time.sleep(.1)
        try: c=connection('b');emit(removed_accepts=True);c.close()
        except Exception as e:emit(removed_accepts=False,error=str(e),remove_seconds=time.monotonic()-t)
        ra,rb=a.result(),b.result()
        # Criterion 1: neither stream may be truncated during listener removal.
        assert ra['bytes']==rb['bytes']==200*65536,(ra,rb)
        emit(stream_a=ra,stream_b=rb)
    generate(['a','b']);time.sleep(.2)
def benchmark():
    for enabled in [True,False,True]:
        generate(['a','b'],enabled);time.sleep(.2);c=connection();samples=[]
        for i in range(420):
            t=time.perf_counter_ns();status,body=request(c)
            assert status==200,(status,body)
            if i>=20:samples.append((time.perf_counter_ns()-t)/1e6)
        c.close();samples.sort();emit(auth=enabled,n=len(samples),median_ms=statistics.median(samples),p99_ms=samples[395],samples_ms=samples)
def latency():
    # Criterion 9: measure each auth check directly; keep TLS/CONNECT outside samples.
    c=connection(); samples=[]
    for i in range(520):
        t=time.perf_counter_ns();status,body=request(c,'/latency-warmup' if i<20 else '/latency')
        assert status==200,(status,body)
        if i>=20:samples.append((time.perf_counter_ns()-t)/1e6)
    c.close();samples.sort();emit(n=500,median_ms=statistics.median(samples),p99_ms=samples[494])
if __name__=='__main__':globals()[sys.argv[1]]()
