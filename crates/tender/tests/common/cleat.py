"""Connect-only cleat protocol-11 probe; never starts or discovers a daemon.

Usage: python3 cleat.py SOCKET SESSION [INPUT]
Source: flotilla-org/cleat fd66a7121149e85eb8fbc57a99a388b0c91dae6c.
The caller owns process/SSH lifecycle; this client only owns its socket.
"""
import json
import socket
import struct
import sys


def varint(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def string(value):
    value = value.encode()
    return varint(len(value)) + value


def render_generation(payload):
    offset = 0

    def integer():
        nonlocal offset
        value, shift = 0, 0
        while True:
            byte = payload[offset]
            offset += 1
            value |= (byte & 127) << shift
            if byte < 128:
                return value
            shift += 7

    integer()  # cols
    integer()  # rows
    offset += 24  # six f32 geometry coordinates
    integer()  # viewport_kind
    integer()  # scrollback_offset_rows
    for _ in range(4):  # scrollbar: enum, total, viewport rows, top row
        integer()
    offset += 1  # scrollbar at_bottom
    offset += 8  # TerminalModeState: six booleans and two one-byte enum variants
    return integer()


class Client:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.socket.settimeout(5)
        self.socket.connect(path)
        body = b'{"selectors":[]}'
        self.socket.sendall(
            b'POST /connect HTTP/1.1\r\nHost: cleat\r\n'
            b'Content-Type: application/json\r\nConnection: Upgrade\r\n'
            b'Upgrade: cleat-packet/1\r\nContent-Length: '
            + str(len(body)).encode()
            + b'\r\nx-cleat-output-context: {"version":1,"context":{"kind":"remote"}}\r\n\r\n' + body
        )
        head = bytearray()
        while not head.endswith(b'\r\n\r\n'):
            head.extend(self.exact(1))
            if len(head) > 65536:
                raise ValueError('oversized HTTP response')
        if not head.startswith(b'HTTP/1.1 101 '):
            raise ValueError(bytes(head))
        channel, kind, hello = self.read()
        if (channel, kind, hello) != (0, 1, b'\x0b\x0b'):
            raise ValueError(('incompatible hello', channel, kind, hello))
        channel, kind, self.directory = self.read()
        if (channel, kind) != (0, 2):
            raise ValueError('missing directory snapshot')

    def exact(self, size):
        result = bytearray()
        while len(result) < size:
            chunk = self.socket.recv(size - len(result))
            if not chunk:
                raise EOFError('cleat connection closed')
            result.extend(chunk)
        return bytes(result)

    def read(self):
        channel, kind, size = struct.unpack('<IBI', self.exact(9))
        if size > 4 * 1024 * 1024:
            raise ValueError('oversized packet')
        return channel, kind, self.exact(size)

    def write(self, channel, kind, payload):
        self.socket.sendall(struct.pack('<IBI', channel, kind, len(payload)) + payload)

    def open(self, session):
        # OpenChannel { channel:1, session_id, role:Controller, take:false,
        # identity:{ kind:Principal, name:"tender-proof" } } in postcard.
        self.write(0, 4, b'\x01' + string(session) + b'\x01\x00\x00' + string('tender-proof'))

    def input(self, data):
        # TerminalInputEvent::RawBytes is variant 6; bytes is a Vec<u8>.
        self.write(1, 18, b'\x06' + varint(len(data)) + data)

    def render(self):
        while True:
            channel, kind, payload = self.read()
            if kind == 6:
                raise ValueError(('cleat control error', payload))
            if (channel, kind) == (1, 16):
                self.write(1, 17, varint(render_generation(payload)))
                return payload

    def close(self):
        self.socket.close()


if __name__ == '__main__':
    client = Client(sys.argv[1])
    try:
        client.open(sys.argv[2])
        initial = client.render()
        result = {'directory_bytes': len(client.directory), 'initial_render_bytes': len(initial)}
        if len(sys.argv) > 3:
            client.input(sys.argv[3].encode())
            result['updated_render_bytes'] = len(client.render())
        print(json.dumps(result))
    finally:
        client.close()
