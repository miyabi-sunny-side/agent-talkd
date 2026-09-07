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
rows = [row, *state.get('extra_rows', [])]
if command in (['agent', 'get'], ['agent', 'prompt'], ['pane', 'read']):
    row = next(candidate for candidate in rows if candidate['pane_id'] == args[2])
elif command == ['pane', 'process-info']:
    row = next(candidate for candidate in rows if candidate['pane_id'] == args[3])
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
    result = {'agents': rows}
elif command == ['workspace', 'list']:
    assert args == ['workspace', 'list']
    result = {'workspaces': []}
elif command == ['tab', 'list']:
    assert args[:3] == ['tab', 'list', '--workspace'] and args[3] in {r['workspace_id'] for r in rows}
    result = {'tabs': []}
elif command == ['agent', 'get']:
    assert args == ['agent', 'get', row['pane_id']]
    result = {'agent': dict(row, pane_id=state.get('get_pane_id', row['pane_id']))}
elif command == ['pane', 'process-info']:
    assert args == ['pane', 'process-info', '--pane', row['pane_id']]
    result = {'type': 'pane_process_info', 'process_info': {'pane_id': row['pane_id'], 'foreground_processes': [{'pid': state.get('pid', 123), 'name': 'codex'}]}}
elif command == ['pane', 'read']:
    assert args == ['pane', 'read', row['pane_id'], '--source', 'visible', '--format', 'text']
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
    assert len(args) == 4 and args[2] == row['pane_id']
    result = {'type': 'agent_prompted', 'agent': row}
    if mode == 'wrong-terminal':
        result['agent']['terminal_id'] = 'other-terminal'
else:
    raise AssertionError(args)
print(json.dumps({'result': result}))
