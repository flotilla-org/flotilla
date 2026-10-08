#!/usr/bin/env -S uv run --with websockets python
"""Live vessel-daemon identity acceptance; requires logged-in CODEX_HOME and cleat.

Uses actual Codex tool execution, no mocked model output or committed recordings.
The private daemon is always stopped and the scratch auth is removed.
"""
import asyncio
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import uuid

import websockets


class Client:
    def __init__(self, socket):
        self.socket = socket
        self.counter = 0
        self.events = []
        self.pending = {}

    async def connect(self):
        self.ws = await websockets.unix_connect(self.socket)
        self.reader = asyncio.create_task(self.read())
        await self.request('initialize', {'clientInfo': {'name': 'flotilla-identity-proof', 'version': '1'}})
        await self.ws.send(json.dumps({'method': 'initialized'}))
        return self

    async def read(self):
        async for message in self.ws:
            value = json.loads(message)
            if 'id' in value and ('result' in value or 'error' in value):
                waiter = self.pending.pop(value['id'], None)
                if waiter:
                    waiter.set_result(value)
            else:
                self.events.append(value)

    async def request(self, method, params):
        self.counter += 1
        waiter = asyncio.get_running_loop().create_future()
        self.pending[self.counter] = waiter
        await self.ws.send(json.dumps({'id': self.counter, 'method': method, 'params': params}))
        response = await asyncio.wait_for(waiter, 30)
        if 'error' in response:
            raise RuntimeError(f'{method}: {response["error"]}')
        return response['result']

    async def close(self):
        await self.ws.close()
        try:
            await self.reader
        except websockets.exceptions.ConnectionClosed:
            pass


def command(args, env):
    result = subprocess.run(args, env=env, text=True, capture_output=True, check=True)
    return json.loads(result.stdout) if result.stdout.strip().startswith('{') else result.stdout


async def prove(root, env, codex):
    server = 'identity-' + uuid.uuid4().hex
    sessions = []
    client = None
    def lifecycle(verb):
        return command([codex, 'app-server', 'daemon', verb], env)
    try:
        (root / 'app-server-daemon').mkdir()
        (root / 'app-server-daemon/settings.json').write_text(json.dumps({'remoteControlEnabled': False, 'shutdownGraceSeconds': 2, 'updater': {'autoUpdateEnabled': False}}))
        info = lifecycle('bootstrap')
        print('BOOTSTRAP', json.dumps(info))
        pid_files = list(root.rglob('*.pid'))
        assert pid_files, 'bootstrap did not publish a PID record'
        pid_path = next((p for p in pid_files if 'updater' not in p.name), pid_files[0])
        original_pid = pid_path.read_text()
        second = lifecycle('start')
        assert pid_path.read_text() == original_pid, 'idempotent start changed PID'
        socket = info.get('socketPath') or second.get('socketPath')
        if not socket:
            raise RuntimeError(f'lifecycle JSON socket key unknown: {list(info)}')
        client = await Client(socket).connect()
        crews = []
        for index in range(2):
            identity = {'FLOTILLA_CREW_ID': f'proof-crew-{index}', 'FLOTILLA_CREW_ROLE': f'proof-role-{index}', 'FLOTILLA_TERMINAL_SESSION': f'proof-terminal-{index}'}
            config = {'shell_environment_policy.set': identity}
            thread = await client.request('thread/start', {'cwd': str(root), 'model': os.environ.get('CODEX_TRANSPORT_MODEL', 'gpt-6.1-sol'), 'approvalPolicy': 'never', 'sandbox': 'danger-full-access', 'config': config})
            crews.append((thread['thread']['id'], identity, config))

        async def phase(name):
            async def one(thread, identity, config):
                output = root / f'{name}-{identity["FLOTILLA_CREW_ID"]}.json'
                program = 'import os,json,pathlib;pathlib.Path(' + repr(str(output)) + ').write_text(json.dumps({k:os.environ.get(k) for k in ' + repr(list(identity)) + '}))'
                shell = 'python3 -c ' + shlex.quote(program)
                start_events = len(client.events)
                started = await client.request('turn/start', {'threadId': thread, 'input': [{'type': 'text', 'text': 'Run exactly this shell command using your command execution tool, then stop. Do not write the output yourself or use apply_patch. Command:\n' + shell}]})
                for _ in range(1800):
                    completed = any(e.get('method') == 'turn/completed' and e.get('params', {}).get('threadId') == thread and e.get('params', {}).get('turn', {}).get('id') == started['turn']['id'] for e in client.events[start_events:])
                    if completed:
                        if not output.exists():
                            outcomes = [e.get('params', {}).get('turn', {}) for e in client.events[start_events:] if e.get('method') == 'turn/completed' and e.get('params', {}).get('threadId') == thread]
                            print('FAILED TURN', json.dumps([{k: t.get(k) for k in ['id', 'status', 'error']} for t in outcomes]))
                            for e in client.events[start_events:]:
                                item = e.get('params', {}).get('item', {})
                                if e.get('params', {}).get('threadId') == thread and item.get('type') == 'commandExecution':
                                    print('FAILED COMMAND', json.dumps({k:item.get(k) for k in ['command', 'status', 'exitCode', 'aggregatedOutput']}))
                            raise AssertionError(f'{name}: no actual command output for {thread}')
                        assert json.loads(output.read_text()) == identity, f'{name}: wrong shell identity'
                        commands = [e for e in client.events[start_events:] if e.get('method') == 'item/completed' and e.get('params', {}).get('threadId') == thread and e.get('params', {}).get('item', {}).get('type') == 'commandExecution']
                        assert commands, f'{name}: no commandExecution evidence'
                        assert any('python3 -c' in e['params']['item'].get('command', '') for e in commands), f'{name}: no Python shell execution'
                        return
                    await asyncio.sleep(.1)
                raise TimeoutError(f'{name}: turn did not finish')
            await asyncio.gather(*(one(*crew) for crew in crews))
            print(f'PASS {name}: two concurrent threads, correct shell identities, actual command events')

        await phase('initial')
        for thread, identity, config in crews:
            session = identity['FLOTILLA_CREW_ID']
            attach = shlex.join([codex, '--remote', 'unix://' + socket, '-c', 'shell_environment_policy.set=' + '{' + ','.join(k + '=' + json.dumps(v) for k, v in identity.items()) + '}', 'resume', thread])
            subprocess.run(['cleat', 'launch', session, '--server', server, '--tag', 'purpose=probe', '--size', '160x40', '--cwd', str(root), '--env', 'CODEX_HOME=' + str(root), '--cmd', attach], env=env, check=True, capture_output=True)
            sessions.append(session)
        await asyncio.sleep(5)
        for session in sessions:
            subprocess.run(['cleat', 'inspect', session, '--server', server], env=env, check=True, capture_output=True)
        await phase('tui-attached')
        for thread, identity, config in crews:
            await client.request('thread/resume', {'threadId': thread, 'config': config, 'approvalPolicy': 'never', 'sandbox': 'danger-full-access'})
        await phase('resumed')
        await client.close()
        client = await Client(socket).connect()
        for thread, identity, config in crews:
            await client.request('thread/resume', {'threadId': thread, 'config': config, 'approvalPolicy': 'never', 'sandbox': 'danger-full-access'})
        await phase('reconnected')
        for session in sessions:
            subprocess.run(['cleat', 'kill', session, '--server', server], env=env, check=True, capture_output=True)
        sessions.clear()
        await client.close()
        client = None
        lifecycle('restart')
        client = await Client(socket).connect()
        for thread, identity, config in crews:
            await client.request('thread/resume', {'threadId': thread, 'config': config, 'approvalPolicy': 'never', 'sandbox': 'danger-full-access'})
        await phase('restarted')
        print('PASS managed daemon lifecycle and per-thread crew identity acceptance')
    finally:
        if client:
            await client.close()
        for session in sessions:
            subprocess.run(['cleat', 'kill', session, '--server', server], env=env, capture_output=True)
        lifecycle('stop')


def main():
    source = Path(os.environ['CODEX_HOME'])
    codex = os.environ.get('CODEX_BIN', shutil.which('codex'))
    with tempfile.TemporaryDirectory(prefix='flotilla-identity-') as directory:
        root = Path(directory)
        shutil.copyfile(source / 'auth.json', root / 'auth.json')
        (root / 'auth.json').chmod(0o600)
        env = dict(os.environ, CODEX_HOME=str(root))
        # Seed deliberately wrong daemon identity: thread overrides must win.
        env.update(FLOTILLA_CREW_ID='daemon-not-a-crew', FLOTILLA_CREW_ROLE='daemon', FLOTILLA_TERMINAL_SESSION='daemon')
        asyncio.run(prove(root, env, codex))


if __name__ == '__main__':
    main()
