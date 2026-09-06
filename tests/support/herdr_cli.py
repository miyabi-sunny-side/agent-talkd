#!/usr/bin/env python3
"""Isolated CLI fixture; models only the commands consumed by agent-talk."""
import json
import pathlib
import sys
import time

root = pathlib.Path(__file__).parent
state = json.loads((root / 'state.json').read_text())
args = sys.argv[1:]
with (root / 'calls.jsonl').open('a') as calls:
    calls.write(json.dumps(args) + '\n')
row = state['row']
command = args[:2]
mode = state.get('mode', '')
if command == ['agent', 'prompt'] and mode:
    if mode == 'timeout':
        time.sleep(30)
    if mode == 'oversized':
        sys.stdout.write('x' * (4 * 1024 * 1024 + 1))
        sys.exit(0)
    if mode == 'stderr-flood':
        sys.stderr.write('x' * (64 * 1024 + 1))
        sys.exit(1)
    if mode == 'invalid-json':
        print('not json')
        sys.exit(0)
    if mode == 'exit-failure':
        print(json.dumps({'result': {'type': 'agent_prompted', 'agent': row}}))
        sys.exit(2)
    if mode == 'blocked':
        print(json.dumps({'error': {'code': 'agent_blocked', 'message': 'blocked'}}), file=sys.stderr)
        sys.exit(1)
if command == ['agent', 'list']:
    assert args == ['agent', 'list']
    result = {'agents': [row]}
elif command == ['workspace', 'list']:
    assert args == ['workspace', 'list']
    result = {'workspaces': []}
elif command == ['tab', 'list']:
    assert args == ['tab', 'list', '--workspace', 'w1']
    result = {'tabs': []}
elif command == ['agent', 'get']:
    assert args == ['agent', 'get', 'w1:p2']
    result = {'agent': row}
elif command == ['pane', 'process-info']:
    assert args == ['pane', 'process-info', '--pane', 'w1:p2']
    result = {'type': 'pane_process_info', 'process_info': {'pane_id': 'w1:p2', 'foreground_processes': [{'pid': state.get('pid', 123), 'name': 'codex'}]}}
elif command == ['pane', 'read']:
    assert args == ['pane', 'read', 'w1:p2', '--source', 'visible', '--format', 'text']
    screen_mode = state.get('screen_mode', '')
    if screen_mode == 'restart':
        row['agent_session']['value'] = 'session-b'
    elif screen_mode == 'terminal-restart':
        row['terminal_id'] = 'terminal-2'
    elif screen_mode == 'ended':
        row['agent_session'] = None
    (root / 'state.json').write_text(json.dumps(state))
    sys.stdout.write('x' * (256 * 1024 + 1) if screen_mode == 'oversized' else '確認してください\n[許可] [拒否]')
    sys.exit(0)
elif command == ['agent', 'prompt']:
    assert len(args) == 4 and args[2] == 'w1:p2'
    result = {'type': 'agent_prompted', 'agent': row}
    if mode == 'wrong-terminal':
        result['agent']['terminal_id'] = 'other-terminal'
else:
    raise AssertionError(args)
print(json.dumps({'result': result}))
