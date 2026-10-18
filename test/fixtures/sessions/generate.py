"""Writes the Claude Code transcripts and price file in test/fixtures/sessions.

Usage: python3 generate.py test/fixtures/sessions, then add the gpt-test-1 price the Codex
fixtures use. The expected files are `agt sessions` output for --root projects --since 2026-02-15, checked by hand.
"""
import json, os, sys
root = sys.argv[1]
def write(rel, records):
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'w') as fh:
        for r in records:
            fh.write((r if isinstance(r, str) else json.dumps(r)) + '\n')

def usage(inp, cw, cr, out, **extra):
    u = {'input_tokens': inp, 'cache_creation_input_tokens': cw, 'cache_read_input_tokens': cr, 'output_tokens': out}
    u.update(extra)
    return u

def asst(ts, mid, model, u, content=None):
    return {'type': 'assistant', 'timestamp': ts, 'message': {'id': mid, 'model': model, 'usage': u, 'content': content or []}}

def tool(tid, name, inp):
    return {'type': 'tool_use', 'id': tid, 'name': name, 'input': inp}

def result(ts, tid, content, err=False):
    return {'type': 'user', 'timestamp': ts, 'message': {'role': 'user', 'content': [{'type': 'tool_result', 'tool_use_id': tid, 'content': content, 'is_error': err}]}}

def user(ts, text, **extra):
    r = {'type': 'user', 'timestamp': ts, 'message': {'role': 'user', 'content': text}}
    r.update(extra)
    return r

O = 'claude-opus-4-8'
S = 'claude-sonnet-4-6'
write('prices.json', [json.dumps({'models': {
    'claude-opus-4': {'input': 15, 'write_5m': 18.75, 'write_1h': 30, 'read': 1.5, 'output': 75},
    'claude-opus-4-8': {'input': 5, 'write_5m': 6.25, 'write_1h': 10, 'read': 0.5, 'output': 25, 'fast_multiplier': 6},
    'claude-sonnet-4-6': {'input': 3, 'write_5m': 3.75, 'write_1h': 6, 'read': 0.3, 'output': 15,
                          'over': {'prompt_tokens': 200000, 'input': 6, 'write_5m': 7.5, 'write_1h': 12, 'read': 0.6, 'output': 22.5}},
}}, indent=1)])

main = [
    'not json',
    user('2026-03-01T10:00:00.000Z', 'please fix the build'),
    asst('2026-03-01T10:00:05.000Z', 'm1', O, usage(10, 20000, 0, 300, cache_creation={'ephemeral_5m_input_tokens': 5000, 'ephemeral_1h_input_tokens': 15000})),
    asst('2026-03-01T10:00:06.000Z', 'm1', O, usage(10, 20000, 0, 300, cache_creation={'ephemeral_5m_input_tokens': 5000, 'ephemeral_1h_input_tokens': 15000}),
         [tool('t1', 'Bash', {'command': 'cd /repo && FOO=1; npm test -- --watch=false'})]),
    result('2026-03-01T10:00:30.000Z', 't1', 'Exit code 1\nFAIL src/a.test.ts (12 tests)', True),
    asst('2026-03-01T10:00:40.000Z', 'm2', O, usage(5, 1000, 20000, 120, speed='fast'),
         [tool('t2', 'Read', {'file_path': '/home/u/.claude/skills/x/SKILL.md'}), tool('t3', 'mcp__slack__post', {'text': 'héllo 😀'})]),
    result('2026-03-01T10:00:41.000Z', 't2', [{'type': 'text', 'text': 'x' * 400}, {'type': 'image', 'source': {}}]),
    result('2026-03-01T10:00:42.000Z', 't3', 'Permission to use mcp__slack__post has been denied by the Claude Code auto mode classifier. Reason: [external] posting', True),
    {'type': 'system', 'subtype': 'compact_boundary', 'timestamp': '2026-03-01T10:01:00.000Z'},
    user('2026-03-01T10:20:00.000Z', 'ok'),
    asst('2026-03-01T10:20:10.000Z', 'm3', O, usage(7, 30000, 100, 50), [tool('t4', 'Bash', {'command': 'git status'})]),
    result('2026-03-01T10:20:11.000Z', 't4', "The user doesn't want to proceed with this tool use.", True),
    user('2026-03-01T10:21:00.000Z', '[Request interrupted by user for tool use]'),
    user('2026-03-01T10:22:00.000Z', 'run the tests again?'),
    user('2026-03-01T10:23:00.000Z', '<command-name>/review</command-name><command-args>--fix</command-args>'),
    user('2026-03-01T10:24:00.000Z', '<system-reminder>ignored</system-reminder>'),
    user('2026-03-01T10:25:00.000Z', 'meta', isMeta=True),
    asst('2026-03-01T10:40:00.000Z', 'm4', S, usage(250000, 0, 0, 10), [tool('t5', 'Edit', {'file_path': '/repo/a.ts'})]),
    result('2026-03-01T10:40:01.000Z', 't5', '<tool_use_error>String to replace not found in file 3 times</tool_use_error>', True),
    asst('2026-03-01T10:40:30.000Z', 'm4b', S, usage(9, 0, 1000, 10), [tool('t6', 'Edit', {'file_path': '/repo/b.ts'}), tool('t7', 'Bash', {'command': 'rm -rf build'})]),
    result('2026-03-01T10:40:31.000Z', 't6', '<tool_use_error>String to replace not found in file 12 times</tool_use_error>', True),
    result('2026-03-01T10:40:32.000Z', 't7', "The user doesn't want to proceed with this tool use.", True),
    asst('2026-03-01T10:41:00.000Z', 'm5', '<synthetic>', usage(1, 1, 1, 1)),
    asst('2026-03-01T10:42:00.000Z', 'm6', 'claude-mystery-1', usage(100, 0, 0, 10)),
    user('2026-02-01T09:00:00.000Z', 'before the window'),
]
write('projects/-repo-app/aaaaaaaa-1111.jsonl', main)
write('projects/-repo-app/aaaaaaaa-1111/subagents/agent-x1.jsonl', [
    asst('2026-03-01T10:05:00.000Z', 'sub1', S, usage(3, 8000, 0, 40), [tool('s1', 'Grep', {'pattern': 'foo'})]),
    result('2026-03-01T10:05:01.000Z', 's1', 'ENOENT: no such file', True),
    asst('2026-03-01T10:05:05.000Z', 'sub1b', S, usage(3, 50, 8000, 40), [tool('s2', 'Grep', {'pattern': 'bar'}), tool('s3', 'Grep', {'pattern': 'baz'})]),
    result('2026-03-01T10:05:06.000Z', 's2', 'ENOENT: no such file', True),
    result('2026-03-01T10:05:07.000Z', 's3', 'ENOENT: no such file', True),
    asst('2026-03-01T10:05:09.000Z', 'sub2', S, usage(3, 100, 8000, 400)),
])
write('projects/-repo-app/aaaaaaaa-1111/subagents/agent-x1.meta.json', [json.dumps({'agentType': 'explorer'})])
write('projects/-repo-app/aaaaaaaa-1111/subagents/agent-x2.jsonl', [
    asst('2026-03-01T10:06:00.000Z', 'sub3', S, usage(3, 12000, 0, 90)),
])
write('projects/-repo-app/aaaaaaaa-1111/subagents/agent-x2.meta.json', [json.dumps({'agentType': 'explorer'})])
write('projects/-repo-web/bbbbbbbb-2222.jsonl', [
    user('2026-03-02T08:00:00.000Z', 'ok'),
    user('2026-03-02T08:00:30.000Z', 'did you check the logs'),
    asst('2026-03-02T08:01:00.000Z', 'w1', S, usage(2, 4000, 0, 70)),
    user('2026-03-02T08:30:00.000Z', 'continue'),
])
write('projects/-repo-docs/cccccccc-3333.jsonl', [
    user('2026-03-03T08:00:00.000Z', 'ok'),
    user('2026-03-03T08:00:10.000Z', 'did you check the logs'),
    user('2026-03-03T08:00:20.000Z', 'continue'),
    asst('2026-03-03T08:01:00.000Z', 'd1', O, usage(1, 2000, 0, 15)),
])
write('projects/-repo-ops/dddddddd-4444.jsonl', [
    user('2026-03-04T08:00:00.000Z', 'did you check the logs'),
    user('2026-03-04T08:00:05.000Z', 'ok'),
    asst('2026-03-04T08:01:00.000Z', 'o1', O, usage(1, 900, 0, 9)),
])
write('projects/-tmp-probe/eeeeeeee-5555.jsonl', [
    asst('2026-03-01T11:00:00.000Z', 'p1', O, usage(1, 100, 0, 5)),
])
