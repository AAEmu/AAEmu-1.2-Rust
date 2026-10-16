r"""Bound-doodad positions (server `housing_bindings*.json`) from the 10.x prefab attach points.

The server places a house's bound doodads (nameplate, doors, windows, barrel, ...) at a local
position per template and attach point (`AttachPointId` -> X/Y/Z).  The client gets these from
helper nodes named `$driver`, `$cannonN`, `$heal_pointN`, `$name_plate01` inside the house CGFs.
Position = (prefab object position + helper position / 100) with the template's `auto_z_offset_z`
subtracted from Z.  Validated against the existing 1.2 entry of housing 171 (see `--check`).

    c:\python312\python.exe -I rust\tools\backport\housing_bindings.py ^
        --db10 compact-10.sqlite3 --pak10 <10.x game_pak> --pak10-list pak10.tsv ^
        --housings 330,402,403,404,417,418,419,432,433,434 ^
        --out rust\backports\housing-thatched\server\housing_bindings.json ^
        [--check rust\crates\aaemu-game\app\Data\housing_bindings.json --pak12 ... --db12 ...]
"""
import argparse
import collections
import json
import os
import sys
import xml.etree.ElementTree as ET

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import assetlib  # noqa: E402
import bflib  # noqa: E402


def helper_name(attach_id):
    """AttachPointKind value -> helper node name in the CGF (None if not a housing helper)."""
    if attach_id == 1:
        return '$driver'
    if 9 <= attach_id <= 17:
        return '$cannon%d' % (attach_id - 9)
    if 36 <= attach_id <= 45:
        return '$heal_point%d' % (attach_id - 36)
    if 46 <= attach_id <= 56:
        return '$cannon%d' % (attach_id - 46 + 9)
    if attach_id == 57:
        return '$name_plate01'
    return None


class Source(object):
    """A database + pak pair whose prefabs/CGFs are read."""

    def __init__(self, db, pak):
        self.db, self.pak = db, pak
        self._libs = {}
        self._cgf = {}

    def lib(self, path):
        k = assetlib.norm(path)
        if k not in self._libs:
            data = self.pak.read(k)
            root = ET.fromstring(data)
            self._libs[k] = {p.get('Name'): p for p in root.iter('Prefab')}
        return self._libs[k]

    def helpers(self, cgf):
        k = assetlib.norm(assetlib.gp(cgf))
        if k not in self._cgf:
            data = self.pak.read(k)
            self._cgf[k] = {}
            if data:
                for name, parent, oid, pos, tm in assetlib.cgf_helpers(data):
                    self._cgf[k].setdefault(name.lower(), pos)
        return self._cgf[k]

    def housing(self, hid):
        return self.db.execute('select * from housings where id=?', (hid,)).fetchone()

    def attach_positions(self, hid, wanted, state=1, ref_rewrite=None):
        """{attach id: (x, y, z)} for the wanted attach ids, from the state-`state` prefabs."""
        h = self.housing(hid)
        model = self.db.execute('select * from models where id=?', (h['main_model_id'],)).fetchone()
        zoff = h['auto_z_offset_z'] or 0.0
        found = {}
        refs = self.db.execute('select file_path from prefab_elements where prefab_model_id=? and state_id=?',
                               (model['sub_id'], state)).fetchall()
        for (ref,) in refs:
            parsed = bflib.parse_prefab_ref(ref)
            if not parsed:
                continue
            d, stem, name = parsed
            prefab = self.lib(assetlib.gp(d + stem + '.xml')).get(name)
            if prefab is None:
                continue
            for o in prefab.iter('Object'):
                if o.get('Type') != 'Brush' or not o.get('Prefab'):
                    continue
                if o.get('Rotate') or o.get('Scale'):
                    continue            # decorative (ivy, plants); housing parts are unrotated
                px, py, pz = [float(v) for v in o.get('Pos', '0,0,0').split(',')]
                hp = self.helpers(o.get('Prefab'))
                for aid in wanted:
                    hn = helper_name(aid)
                    if hn and hn in hp and aid not in found:
                        x, y, z = hp[hn]
                        found[aid] = (px + x / 100.0, py + y / 100.0, pz + z / 100.0 - zoff)
        return found


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--db10', required=True)
    ap.add_argument('--pak10', required=True)
    ap.add_argument('--pak10-list', required=True)
    ap.add_argument('--housings', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--same-as', action='append', default=[], metavar='NEW=OLD',
                    help='copy the entry of an existing 1.2 template (e.g. 330=171 shares model 1082)')
    ap.add_argument('--existing', help='existing housing_bindings.json (source of --same-as entries / --check)')
    ap.add_argument('--check', action='store_true', help='compare computed positions of --check-housing with --existing')
    ap.add_argument('--check-housing', type=int, default=171)
    ap.add_argument('--db12')
    ap.add_argument('--pak12')
    ap.add_argument('--pak12-list')
    a = ap.parse_args()

    src10 = Source(bflib.open_mem(a.db10), assetlib.Pak(a.pak10, listing=a.pak10_list))
    existing = json.load(open(a.existing, encoding='utf-8')) if a.existing else []
    by_tpl = {t: e for e in existing for t in e['TemplateId']}

    if a.check:
        src12 = Source(bflib.open_mem(a.db12), assetlib.Pak(a.pak12, listing=a.pak12_list))
        hid = a.check_housing
        ref = by_tpl[hid]['AttachPointId']
        got = src12.attach_positions(hid, [int(k) for k in ref])
        worst = 0.0
        for k, v in sorted(ref.items(), key=lambda kv: int(kv[0])):
            g = got.get(int(k))
            if g is None:
                print('  attach %s: not derivable (existing %s)' % (k, v))
                continue
            err = max(abs(g[0] - v.get('X', 0)), abs(g[1] - v.get('Y', 0)), abs(g[2] - v.get('Z', 0)))
            worst = max(worst, err)
            print('  attach %-3s existing (%.4f, %.4f, %.4f) derived (%.4f, %.4f, %.4f) err %.4f'
                  % (k, v.get('X', 0), v.get('Y', 0), v.get('Z', 0), g[0], g[1], g[2], err))
        print('worst error for housing %d: %.4f m' % (hid, worst))
        return

    entries = collections.OrderedDict()   # json text of the attach dict -> [template ids]
    unresolved = collections.defaultdict(list)
    for hid in [int(x) for x in a.housings.split(',') if x]:
        wanted = sorted({r[0] for r in src10.db.execute(
            'select attach_point_id from housing_binding_doodads where housing_id=?', (hid,))})
        pos = src10.attach_positions(hid, wanted)
        for aid in wanted:
            if aid not in pos:
                unresolved[hid].append(aid)
        points = collections.OrderedDict(
            (str(aid), collections.OrderedDict([('X', round(v[0], 4)), ('Y', round(v[1], 4)), ('Z', round(v[2], 4))]))
            for aid, v in sorted(pos.items()))
        entries.setdefault(json.dumps(points), []).append(hid)
    out = []
    for text, tpls in entries.items():
        out.append(collections.OrderedDict([('TemplateId', tpls), ('AttachPointId', json.loads(text, object_pairs_hook=collections.OrderedDict))]))
    for spec in a.same_as:
        new, old = [int(x) for x in spec.split('=')]
        out.append(collections.OrderedDict([('TemplateId', [new]), ('AttachPointId', by_tpl[old]['AttachPointId'])]))
    with open(a.out, 'w', encoding='utf-8', newline='\n') as f:
        json.dump(out, f, indent=4, ensure_ascii=False)
        f.write('\n')
    print('wrote %s: %d entries for templates %s' % (a.out, len(out), [t for e in out for t in e['TemplateId']]))
    for hid, ids in sorted(unresolved.items()):
        print('  housing %d: no helper found for attach points %s' % (hid, ids))


if __name__ == '__main__':
    main()
