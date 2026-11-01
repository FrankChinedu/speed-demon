# Resolves a runs.tsv merge conflict: one row per (commit, track, run id), keeping a noted copy.
import sys
p = sys.argv[1]; seen = {}; order = []
for l in open(p).read().split('\n'):
    if l.startswith(('<<<<<<<', '=======', '>>>>>>>')) or not l.strip(): continue
    f = l.split('\t'); key = tuple(f[:3])
    if key not in seen: seen[key] = l; order.append(key)
    elif len(f) > 7 and f[7] and not (len(seen[key].split('\t')) > 7 and seen[key].split('\t')[7]): seen[key] = l
open(p, 'w').write('\n'.join(seen[k] for k in order) + '\n')
