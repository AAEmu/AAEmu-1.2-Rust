r"""Generate the housing backport bundle (data SQL, asset list, prefab subset spec) from the 10.x
database.  Default roots: the Thatched Farmhouse family (housings 171, 330, 402-404, 417-419,
432-434).  Re-runnable and parameterised by housing ids; see README.md in this folder.

    c:\python312\python.exe -I rust\tools\backport\housing_thatched.py ^
        --db10 D:\aa-re\dwarf-backport-20261014\compact-10.sqlite3 ^
        --db12 D:\AA1.2\game\db\compact.sqlite3 ^
        --pak10 "D:\ArcheAge Returns 10.0.2.13 - 8yx - r575 - 2026-06-18\game_pak" ^
        --pak12 D:\AA1.2\game_pak ^
        --tool D:\aaemu-target\client-ext\debug\aaemu-clientpatch.exe ^
        --out rust\backports\housing-thatched

Merge rules (BACKPORTING.md): additive only.  A row that 1.2 already has (by key) is kept as is;
10.x columns 1.2 does not have are dropped; ids that are not stable across versions
(item_housings, localized_texts, housing_binding_doodads, prefab_elements) are assigned when the
patch is applied.  The shared patch (001) goes into client and server databases; the server patch
(002) only into the server database (rebuild tables, which the 1.2 client has no loader for).
"""
import argparse
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import assetlib  # noqa: E402
import bflib  # noqa: E402
from bflib import Plan, open_mem, q  # noqa: E402

DEFAULT_HOUSINGS = [171, 330, 402, 403, 404, 417, 418, 419, 432, 433, 434]

# Pointers a typed doodad-func / effect row may hold that must resolve in 1.2 (or the row is dropped).
REQUIRED_FK = {
    'craft_pack_id': 'craft_packs', 'quest_context_id': 'quest_contexts', 'buff_id': 'buffs',
    'loot_pack_id': 'loot_packs', 'npc_id': 'npcs', 'slave_id': 'slaves',
    'actability_group_id': 'actability_groups', 'craft_id': 'crafts',
}
# Housing infrastructure shared with existing 1.2 houses: not extended by this patch.
NOT_EXTENDED = ('housing_group_categories', 'housing_groups', 'housing_deco_limits', 'housing_deco_limit_elems')
# Doodad funcs of these types need quests or systems 1.2 does not have.
SKIPPED_FUNC_TYPES = {'DoodadFuncQuest': 'quests are not part of this bundle'}

# Dependency order of the generated SQL (cosmetic: the tables have no constraints).
TABLE_ORDER = ['taxations', 'icons', 'items', 'item_housings', 'prefab_models', 'prefab_elements', 'models',
               'skills', 'skill_effects', 'effects', 'buff_effects', 'craft_effects', 'special_effects',
               'interaction_effects', 'housings', 'housing_build_steps', 'doodad_groups', 'doodad_almighties',
               'doodad_place_skins', 'doodad_func_groups', 'doodad_funcs', 'doodad_phase_funcs',
               'housing_binding_doodads', 'localized_texts']


def snake(name):
    return re.sub(r'(?<!^)(?=[A-Z])', '_', name).lower()


class Walker(object):
    """Closure walker over the 10.x database writing into a Plan."""

    def __init__(self, plan, suffix, include_rebuild_skills=False):
        self.p = plan
        self.suffix = suffix
        self.d10 = plan.d10
        self.include_rebuild_skills = include_rebuild_skills
        self.in_progress = set()
        self.pack_of = {}            # housing id -> rebuilding pack id (new housings and present roots)
        self.new_housings = []
        self.unresolved = collections.defaultdict(set)   # table -> ids missing in 1.2 and not pulled
        self.func_types_used = collections.Counter()
        self._children = {}
        self._cols10 = {}

    # ---- helpers -------------------------------------------------------------------------------
    def type_table(self, actual_type):
        b = snake(actual_type)
        cands = [b + 's', b + 'es', (b[:-1] + 'ies') if b.endswith('y') else None, b, b + 'ses']
        for c in cands:
            if c and c in self.p.t10:
                return c
        return None

    def child_tables(self, parent):
        if parent in self._children:
            return self._children[parent]
        sing = parent[:-3] + 'y' if parent.endswith('ies') else parent[:-2] if parent.endswith(('ses', 'xes')) else parent[:-1]
        col = sing + '_id'
        res = [(t, col) for t in sorted(self.p.t10) if t != parent and t in self.p.t12 and col in self.cols10(t)]
        self._children[parent] = res
        return res

    def cols10(self, t):
        if t not in self._cols10:
            self._cols10[t] = bflib.columns(self.d10, t)
        return self._cols10[t]

    def rewrite_ref(self, ref, via):
        """prefab://... -> same prefab in the generated subset library; other refs are recorded as files."""
        parsed = bflib.parse_prefab_ref(ref)
        if parsed:
            d, stem, name = parsed
            self.p.assets.prefab(ref, via)
            return 'prefab://%s%s%s.xml/%s' % (d, stem, self.suffix, name)
        m = re.match(r'(?:cgf|cga|cga_loop|cdf|chr|skin)://(.*)$', ref, re.I)
        if m:
            self.p.assets.file(assetlib.gp(m.group(1)), via)
        return ref

    def external_ok(self, col, val):
        """Does a required foreign key resolve in 1.2 (or the plan)?"""
        if not val:
            return True
        t = REQUIRED_FK[col]
        if t not in self.p.t12 or self.p.known(t, val):
            return True
        self.unresolved[t].add(val)
        return False

    # ---- housing -------------------------------------------------------------------------------
    def add_housing(self, hid):
        row = self.p.row10('housings', hid)
        if row is None:
            raise SystemExit('housing %d not in the 10.x database' % hid)
        self.pack_of[hid] = row['housing_rebuilding_pack_id']
        if self.p.in12('housings', hid):
            self.p.kept['housings'] += 1
            return
        size = self.p.row10('housing_sizes', row['housing_size_id'])
        radius = size['garden_radius'] if size else None
        self.p.take('housings', row, garden_radius=radius)
        self.new_housings.append(hid)
        for k in ('main_model_id', 'door_model_id', 'stair_model_id'):
            if row[k]:
                self.add_model(row[k], 'housing %d' % hid)
        for st in self.p.rows10('housing_build_steps', 'housing_id', hid):
            self.p.take('housing_build_steps', st, link=('housing_id', 'step'))
            self.add_model(st['model_id'], 'housing %d step %d' % (hid, st['step']))
            self.add_skill(st['skill_id'])
        for b in self.p.rows10('housing_binding_doodads', 'housing_id', hid):
            if self.add_doodad(b['doodad_id']) is not False:
                self.p.add_row('housing_binding_doodads', dict(
                    owner_id=hid, owner_type='Housing', attach_point_id=b['attach_point_id'], doodad_id=b['doodad_id']))
        for ih in self.p.rows10('item_housings', 'design_id', hid):
            if self.add_item(ih['item_id']) is not False:
                self.p.add_row('item_housings', dict(item_id=ih['item_id'], design_id=hid))
        if row['taxation_id']:
            self.add_entity('taxations', row['taxation_id'])
        for k in ('demolish_refund_item_id', 'server_transfer_demolish_refund_item_id', 'rotate_item_id'):
            if row[k]:
                self.add_item(row[k])

    def add_entity(self, t, i, link=None):
        """A simple leaf row (taxations, icons, doodad_groups ...): added when 1.2 lacks it."""
        if self.p.in12(t, i) or self.p.planned(t, i):
            return True
        r = self.p.row10(t, i)
        if r is None:
            self.unresolved[t].add(i)
            return False
        self.p.take(t, r, link=link)
        return True

    def add_model(self, mid, via):
        if self.p.in12('models', mid) or self.p.planned('models', mid):
            return
        r = self.p.row10('models', mid)
        if r is None:
            self.unresolved['models'].add(mid)
            return
        if r['sub_type'] != 'PrefabModel':
            raise SystemExit('model %d has unsupported sub_type %s' % (mid, r['sub_type']))
        # 1.2 names the wide-angle camera distance after 10.x's "action mode" one
        self.p.take('models', r, camera_distance_for_wide_angle=r['camera_distance_for_action_mode'])
        if not self.p.in12('prefab_models', r['sub_id']):
            self.p.add_row('prefab_models', dict(id=r['sub_id']))
        for el in self.p.rows10('prefab_elements', 'prefab_model_id', r['sub_id']):
            new_ref = self.rewrite_ref(el['file_path'], 'model %d (%s)' % (mid, via))
            self.p.add_row('prefab_elements', dict(prefab_model_id=el['prefab_model_id'], state_id=el['state_id'],
                                                   file_path=new_ref))

    # ---- items / skills ----------------------------------------------------------------------------
    def add_item(self, iid):
        if self.p.in12('items', iid) or self.p.planned('items', iid):
            return True
        r = self.p.row10('items', iid)
        if r is None:
            self.unresolved['items'].add(iid)
            return False
        # 1.2 keeps prices in the item row; 10.x has an item_prices table (currency 0 = coin,
        # 1 = honor points, 2 = living points)
        price = {}
        for pr in self.p.rows10('item_prices', 'item_id', iid):
            cols = {0: ('price', 'refund'), 1: ('honor_price', None), 2: ('living_point_price', None)}.get(pr['currency_id'])
            if cols:
                price[cols[0]] = pr['price']
                if cols[1]:
                    price[cols[1]] = pr['refund']
        self.p.take('items', r, **price)
        for k in ('icon_id', 'male_icon_id', 'over_icon_id'):
            if r[k]:
                self.add_icon(r[k], 'item %d' % iid)
        if r['use_skill_id']:
            self.add_skill(r['use_skill_id'])
        if r['category_id'] and not self.p.in12('item_categories', r['category_id']):
            self.add_entity('item_categories', r['category_id'])
        return True

    def add_icon(self, icon_id, via):
        if self.p.in12('icons', icon_id) or self.p.planned('icons', icon_id):
            return
        r = self.p.row10('icons', icon_id)
        if r is None:
            self.unresolved['icons'].add(icon_id)
            return
        self.p.take('icons', r, link=('filename',))
        self.p.assets.icon(r['filename'], via)

    def add_skill(self, sid):
        """Pull a skill and its effects when 1.2 lacks it. Returns False if it cannot be supported."""
        if not sid:
            return True
        if self.p.in12('skills', sid) or self.p.planned('skills', sid):
            return True
        if sid in self.in_progress:
            return True
        r = self.p.row10('skills', sid)
        if r is None:
            self.unresolved['skills'].add(sid)
            return False
        self.in_progress.add(sid)
        self.p.take('skills', r)
        if r['icon_id']:
            self.add_icon(r['icon_id'], 'skill %d' % sid)
        for se in self.p.rows10('skill_effects', 'skill_id', sid):
            self.add_skill_effect(sid, se)
        self.in_progress.discard(sid)
        return True

    def add_skill_effect(self, sid, se):
        eff = self.p.row10('effects', se['effect_id'])
        if eff is None:
            self.p.drop('skill_effect without effects row', 'skill_effects', se['id'])
            return
        tt = self.type_table(eff['actual_type'])
        if tt is None or tt not in self.p.t12:
            self.p.drop('effect type %s unknown to 1.2' % eff['actual_type'], 'effects', eff['id'])
            return
        trow = self.p.row10(tt, eff['actual_id'])
        if trow is None:
            self.p.drop('typed effect row missing', 'effects', eff['id'])
            return
        bad = self.typed_row_problem(trow)
        if bad:
            self.p.drop('effect needs %s' % bad, 'effects', eff['id'])
            return
        if se['consume_item_id'] and not self.add_item(se['consume_item_id']):
            self.p.drop('effect consumes unavailable item', 'skill_effects', se['id'])
            return
        self.p.take('skill_effects', se, link=('skill_id',))
        self.p.take('effects', eff, link=('actual_type', 'actual_id'))
        self.p.take(tt, trow, content_check=True)

    def typed_row_problem(self, row):
        """Name of a required reference of a typed row that 1.2 cannot resolve, else None.
        Referenced skills, items and doodads are pulled into the plan."""
        for k in row.keys():
            v = row[k]
            if not isinstance(v, int) or not v:
                continue
            if k in ('skill_id', 'func_skill_id'):
                if not self.add_skill(v):
                    return 'skill %d' % v
            elif k == 'item_id':
                if not self.add_item(v):
                    return 'item %d' % v
            elif k in REQUIRED_FK:
                if not self.external_ok(k, v):
                    return '%s %d' % (REQUIRED_FK[k], v)
        return None

    # ---- doodads -------------------------------------------------------------------------------
    def add_doodad(self, did):
        if self.p.in12('doodad_almighties', did):
            self.p.kept['doodad_almighties'] += 1
            return True
        if self.p.planned('doodad_almighties', did):
            return True
        a = self.p.row10('doodad_almighties', did)
        if a is None:
            self.unresolved['doodad_almighties'].add(did)
            return False
        model = self.rewrite_ref(a['model'], 'doodad %d' % did) if a['model'] else a['model']
        self.p.take('doodad_almighties', a, model=model)
        if a['group_id']:
            self.add_entity('doodad_groups', a['group_id'])
        for sk in self.p.rows10('doodad_place_skins', 'doodad_almighty_id', did):
            self.p.take('doodad_place_skins', sk, link=('doodad_almighty_id',))
        for g in self.p.rows10('doodad_func_groups', 'doodad_almighty_id', did):
            self.p.take('doodad_func_groups', g, link=('doodad_almighty_id',))
            for f in self.p.rows10('doodad_funcs', 'doodad_func_group_id', g['id']):
                self.add_func('doodad_funcs', f, 'func_skill_id')
            for f in self.p.rows10('doodad_phase_funcs', 'doodad_func_group_id', g['id']):
                self.add_func('doodad_phase_funcs', f, None)
        return True

    def add_func(self, table, f, skill_col):
        ftype = f['actual_func_type']
        self.func_types_used[ftype] += 1
        if ftype in SKIPPED_FUNC_TYPES:
            self.p.drop('func %s: %s' % (ftype, SKIPPED_FUNC_TYPES[ftype]), table, f['id'])
            return
        tt = self.type_table(ftype)
        if tt is None or tt not in self.p.t12:
            self.p.drop('func type %s unknown to 1.2' % ftype, table, f['id'])
            return
        trow = self.p.row10(tt, f['actual_func_id'])
        if trow is None:
            self.p.drop('typed func row missing (%s)' % ftype, table, f['id'])
            return
        if skill_col and f[skill_col] and not self.add_skill(f[skill_col]):
            self.p.drop('func needs unavailable skill', table, f['id'])
            return
        bad = self.typed_row_problem(trow)
        if bad:
            self.p.drop('func %s needs %s' % (ftype, bad), table, f['id'])
            return
        self.p.take(table, f, link=('doodad_func_group_id',))
        if self.p.take(tt, trow, content_check=True):
            for ct, cc in self.child_tables(tt):
                for x in self.p.rows10(ct, cc, f['actual_func_id']):
                    self.p.take(ct, x, content_check=True)


# ---- rebuild (server only) ---------------------------------------------------------------------
REBUILD_TABLES = ['housing_rebuilding_packs', 'housing_rebuilding_pack_rebuildings', 'housing_rebuildings',
                  'housing_rebuilding_materials']
SQL_TYPE = {'integer': 'INT', 'int': 'INT', 'bigint': 'INT', 'float': 'REAL', 'double': 'REAL', 'real': 'REAL',
            'boolean': 'NUM', 'bool': 'NUM', 'varchar': 'TEXT', 'text': 'TEXT'}


def sql_type(decl):
    base = re.split(r'[\s(]', decl.strip().lower())[0]
    return SQL_TYPE.get(base, 'TEXT')


def rebuild_ddl(d10, t):
    cols = d10.execute('pragma table_info("%s")' % t).fetchall()
    body = ',\n  '.join('%s %s' % (q(c['name']), sql_type(c['type'])) for c in cols)
    return 'CREATE TABLE IF NOT EXISTS %s(\n  %s\n);' % (q(t), body)


def plan_rebuild(plan, walker, packs):
    """Rebuild tables for `packs` (and the skills their rebuildings use) into `plan`."""
    d10 = plan.d10
    for t in REBUILD_TABLES:
        plan.extra_cols[t] = bflib.columns(d10, t)
    targets = set()
    for pid in sorted(packs):
        plan.take('housing_rebuilding_packs', plan.row10('housing_rebuilding_packs', pid))
        for pr in plan.rows10('housing_rebuilding_pack_rebuildings', 'housing_rebuilding_pack_id', pid):
            plan.take('housing_rebuilding_pack_rebuildings', pr)
            rb = plan.row10('housing_rebuildings', pr['housing_rebuilding_id'])
            if plan.take('housing_rebuildings', rb):
                targets.add(rb['housing_id'])
                walker.add_skill(rb['skill_id'])
                for m in plan.rows10('housing_rebuilding_materials', 'housing_rebuilding_id', rb['id']):
                    plan.take('housing_rebuilding_materials', m)
    return targets


# ---- localized texts -----------------------------------------------------------------------------
LANG_COLS = ['ko', 'en_us', 'zh_cn', 'ja', 'ru', 'zh_tw', 'de', 'fr', 'th', 'ind', 'en_sg', 'pt', 'es']


def englishish(s):
    s = s.strip()
    return bool(s) and sum(ord(c) < 128 for c in s) >= 0.9 * len(s)


def add_localized(plan, skip=('localized_texts',)):
    """10.x texts of every planned row whose (tbl, column, idx) 1.2 lacks.

    10.x ships most new texts only in zh_cn (as English).  The 1.2 client reads its own language
    column, so en_us is seeded from an English-looking zh_cn text, and ko from the entity's own
    (Korean) `name` column, when those are empty."""
    d10 = plan.d10
    c12 = set(plan.cols12('localized_texts'))
    lang = [c for c in LANG_COLS if c in c12 and c in bflib.columns(d10, 'localized_texts')]
    n = 0
    for t in list(plan.rows):
        if t in skip or t in bflib.FRESH_ID_TABLES or plan.key_cols(t) != ('id',):
            continue
        ids = sorted(k[0] for k in plan.rows[t])
        for i in range(0, len(ids), 500):
            ch = ids[i:i + 500]
            for r in d10.execute('select * from localized_texts where tbl_name=? and idx in (%s) order by id' % ','.join('?' * len(ch)), [t] + ch):
                d = collections.OrderedDict((c, r[c]) for c in ['tbl_name', 'tbl_column_name', 'idx'] + lang)
                en = d.get('en_us')
                zh = d.get('zh_cn')
                if not en and zh and englishish(zh):
                    d['en_us'] = zh
                if not d.get('ko') and r['tbl_column_name'] == 'name':
                    own = plan.rows[t].get((r['idx'],), {}).get('name')
                    if own:
                        d['ko'] = own
                if plan.add_row('localized_texts', d):
                    n += 1
    return n


# ---- assets ----------------------------------------------------------------------------------------
def build_assets(plan, walker, donor, base):
    clo = assetlib.Closure(donor, base)
    for ref, via in plan.assets.prefab_refs.items():
        d, stem, name = bflib.parse_prefab_ref(ref)
        clo.prefab(assetlib.gp(d + stem + '.xml'), name, via)
    for path, via in plan.assets.files.items():
        clo.walk(path, via)
    icon_files = []
    for fn, via in plan.assets.icons.items():
        p = 'game/ui/icon/' + fn
        if p in donor:
            clo.add(p, 'icon', via)
            icon_files.append(p)
        else:
            clo.missing.setdefault(p, via)
    return clo


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--db10', required=True)
    ap.add_argument('--db12', required=True)
    ap.add_argument('--pak10', required=True)
    ap.add_argument('--pak12', required=True)
    ap.add_argument('--tool', required=True, help='aaemu-clientpatch binary (pak listing)')
    ap.add_argument('--out', required=True, help='bundle directory (rust/backports/<name>)')
    ap.add_argument('--housings', default=','.join(map(str, DEFAULT_HOUSINGS)))
    ap.add_argument('--name', default='housing-thatched')
    ap.add_argument('--version', default='1')
    ap.add_argument('--lib-suffix', default='_x10ht', help='suffix of the generated subset prefab libraries')
    ap.add_argument('--pak10-list', help='cached `pak list` output of the 10.x pak')
    ap.add_argument('--pak12-list', help='cached `pak list` output of the 1.2 pak')
    a = ap.parse_args()
    housings = [int(x) for x in a.housings.split(',') if x]

    d10, d12 = open_mem(a.db10), open_mem(a.db12)
    # ---- shared data
    plan = Plan(d10, d12)
    w = Walker(plan, a.lib_suffix)
    for h in housings:
        w.add_housing(h)
    nloc = add_localized(plan)
    # ---- server-only rebuild data
    splan = Plan(d10, d12)
    sw = Walker(splan, a.lib_suffix)
    packs = {p for p in w.pack_of.values() if p}
    targets = plan_rebuild(splan, sw, packs)
    for t in sorted(targets):
        if not (plan.in12('housings', t) or plan.planned('housings', t)):
            w.unresolved['housings(rebuild target)'].add(t)
    # materials of the rebuildings are items every client needs to show
    for m in splan.rows.get('housing_rebuilding_materials', {}).values():
        w.add_item(m['item_id'])
    # rebuild skills live only in the server patch, but their items/icons must not diverge
    nloc += add_localized(plan)
    sloc = add_localized(splan, skip=('localized_texts',))

    # ---- assets
    donor = assetlib.Pak(a.pak10, tool=a.tool, listing=a.pak10_list)
    base = assetlib.Pak(a.pak12, tool=a.tool, listing=a.pak12_list)
    clo = build_assets(plan, w, donor, base)
    status = clo.classify()

    # ---- write bundle
    out = a.out
    os.makedirs(os.path.join(out, 'sql'), exist_ok=True)
    head = ('-- Generated by rust/tools/backport/housing_thatched.py -- do not edit by hand.\n'
            '-- Bundle %s %s, housings %s.\n'
            '-- Additive only: every INSERT is guarded by NOT EXISTS on the row key; rows 1.2 already has are kept.\n'
            % (a.name, a.version, ','.join(map(str, housings))))
    sql1 = plan.emit_sql(head + '-- Shared patch: client and server compact.sqlite3.\n', TABLE_ORDER)
    open(os.path.join(out, 'sql', '001_housing_thatched.sql'), 'w', encoding='utf-8', newline='\n').write(sql1)

    s = [head + '-- Server patch: rebuild tables (the 1.2 client has no loader for them) and rebuild pack links.',
         '-- aaemu:add-column housings housing_rebuilding_pack_id INT', '']
    for t in REBUILD_TABLES:
        s.append(rebuild_ddl(d10, t))
    s.append('')
    body = splan.emit_sql('', ['housing_rebuilding_packs', 'housing_rebuilding_pack_rebuildings', 'housing_rebuildings',
                               'housing_rebuilding_materials', 'skills', 'skill_effects', 'effects', 'special_effects',
                               'buff_effects', 'craft_effects', 'icons', 'items', 'localized_texts'])
    s.append(body)
    for h in housings:
        if w.pack_of.get(h):
            s.append('UPDATE housings SET housing_rebuilding_pack_id = %d WHERE id = %d AND housing_rebuilding_pack_id IS NULL;'
                     % (w.pack_of[h], h))
    open(os.path.join(out, 'sql', '002_housing_rebuild_server.sql'), 'w', encoding='utf-8', newline='\n').write('\n'.join(s) + '\n')

    new_assets = sorted(k for k, v in status.items() if v == 'new')
    differs = sorted(k for k, v in status.items() if v == 'differs')
    with open(os.path.join(out, 'assets.txt'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('# Donor (10.x) pak files this bundle adds to the 1.2 client pak (new in 1.2). Generated.\n')
        for k in new_assets:
            f.write(k + '\n')
    with open(os.path.join(out, 'prefab_subsets.tsv'), 'w', encoding='utf-8', newline='\n') as f:
        f.write('old_lib\tnew_lib\tprefab_name\n')
        for lib, names in sorted(clo.used_prefabs.items()):
            stem, ext = os.path.splitext(lib)
            for n in sorted(names):
                f.write('%s\t%s\t%s\n' % (lib, stem + a.lib_suffix + ext, n))
    bundle = collections.OrderedDict([
        ('name', a.name), ('version', a.version),
        ('description', 'Thatched Farmhouse family (housings %s): tier-2 farms with bound doodads, rebuild data, assets.' % ','.join(map(str, housings))),
        ('sql', ['sql/001_housing_thatched.sql', collections.OrderedDict([('file', 'sql/002_housing_rebuild_server.sql'), ('targets', ['server'])])]),
        ('assets', collections.OrderedDict([('donor_files', 'assets.txt'), ('prefab_subsets', 'prefab_subsets.tsv')])),
    ])
    with open(os.path.join(out, 'bundle.json'), 'w', encoding='utf-8', newline='\n') as f:
        json.dump(bundle, f, indent=2)
        f.write('\n')

    # ---- report
    print('== shared patch rows: %d, localized: %d' % (sum(plan.counts().values()), nloc))
    for t, n in plan.counts().items():
        print('  %-28s %6d  (kept from 1.2: %d)' % (t, n, plan.kept[t]))
    print('== server patch rows: %d, localized: %d' % (sum(splan.counts().values()), sloc))
    for t, n in splan.counts().items():
        print('  %-28s %6d' % (t, n))
    print('== housing rebuild packs:', w.pack_of, 'targets', sorted(targets))
    print('== dropped rows by reason:')
    for reason, lst in sorted({**plan.dropped, **{'[server] ' + k: v for k, v in splan.dropped.items()}}.items()):
        print('  %5d  %s' % (len(lst), reason))
    print('== unresolved references (not in 1.2, not pulled):')
    for t, ids in sorted({**w.unresolved, **{'[server] ' + k: v for k, v in sw.unresolved.items()}}.items()):
        print('  %-24s %s' % (t, sorted(ids)[:30]))
    tot = sum(donor.get(k)[1] for k in new_assets)
    print('== assets: %d files in closure, %d new (%d bytes) listed in assets.txt, %d differ (kept as in 1.2), %d same'
          % (len(status), len(new_assets), tot, len(differs), sum(1 for v in status.values() if v == 'same')))
    for k in differs:
        print('  differs: %s' % k)
    print('  prefab subset libraries:', {k: len(v) for k, v in clo.used_prefabs.items()})
    print('  missing refs (%d):' % len(clo.missing))
    for k, v in sorted(clo.missing.items()):
        print('    %s <- %s' % (k, v))
    print('  particle/sound effect names referenced (not checked here):', len(clo.particles))
    print('  shaders:', {k: len(v) for k, v in sorted(clo.shaders.items())}, 'dds:', clo.dds_fourcc)
    for n in clo.notes:
        print('  note:', n)


if __name__ == '__main__':
    main()
