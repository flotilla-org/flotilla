import asyncio,json,socket,ssl,time
import websockets
async def ws():
    t=time.monotonic()
    try:
        async with websockets.connect('wss://echo.websocket.org',proxy='http://127.0.0.1:18080',open_timeout=15) as w:
            greeting=await asyncio.wait_for(w.recv(),5)
            await w.send('envoy-3593-test-payload');reply=await asyncio.wait_for(w.recv(),5)
            print(json.dumps({'websocket':{'greeting':greeting,'reply':reply,'seconds':time.monotonic()-t}}))
    except Exception as e:print(json.dumps({'websocket_error':repr(e),'seconds':time.monotonic()-t}))
def ssh():
    t=time.monotonic()
    try:
        s=socket.create_connection(('127.0.0.1',18080),timeout=15);s.sendall(b'CONNECT github.com:22 HTTP/1.1\r\nHost: github.com:22\r\n\r\n');head=b''
        while b'\r\n\r\n' not in head:head+=s.recv(1)
        banner=s.recv(1024);print(json.dumps({'ssh_connect':head.decode(),'banner':banner.decode(),'seconds':time.monotonic()-t}));s.close()
    except Exception as e:print(json.dumps({'ssh_error':repr(e),'seconds':time.monotonic()-t}))
if __name__=='__main__':asyncio.run(ws());ssh()
