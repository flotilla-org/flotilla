"""Throwaway Envoy experiment; all credentials are deliberately fake."""
import argparse, base64, concurrent.futures, json, os, pathlib, ssl, subprocess, threading, time
import grpc, yaml
import socket, socketserver, select, tempfile
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from envoy.service.auth.v3 import external_auth_pb2 as auth, external_auth_pb2_grpc as auth_grpc
from envoy.service.ext_proc.v3 import external_processor_pb2 as proc, external_processor_pb2_grpc as proc_grpc
from google.rpc import status_pb2
ROOT=pathlib.Path('/tmp/envoy-spike')
def typed(name, **kw): return {'@type':'type.googleapis.com/'+name, **kw}
def cluster(name,port):
    return dict(name=name,connect_timeout='2s',type='STATIC',load_assignment=dict(cluster_name=name,endpoints=[dict(lb_endpoints=[dict(endpoint=dict(address=dict(socket_address=dict(address='127.0.0.1',port_value=port))))])]))
def hcm(routes,filters,connect=False):
    return typed('envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager',stat_prefix='http',codec_type='AUTO',route_config=dict(name='routes',virtual_hosts=[dict(name='all',domains=['*'],routes=routes)]),http_filters=filters,**({'upgrade_configs':[{'upgrade_type':'CONNECT'}], 'http_protocol_options':{'allow_absolute_url':True}} if connect else {}))
def router(): return dict(name='envoy.filters.http.router',typed_config=typed('envoy.extensions.filters.http.router.v3.Router'))
def generate(vessels=('a','b'),authz=True):
    cs=[cluster('upstream',18443),cluster('auth',19001),cluster('proc',19002)]
    cs[0]['transport_socket']=dict(name='envoy.transport_sockets.tls',typed_config=typed('envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext'))
    cs[1]['typed_extension_protocol_options']={'envoy.extensions.upstreams.http.v3.HttpProtocolOptions':typed('envoy.extensions.upstreams.http.v3.HttpProtocolOptions',explicit_http_config={'http2_protocol_options':{}})}
    cs[2]['typed_extension_protocol_options']=cs[1]['typed_extension_protocol_options']
    # Real external addresses are fixed before Envoy starts; no unrestricted dynamic forwarding.
    import socket
    for host,port in [('github.com',22),('echo.websocket.org',443),('chatgpt.com',443)]:
        c=cluster(host,port); c['load_assignment']['endpoints'][0]['lb_endpoints'][0]['endpoint']['address']['socket_address']['address']=socket.gethostbyname(host); cs.append(c)
    listeners=[]
    for v in vessels:
        inner='tls_'+v
        endpoint = {'address': {'envoy_internal_address': {'server_listener_name': inner}}}
        cs.append({'name': inner, 'connect_timeout': '2s', 'type': 'STATIC',
                   'load_assignment': {'cluster_name': inner, 'endpoints': [{'lb_endpoints': [{'endpoint': endpoint}]}]},
                   'transport_socket': {'name': 'envoy.transport_sockets.internal_upstream',
                       'typed_config': typed('envoy.extensions.transport_sockets.internal_upstream.v3.InternalUpstreamTransport',
                           transport_socket={'name': 'envoy.transport_sockets.raw_buffer',
                             'typed_config': typed('envoy.extensions.transport_sockets.raw_buffer.v3.RawBuffer')})}})

        filters=[]
        if authz: filters.append(dict(name='envoy.filters.http.ext_authz',typed_config=typed('envoy.extensions.filters.http.ext_authz.v3.ExtAuthz',transport_api_version='V3',grpc_service=dict(envoy_grpc=dict(cluster_name='auth'),timeout='2s'),failure_mode_allow=False, emit_filter_state_stats=True, status_on_error={'code':'Forbidden'})))
        filters += [dict(name='envoy.filters.http.ext_proc',typed_config=typed('envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor',grpc_service=dict(envoy_grpc=dict(cluster_name='proc'),timeout='2s'),failure_mode_allow=False,processing_mode=dict(request_header_mode='SEND',response_header_mode='SKIP',request_body_mode='NONE',response_body_mode='NONE',request_trailer_mode='SKIP',response_trailer_mode='SKIP'),allow_mode_override=True)),router()]
        routes=[dict(match=dict(prefix='/'),route=dict(cluster='upstream',timeout='0s'),typed_per_filter_config={'envoy.filters.http.ext_authz':typed('envoy.extensions.filters.http.ext_authz.v3.ExtAuthzPerRoute',check_settings={'context_extensions':{'vessel':v}})})]
        tls=dict(name='envoy.transport_sockets.tls',typed_config=typed('envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext',common_tls_context=dict(tls_certificates=[dict(certificate_chain=dict(filename=str(ROOT/'cert.pem')),private_key=dict(filename=str(ROOT/'key.pem')))])))
        inner_hcm=hcm(routes,filters)
        inner_hcm['access_log']=[{'name':'envoy.access_loggers.file','typed_config':typed('envoy.extensions.access_loggers.file.v3.FileAccessLog',path=str(ROOT/'auth-timing.jsonl'),log_format={'json_format':{'path':'%REQ(:PATH)%','auth_us':'%FILTER_STATE(envoy.filters.http.ext_authz:FIELD:latency_us)%','code':'%RESPONSE_CODE%'}})}]
        listeners.append(dict(name=inner,internal_listener={},filter_chains=[dict(transport_socket=tls,filters=[dict(name='envoy.filters.network.http_connection_manager',typed_config=inner_hcm)])]))
        outer_routes=[]
        for host in ['forge.test:443','git.test:443','github.com:22','echo.websocket.org:443','chatgpt.com:443']:
            dest=inner if host.split(':')[0] in ('forge.test','git.test') else host.split(':')[0]
            outer_routes.append(dict(match=dict(connect_matcher={},headers=[dict(name=':authority',string_match=dict(exact=host))]),route=dict(cluster=dest,timeout='0s',upgrade_configs=[dict(upgrade_type='CONNECT',connect_config={})])))
        outer_routes.append(dict(match=dict(connect_matcher={}),direct_response=dict(status=403)))
        outer_routes.append(dict(match=dict(prefix='/'),direct_response=dict(status=403)))
        outer_hcm=hcm(outer_routes,[router()],True)
        outer_hcm['access_log']=[{'name':'envoy.access_loggers.file','typed_config':typed('envoy.extensions.access_loggers.file.v3.FileAccessLog',path=str(ROOT/'access.jsonl'),log_format={'json_format':{'authority':'%REQ(:AUTHORITY)%','code':'%RESPONSE_CODE%','details':'%RESPONSE_CODE_DETAILS%','received':'%BYTES_RECEIVED%','sent':'%BYTES_SENT%','ms':'%DURATION%','upstream':'%UPSTREAM_HOST%'}})}]
        listeners.append(dict(name='vessel_'+v,address=dict(pipe=dict(path=str(ROOT/(v+'.sock')))),filter_chains=[dict(filters=[dict(name='envoy.filters.network.http_connection_manager',typed_config=outer_hcm)])]))
    boot=dict(node=dict(id='spike',cluster='spike'),admin=dict(address=dict(socket_address=dict(address='127.0.0.1',port_value=19000))),bootstrap_extensions=[dict(name='envoy.bootstrap.internal_listener',typed_config=typed('envoy.extensions.bootstrap.internal_listener.v3.InternalListener'))],dynamic_resources=dict(lds_config=dict(path_config_source=dict(path=str(ROOT/'lds.yaml')),resource_api_version='V3'),cds_config=dict(path_config_source=dict(path=str(ROOT/'cds.yaml')),resource_api_version='V3')))
    # Keep all clusters stable across LDS updates.
    if not (ROOT/'bootstrap.yaml').exists(): (ROOT/'bootstrap.yaml').write_text(yaml.safe_dump(boot))
    # Retain removed vessels' clusters so active streams can drain on old listeners.
    if (ROOT/'cds.yaml').exists():
        old=yaml.safe_load((ROOT/'cds.yaml').read_text())['resources']
        names={c['name'] for c in cs}
        cs += [{k:v for k,v in c.items() if k!='@type'} for c in old if c['name'] not in names]
    tmp=ROOT/'cds.new'; tmp.write_text(yaml.safe_dump(dict(version_info=str(time.time_ns()),resources=[{'@type':'type.googleapis.com/envoy.config.cluster.v3.Cluster',**c} for c in cs])))
    tmp.replace(ROOT/'cds.yaml')
    tmp=ROOT/'lds.new'; tmp.write_text(yaml.safe_dump(dict(version_info=str(time.time_ns()),resources=[{'@type':'type.googleapis.com/envoy.config.listener.v3.Listener',**x} for x in listeners])))
    tmp.replace(ROOT/'lds.yaml')
class Auth(auth_grpc.AuthorizationServicer):
    def Check(self,req,ctx):
        vessel=req.attributes.context_extensions.get('vessel')
        value=req.attributes.request.http.headers.get('authorization','')
        expected=['Bearer placeholder-'+vessel,'Basic '+base64.b64encode(('user:placeholder-'+vessel).encode()).decode()]
        allowed=value in expected
        print(json.dumps(dict(hook='auth',vessel=vessel,allowed=allowed)),flush=True)
        if not value: return auth.CheckResponse(status=status_pb2.Status(code=7),denied_response={'status':{'code':401},'headers':[{'header':{'key':'www-authenticate','value':'Basic realm="test"'}}]})
        if not allowed: return auth.CheckResponse(status=status_pb2.Status(code=7),denied_response={'status':{'code':403}})
        return auth.CheckResponse(status=status_pb2.Status(code=0),ok_response={'headers':[{'header':{'key':'authorization','value':'Bearer real-test-token-'+vessel}}]})
class Proc(proc_grpc.ExternalProcessorServicer):
    def Process(self,requests,context):
        for req in requests:
            if req.HasField('request_headers'):
                gql=any(h.key==':path' and h.raw_value.startswith(b'/graphql') for h in req.request_headers.headers.headers)
                yield proc.ProcessingResponse(request_headers={},mode_override={'request_body_mode':'BUFFERED' if gql else 'NONE'})
            elif req.HasField('request_body'):
                body=req.request_body.body
                print(json.dumps(dict(hook='proc',body=body.decode(errors='replace'))),flush=True)
                if b'mutation' in body: yield proc.ProcessingResponse(immediate_response={'status':{'code':403},'body':b'mutation denied'})
                else: yield proc.ProcessingResponse(request_body={})
class Upstream(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def do_GET(self): self.handle_req()
    def do_POST(self): self.handle_req()
    def log_message(self,*args): pass
    def handle_req(self):
        self.connection.setsockopt(socket.IPPROTO_TCP,socket.TCP_NODELAY,1)
        if self.path=='/scaffold/stop-auth':
            SERVERS[0].stop(0).wait(); self.send_response(200); self.send_header('Content-Length','0'); self.end_headers(); return
        if self.path=='/stream':
            self.send_response(200); self.send_header('Content-Length',str(200*65536)); self.end_headers()
            try:
                for _ in range(200): self.wfile.write(b'x'*65536); self.wfile.flush(); time.sleep(.05)
            except (BrokenPipeError, ConnectionResetError): pass
            return
        start=time.monotonic(); length=int(self.headers.get('content-length',0)); bodyfile=tempfile.TemporaryFile(); count=0
        if self.headers.get('transfer-encoding','').lower()=='chunked':
            while True:
                n=int(self.rfile.readline().split(b';')[0],16)
                if n==0:
                    while self.rfile.readline()!=b'\r\n': pass
                    break
                left=n
                while left:
                    chunk=self.rfile.read(min(left,65536)); bodyfile.write(chunk); count+=len(chunk); left-=len(chunk)
                self.rfile.read(2)
        else:
            while count<length:
                chunk=self.rfile.read(min(length-count,65536)); bodyfile.write(chunk); count+=len(chunk)
        bodyfile.seek(0)
        print(json.dumps(dict(upstream=self.path,authorization=self.headers.get('authorization'),bytes=count,seconds=time.monotonic()-start)),flush=True)
        if self.path.startswith('/repo.git'):
            env={**os.environ,'GIT_PROJECT_ROOT':str(ROOT),'GIT_HTTP_EXPORT_ALL':'1','PATH_INFO':self.path.split('?')[0],'QUERY_STRING':self.path.partition('?')[2],'REQUEST_METHOD':self.command,'CONTENT_TYPE':self.headers.get('content-type',''),'CONTENT_LENGTH':str(count),'REMOTE_USER':'test'}
            p=subprocess.run(['git','http-backend'],stdin=bodyfile,stdout=subprocess.PIPE,env=env)
            head,sep,data=p.stdout.partition(b'\r\n\r\n'); self.send_response(200)
            for line in head.split(b'\r\n'):
                if b': ' in line:
                    k,val=line.split(b': ',1)
                    if k.lower()!=b'status': self.send_header(k.decode(),val.decode())
        else: data=json.dumps({'authorization':self.headers.get('authorization'),'bytes':count}).encode(); self.send_response(200)
        self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)
class Bridge(socketserver.BaseRequestHandler):
    def handle(self):
        upstream=socket.socket(socket.AF_UNIX); upstream.connect(str(ROOT/self.server.uds))
        try:
            while True:
                ready,_,_=select.select([self.request,upstream],[],[],120)
                if not ready: break
                for src in ready:
                    data=src.recv(65536)
                    if not data: return
                    (upstream if src is self.request else self.request).sendall(data)
        finally: upstream.close()
class BridgeServer(socketserver.ThreadingTCPServer):
    allow_reuse_address=True
    daemon_threads=True
def services():
    for port,uds in [(18080,'a.sock'),(18081,'b.sock')]:
        bridge=BridgeServer(('127.0.0.1',port),Bridge); bridge.uds=uds
        threading.Thread(target=bridge.serve_forever,daemon=True).start()
    for service,port,add in [(Auth(),19001,auth_grpc.add_AuthorizationServicer_to_server),(Proc(),19002,proc_grpc.add_ExternalProcessorServicer_to_server)]:
        server=grpc.server(concurrent.futures.ThreadPoolExecutor(max_workers=16)); add(service,server); server.add_insecure_port('127.0.0.1:'+str(port)); server.start()
        # Retain servers for lifetime of process.
        SERVERS.append(server)
    http=ThreadingHTTPServer(('127.0.0.1',18443),Upstream); ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(ROOT/'cert.pem',ROOT/'key.pem'); http.socket=ctx.wrap_socket(http.socket,server_side=True); http.serve_forever()
SERVERS=[]
if __name__=='__main__':
    p=argparse.ArgumentParser(); p.add_argument('action',choices=['generate','services']); p.add_argument('--vessels',default='a,b'); p.add_argument('--no-auth',action='store_true'); args=p.parse_args()
    if args.action=='generate': generate(args.vessels.split(','),not args.no_auth)
    else: services()
