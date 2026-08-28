"""Drive Claude Code headless (`--permission-mode plan`) and log everything.

Usage: python claude_driver.py <out_dir> <model> <effort> <task_file> [followup_file] [cwd]
AskUserQuestion is answered with its FIRST option. ExitPlanMode #1 is denied
with the follow-up text (so the model revises); ExitPlanMode #2 ends the run.
"""
import json, os, subprocess, sys, threading, time, queue, glob

out_dir, model, effort, task_file = sys.argv[1:5]
followup_file = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] != '-' else None
cwd = sys.argv[6] if len(sys.argv) > 6 else r'C:\Users\Joao\RustProjects\codex'
os.makedirs(out_dir, exist_ok=True)
task = open(task_file, encoding='utf-8').read().strip()
followup = open(followup_file, encoding='utf-8').read().strip() if followup_file else None
TOTAL_TIMEOUT = float(os.environ.get('SIM_TOTAL_TIMEOUT', 3600))
IDLE_TIMEOUT = float(os.environ.get('SIM_IDLE_TIMEOUT', 1200))
RESULT_IDLE = float(os.environ.get('SIM_RESULT_IDLE', 900))
PLANS_DIR = os.path.join(os.environ.get('CLAUDE_CONFIG_DIR', os.path.expanduser('~/.claude')), 'plans')
plan_file_paths = []  # file paths the model wrote to (Write/Edit tool_use blocks)

log = open(os.path.join(out_dir, 'events.jsonl'), 'w', encoding='utf-8')
summary = {'harness': 'claude_code', 'model': model, 'effort': effort, 'cwd': cwd, 'rounds': [], 'questions': [],
           'started_at': time.time(), 'exit_plan_calls': 0}

def L(kind, obj):
    log.write(json.dumps({'t': round(time.time() - summary['started_at'], 1), 'kind': kind, 'obj': obj}, ensure_ascii=False) + '\n')
    log.flush()

claude = os.environ.get('CLAUDE_BIN', r'C:\Users\Joao\.local\bin\claude.exe')
args = [claude, '--print', '--verbose', '--input-format', 'stream-json', '--output-format', 'stream-json',
        '--model', model, '--effort', effort, '--permission-mode', 'plan', '--permission-prompt-tool', 'stdio']
proc = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
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

def send(obj):
    L('send', obj)
    proc.stdin.write(json.dumps(obj, ensure_ascii=False) + '\n')
    proc.stdin.flush()

def user_msg(text):
    send({'type': 'user', 'message': {'role': 'user', 'content': text}})

def control_reply(request_id, response):
    send({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': request_id, 'response': response}})

def newest_plan_file(after):
    files = [f for f in glob.glob(os.path.join(PLANS_DIR, '*.md')) if os.path.getmtime(f) >= after - 5]
    files += [f for f in plan_file_paths if os.path.isfile(f)]
    if not files:
        return None, None
    f = max(files, key=os.path.getmtime)
    return f, open(f, encoding='utf-8').read()

current = None
stop = False

def new_round(text):
    global current
    current = {'text': text, 'assistant_texts': [], 'tool_calls': 0, 'tool_names': {}, 'question_calls': 0,
               'subagents': 0, 'plans': [], 'errors': [], 'done': False, 'started': round(time.time() - summary['started_at'], 1)}
    summary['rounds'].append(current)

def finish_round(reason):
    current['done'] = True
    current['end_reason'] = reason
    current['ended'] = round(time.time() - summary['started_at'], 1)
    current['duration_s'] = round(current['ended'] - current['started'], 1)
    json.dump(summary, open(os.path.join(out_dir, 'summary.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)

def handle(msg):
    global stop
    t = msg.get('type')
    if t == 'control_request':
        req = msg.get('request', {})
        rid = msg.get('request_id')
        if req.get('subtype') != 'can_use_tool':
            control_reply(rid, {'behavior': 'allow'})
            return
        tool = req.get('tool_name')
        inp = req.get('input') or {}
        if tool == 'AskUserQuestion':
            answers = {}
            for qd in inp.get('questions', []):
                opts = qd.get('options') or []
                pick = opts[0]['label'] if opts else 'Decide tu (recomendação).'
                answers[qd.get('question')] = pick
                summary['questions'].append({'round': len(summary['rounds']), 'header': qd.get('header'), 'question': qd.get('question'),
                                             'options': opts, 'multiSelect': qd.get('multiSelect'), 'picked': pick,
                                             't': round(time.time() - summary['started_at'], 1)})
            current['question_calls'] += 1
            control_reply(rid, {'behavior': 'allow', 'updatedInput': {**inp, 'answers': answers}})
        elif tool == 'ExitPlanMode':
            summary['exit_plan_calls'] += 1
            f, content = newest_plan_file(summary['started_at'])
            current['plans'].append({'file': f, 'content': content, 'input_plan': inp.get('plan')})
            if followup and summary['exit_plan_calls'] == 1:
                finish_round('exit_plan_mode_denied_with_followup')
                new_round(followup)
                control_reply(rid, {'behavior': 'deny', 'message': followup, 'interrupt': False})
            else:
                control_reply(rid, {'behavior': 'deny', 'message': 'Plano recebido. Fim da sessão de simulação — não implementes nada.', 'interrupt': True})
                finish_round('exit_plan_mode_final')
                stop = True
        elif tool in ('Write', 'Edit', 'MultiEdit', 'NotebookEdit'):
            path = str(inp.get('file_path', ''))
            if os.path.normpath(PLANS_DIR).lower() in os.path.normpath(path).lower():
                control_reply(rid, {'behavior': 'allow', 'updatedInput': inp})
            else:
                control_reply(rid, {'behavior': 'deny', 'message': 'Plan mode: only the plan file may be written.', 'interrupt': False})
        else:
            control_reply(rid, {'behavior': 'allow', 'updatedInput': inp})
        return
    if current is None:
        return
    if t == 'assistant':
        current['last_result_t'] = None  # the model woke up again (e.g. background task finished)
        for block in (msg.get('message') or {}).get('content', []):
            if block.get('type') == 'text' and block.get('text'):
                current['assistant_texts'].append(block['text'])
            elif block.get('type') == 'tool_use':
                current['tool_calls'] += 1
                name = block.get('name')
                current['tool_names'][name] = current['tool_names'].get(name, 0) + 1
                if name in ('Agent', 'Task'):
                    current['subagents'] += 1
                if name in ('Write', 'Edit') and (block.get('input') or {}).get('file_path'):
                    fp = block['input']['file_path']
                    if fp not in plan_file_paths:
                        plan_file_paths.append(fp)
    elif t == 'result':
        # A `result` closes a model turn, but in plan mode the session stays alive:
        # background subagents finishing wake the model again (task notifications).
        # Only stop once ExitPlanMode has been seen for the final round, or after
        # RESULT_IDLE seconds of silence following a result.
        current.setdefault('results', []).append({k: msg.get(k) for k in ('subtype', 'duration_ms', 'num_turns', 'total_cost_usd', 'usage', 'modelUsage', 'is_error', 'permission_denials')})
        current['result'] = current['results'][-1]
        current['last_result_t'] = time.time()
        if summary['exit_plan_calls'] >= (2 if followup else 1):
            if not current['done']:
                finish_round('result')
            stop = True
    elif t == 'system' and msg.get('subtype') == 'task_started':
        current['background_tasks'] = current.get('background_tasks', 0) + 1

try:
    new_round(task)
    user_msg(task)
    start = time.time(); last = time.time()
    while not stop:
        if time.time() - start > TOTAL_TIMEOUT or time.time() - last > IDLE_TIMEOUT:
            current['errors'].append('timeout'); break
        if current.get('last_result_t') and time.time() - current['last_result_t'] > RESULT_IDLE and summary['exit_plan_calls'] == 0:
            current['errors'].append('ended_without_exit_plan_mode'); break
        try:
            msg = q.get(timeout=5)
        except queue.Empty:
            continue
        if msg is None:
            current['errors'].append('claude exited'); break
        last = time.time()
        L('recv', msg)
        handle(msg)
finally:
    if current and not current.get('done'):
        finish_round('aborted')
    summary['ended_at'] = time.time()
    json.dump(summary, open(os.path.join(out_dir, 'summary.json'), 'w', encoding='utf-8'), ensure_ascii=False, indent=2)
    try:
        proc.stdin.close()
    except Exception:
        pass
    try:
        proc.wait(timeout=20)
    except Exception:
        proc.kill()
print('DONE rounds=%d questions=%d exit_plan_calls=%d' % (len(summary['rounds']), len(summary['questions']), summary['exit_plan_calls']))
