#!/usr/bin/env python3
# Generates the heal_event appendix of LOG_REFERENCE.md from the sources:
#   scripts/gen-log-events.py <repo root>  > /tmp/events.md
# Re-run at each release (line numbers move).
import re, sys, os, collections
root = sys.argv[1]
ev = collections.OrderedDict()
for fn in sorted(os.listdir(os.path.join(root, 'src'))):
    if not fn.endswith('.rs'): continue
    path = os.path.join(root, 'src', fn)
    text = open(path).read()
    # stop at the test module to skip test-only lines
    cut = text.find('#[cfg(test)]\nmod tests')
    lines = text.split('\n')
    limit = text[:cut].count('\n') + 1 if cut >= 0 else len(lines)
    for i, line in enumerate(lines[:limit]):
        for m in re.finditer(r'heal_event = (?:if [^{]*\{ )?"([a-z0-9_]+)"', line):
            name = m.group(1)
            # level and message: scan back to the macro start, forward to the first string literal after the fields
            j = i
            while j > 0 and not re.search(r'\b(info|warn|error|debug)!\(', lines[j]): j -= 1
            lvl = re.search(r'\b(info|warn|error|debug)!\(', lines[j])
            lvl = lvl.group(1).upper() if lvl else '?'
            # the macro call from its start to its closing ");" (bounded)
            k = i; depth_lines = []
            while k < min(len(lines), i + 25):
                depth_lines.append(lines[k].strip())
                if lines[k].rstrip().endswith(');'): break
                k += 1
            blob = ' '.join(depth_lines)
            blob = re.sub(r'"\s*\\?\s+"', '', blob)
            msgs = re.findall(r'"((?:[^"\\]|\\.)*)"', blob)
            msg = ''
            for cand in reversed(msgs):
                c = cand.replace('\\', '')
                if c.count(' ') >= 3 and '{:' not in c:
                    msg = c.strip(); break
            msg = re.sub(r'\s+', ' ', msg)
            if len(msg) > 110: msg = msg[:107] + '...'
            st = re.search(r'\bstep = "([a-z0-9_]+)"', blob)
            key = f"{name} step={st.group(1)}" if st else name
            ev.setdefault(key, []).append((f"{fn}:{i+1}", lvl, msg))
print("| heal_event (step) | level | where (file:line) | message (first occurrence) |")
print("|---|---|---|---|")
for name in sorted(ev):
    occ = ev[name]
    where = ', '.join(o[0] for o in occ[:6]) + (f" (+{len(occ)-6})" if len(occ) > 6 else '')
    lvls = '/'.join(sorted(set(o[1] for o in occ)))
    msg = next((o[2] for o in occ if o[2]), '').replace('|', '\\|')
    shown = name.replace(" step=", "` step=`") 
    print(f"| `{shown}` | {lvls} | {where} | {msg} |")
