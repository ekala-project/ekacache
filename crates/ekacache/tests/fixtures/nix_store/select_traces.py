"""Select the builder_rpc ITF traces replayed by crates/ekacache/tests/nix_store_quint.rs.

usage: python3 select_traces.py <dir with quint's trace*.itf.json> <output dir>

Each trace is reduced to its daemon behaviour per client state (the effective pick of
`builder_rpc_mbt.qnt`'s `mostlyGood`: a well-behaved step when `g != 0`, else the picks `i`,
`m`, `w`). A greedy cover then keeps a few traces that together contain every (state,
behaviour) pair and every (verb, exit code) and (verb, ops completed) outcome. Each kept trace
is cut after its first `Exited` state (the rest is stuttering) and written compactly.
"""
import glob
import json
import os
import sys

WRITES = {'SendHello', 'SendFeatures', 'SendPost', 'SendAdd', 'SendSubmit'}
READS = {'AwaitMagic', 'AwaitVersion', 'AwaitFeatures', 'AwaitInfo', 'AwaitPathInfo',
         'AwaitSubmitResult'}
DRAINS = {'DrainStartup', 'DrainAdd', 'DrainSubmit'}


def raw(picks, k):
    x = picks.get(k)
    if x is None or x['tag'] == 'None':
        return None
    v = x['value']
    if isinstance(v, dict) and 'tag' in v:
        return v['tag']
    if isinstance(v, dict) and '#bigint' in v:
        return int(v['#bigint'])
    return v


def pick(picks, k):
    g = raw(picks, 'g')
    if g:
        return {'i': 'Good', 'w': True,
                'm': 'Next' if g == 1 else 'Activity' if g == 2 else 'Last'}[k]
    return raw(picks, k)


def seq(path):
    return int(os.path.basename(path)[5:].split('.')[0])


def analyse(path):
    with open(path) as f:
        t = json.load(f)
    states = t['states']
    sig = []
    feats = set()
    end = len(states)
    for k in range(1, len(states)):
        pre = states[k - 1]['st']['tag']
        if pre == 'Exited':
            end = k
            break
        picks = states[k]['mbt::nondetPicks']
        if pre in WRITES:
            b = ('w', pick(picks, 'w'))
        elif pre in READS:
            b = ('i', pick(picks, 'i'))
        elif pre in DRAINS:
            b = ('m', pick(picks, 'm'))
        else:
            b = None
        sig.append((pre, b))
        feats.add((pre, b))
    last = states[end - 1]
    verb = states[0]['verb']['tag']
    n = int(states[0]['fetchLeft']['#bigint'])
    feats.add(('exit', verb, int(last['exitCode']['#bigint'])))
    feats.add(('ops', verb, int(last['opsCompleted']['#bigint'])))
    return dict(path=path, key=(verb, n, tuple(sig)), feats=feats, end=end, trace=t)


def main(src, out):
    traces = [analyse(p) for p in sorted(glob.glob(src + '/trace*.itf.json'), key=seq)]
    distinct = {}
    for tr in traces:
        distinct.setdefault(tr['key'], tr)
    candidates = list(distinct.values())
    wanted = set().union(*(tr['feats'] for tr in candidates))
    chosen = []
    covered = set()
    while covered != wanted:
        best = max(candidates, key=lambda tr: (len(tr['feats'] - covered), -len(tr['key'][2]),
                                               -seq(tr['path'])))
        chosen.append(best)
        covered |= best['feats']
    os.makedirs(out, exist_ok=True)
    for f in glob.glob(out + '/*.itf.json'):
        os.remove(f)
    for j, tr in enumerate(sorted(chosen, key=lambda tr: seq(tr['path']))):
        t = tr['trace']
        t['states'] = t['states'][:tr['end']]
        with open('%s/trace%02d.itf.json' % (out, j), 'w') as f:
            f.write(json.dumps(t, separators=(',', ':')) + '\n')
    print('%d distinct traces, %d kept, %d features' % (len(distinct), len(chosen), len(wanted)))


main(sys.argv[1], sys.argv[2])
