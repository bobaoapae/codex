"""Drive `codex app-server` through a Plan-mode session and log everything.

Usage: python codex_driver.py <out_dir> <model> <effort> <task_file> [followup_file] [cwd]
Every request_user_input question is answered with its FIRST option (the
prompt tells the model to put the recommended option first).
"""
import json, os, subprocess, sys, threading, time, queue

out_dir, model, effort, task_file = sys.argv[1:5]
followup_file = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] != '-' else None
cwd = sys.argv[6] if len(sys.argv) > 6 else r'C:\Users\Joao\RustProjects\codex'
os.makedirs(out_dir, exist_ok=True)
task = open(task_file, encoding='utf-8').read().strip()
followup = open(followup_file, encoding='utf-8').read().strip() if followup_file else None

TOTAL_TIMEOUT = float(os.environ.get('SIM_TOTAL_TIMEOUT', 3600))
IDLE_TIMEOUT = float(os.environ.get('SIM_IDLE_TIMEOUT', 1200))

log = open(os.path.join(out_dir, 'events.jsonl'), 'w', encoding='utf-8')
summary = {
    'harness': 'codex', 'model': model, 'effort': effort, 'cwd': cwd,
    'rounds': [], 'questions': [], 'started_at': time.time(),
}

def L(kind, obj):
    log.write(json.dumps({'t': round(time.time() - summary['started_at'], 1), 'kind': kind, 'obj': obj}, ensure_ascii=False) + '\n')
    log.flush()

codex = os.environ.get('CODEX_BIN', 'codex')
proc = subprocess.Popen([codex, 'app-server'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=open(os.path.join(out_dir, 'stderr.txt'), 'w', encoding='utf-8'),
                        cwd=cwd, text=True, encoding='utf-8', bufsize=1)
q = queue.Queue()

def reader():
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        try:
            q.put(json.loads(line))
        except Exception:
            L('unparsed', line)
    q.put(None)

threading.Thread(target=reader, daemon=True).start()
_id = 0

def send(obj):
    L('send', obj)
    proc.stdin.write(json.dumps(obj, ensure_ascii=False) + '\n')
    proc.stdin.flush()

def request(method, params):
    global _id
    _id += 1
    send({'method': method, 'id': _id, 'params': params})
    return _id

def wait_response(rid, timeout=120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            msg = q.get(timeout=1)
        except queue.Empty:
            continue
        if msg is None:
            raise SystemExit('app-server exited')
        L('recv', msg)
        if msg.get('id') == rid and 'method' not in msg:
            return msg
        handle_async(msg)
    raise SystemExit(f'timeout waiting for response {rid}')

def answer_questions(params):
    answers = {}
    for qd in params.get('questions', []):
        opts = qd.get('options') or []
        pick = opts[0]['label'] if opts else 'Decide tu (recomendação).'
        answers[qd['id']] = {'answers': [pick]}
        summary['questions'].append({'round': len(summary['rounds']) + 1, 'id': qd['id'], 'header': qd.get('header'),
                                     'question': qd.get('question'), 'options': opts, 'picked': pick,
                                     't': round(time.time() - summary['started_at'], 1)})
    return {'answers': answers}

current = None  # per-round accumulator

def handle_async(msg):
    method = msg.get('method')
    if method and 'id' in msg:  # server -> client request
        if method == 'item/tool/requestUserInput':
            res = answer_questions(msg['params'])
            current['question_calls'] += 1
        elif method == 'item/commandExecution/requestApproval':
            res = {'decision': 'accept'}
        elif method == 'item/fileChange/requestApproval':
            res = {'decision': 'decline'}
        elif method == 'item/permissions/requestApproval':
            res = {'decision': 'decline'}
        else:
            res = {}
        send({'id': msg['id'], 'result': res})
        return
    if not method or current is None:
        return
    p = msg.get('params', {})
    tid = p.get('threadId')
    # Sub-agent threads (multi-agent) emit the same notifications; keep the
    # root thread's lifecycle separate and only count sub-agent work in bulk.
    if tid and summary.get('thread_id') and tid != summary['thread_id']:
        if method == 'item/completed':
            current['subagent_items'] = current.get('subagent_items', 0) + 1
            if p.get('item', {}).get('type') == 'commandExecution':
                current['subagent_commands'] = current.get('subagent_commands', 0) + 1
        elif method == 'turn/completed':
            current['subagent_turns_completed'] = current.get('subagent_turns_completed', 0) + 1
        elif method == 'thread/tokenUsage/updated':
            current['subagent_token_usage'] = current.get('subagent_token_usage', {})
            current['subagent_token_usage'][tid] = (p.get('tokenUsage') or {}).get('total')
        return
    if method == 'item/completed':
        item = p.get('item', {})
        t = item.get('type')
        current['items'].append(t)
        if t == 'subAgentActivity':
            current['subagent_activity'] = current.get('subagent_activity', 0) + 1
        if t == 'plan':
            current['plans'].append(item.get('text', ''))
        elif t == 'agentMessage':
            current['messages'].append(item.get('text', ''))
        elif t in ('commandExecution', 'mcpToolCall', 'dynamicToolCall', 'fileChange', 'webSearch', 'collabAgentToolCall', 'toolSearch'):
            current['tool_calls'] += 1
            if t == 'commandExecution':
                current['commands'].append(item.get('command'))
        elif t == 'reasoning':
            current['reasoning_items'] += 1
    elif method == 'thread/tokenUsage/updated':
        current['token_usage'] = p.get('tokenUsage') or p
    elif method == 'turn/completed':
        current['turn_status'] = p.get('turn', {}).get('status')
        current['done'] = True
    elif method == 'error':
        current['errors'].append(p)

def run_round(thread_id, text):
    global current
    current = {'text': text, 'items': [], 'plans': [], 'messages': [], 'tool_calls': 0, 'commands': [],
               'reasoning_items': 0, 'question_calls': 0, 'errors': [], 'done': False,
               'started': round(time.time() - summary['started_at'], 1)}
    rid = request('turn/start', {
        'threadId': thread_id,
        'input': [{'type': 'text', 'text': text}],
        'collaborationMode': {'mode': 'plan', 'settings': {'model': model, 'reasoning_effort': effort, 'developer_instructions': None}},
    })
    wait_response(rid)
    last = time.time()
    start = time.time()
    while not current['done']:
        if time.time() - start > TOTAL_TIMEOUT or time.time() - last > IDLE_TIMEOUT:
            current['errors'].append('timeout')
            break
        try:
            msg = q.get(timeout=5)
        except queue.Empty:
            continue
        if msg is None:
            current['errors'].append('app-server exited')
            break
        last = time.time()
        L('recv', msg)
        handle_async(msg)
    current['ended'] = round(time.time() - summary['started_at'], 1)
    current['duration_s'] = round(current['ended'] - current['started'], 1)
    summary['rounds'].append(current)
    json.dump(summary, open(os.path.join(out_dir, 'summary.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)

try:
    rid = request('initialize', {'clientInfo': {'name': 'plan_sim', 'title': 'Plan sim', 'version': '0.1'},
                                 'capabilities': {'experimentalApi': True}})
    wait_response(rid)
    send({'method': 'initialized'})
    rid = request('thread/start', {'cwd': cwd, 'model': model, 'approvalPolicy': 'never', 'sandbox': 'read-only'})
    resp = wait_response(rid)
    thread_id = resp['result']['thread']['id']
    summary['thread_id'] = thread_id
    run_round(thread_id, task)
    if followup:
        run_round(thread_id, followup)
finally:
    summary['ended_at'] = time.time()
    json.dump(summary, open(os.path.join(out_dir, 'summary.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=15)
    except Exception:
        proc.kill()
print('DONE', json.dumps({k: (len(v) if isinstance(v, list) else v) for k, v in summary.items() if k in ('rounds', 'questions')}))
