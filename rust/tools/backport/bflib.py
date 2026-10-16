"""Shared helpers for backport patch generators (see rust/docs/client/BACKPORTING.md).

A generator walks the dependency closure of some 10.x root rows and records every row that is
*absent* from the 1.2 database, translated to the 1.2 schema, in a `Plan`.  `Plan.emit_sql()` then
writes guarded, additive SQL: rows are inserted only when their key is not already present, so a
patch never overwrites 1.2 content and applying it twice cannot duplicate rows.

All database connections are read-only.  Run with `python -I` (no site/cwd imports).
"""
import collections
import json
import re
import sqlite3
import sys

# Tables whose row id is not stable across versions: ids are assigned when the patch is applied
# (max(id) + n) and rows are matched on their natural key instead.
FRESH_ID_TABLES = {
    'item_housings': ('item_id', 'design_id'),
    'localized_texts': ('tbl_name', 'tbl_column_name', 'idx'),
    'housing_binding_doodads': ('owner_id', 'owner_type', 'attach_point_id', 'doodad_id'),
    'prefab_elements': ('prefab_model_id', 'state_id', 'file_path'),
}


def open_ro(path):
    """Read-only sqlite connection with dict-like rows."""
    c = sqlite3.connect('file:%s?mode=ro' % str(path).replace('\\', '/'), uri=True)
    c.row_factory = sqlite3.Row
    return c


def open_mem(path):
    """Read-only source copied into memory, so Plan can add indexes (the compact.sqlite3 tables
    have none, every lookup would otherwise scan the table)."""
    src = open_ro(path)
    dst = sqlite3.connect(':memory:')
    src.backup(dst)
    src.close()
    dst.row_factory = sqlite3.Row
    return dst


def tables(db):
    return {r[0] for r in db.execute("select name from sqlite_master where type='table'")}


def columns(db, table):
    return [r[1] for r in db.execute('pragma table_info("%s")' % table)]


def q(ident):
    return '"%s"' % ident.replace('"', '""')


def lit(v):
    """SQL literal for a python value coming out of sqlite."""
    if v is None:
        return 'NULL'
    if isinstance(v, bool):
        return '1' if v else '0'
    if isinstance(v, int):
        return str(v)
    if isinstance(v, float):
        r = repr(v)
        return r if r not in ('inf', '-inf', 'nan') else 'NULL'
    if isinstance(v, bytes):
        return "X'%s'" % v.hex()
    s = str(v)
    if '\r' in s or '\n' in s:
        # keep the file free of raw CR/LF (line-ending normalisation would change the data)
        parts = re.split(r'(\r|\n)', s)
        return ' || '.join('char(%d)' % ord(p) if p in ('\r', '\n') else "'%s'" % p.replace("'", "''")
                           for p in parts if p != '')
    return "'%s'" % s.replace("'", "''")


class Collision(Exception):
    """A new child row would reuse an id that 1.2 uses for a different parent."""


class Plan(object):
    """Rows to add to the 1.2 database, by table, plus a report of what was skipped/dropped."""

    def __init__(self, db10, db12):
        self.d10, self.d12 = db10, db12
        self.t10, self.t12 = tables(db10), tables(db12)
        self._c12 = {}
        self.extra_cols = {}                       # tables this patch creates -> column list
        self.rows = collections.OrderedDict()      # table -> OrderedDict(key -> {col: value})
        self.kept = collections.Counter()          # table -> rows already in 1.2 (kept as is)
        self.dropped = collections.defaultdict(list)  # reason -> [(table, id)]
        self.notes = []
        self.assets = AssetRoots()
        self._indexed = set()
        self._nullfree = {}

    # ---- schema ------------------------------------------------------------------------------
    def cols12(self, t):
        if t in self.extra_cols:
            return self.extra_cols[t]
        if t not in self._c12:
            self._c12[t] = columns(self.d12, t) if t in self.t12 else []
        return self._c12[t]

    def key_cols(self, t):
        return FRESH_ID_TABLES.get(t, ('id',))

    def key_of(self, t, row):
        return tuple(row[c] for c in self.key_cols(t))

    # ---- lookups -----------------------------------------------------------------------------
    def _index(self, db, t, cols):
        key = (id(db), t, tuple(cols))
        if key not in self._indexed:
            db.execute('create index if not exists %s on %s (%s)' % (
                q('ix_%s_%s' % (t, '_'.join(cols))), q(t), ', '.join(q(c) for c in cols)))
            self._indexed.add(key)

    def row10(self, t, i, col='id'):
        if t not in self.t10:
            return None
        self._index(self.d10, t, [col])
        return self.d10.execute('select * from %s where %s=?' % (q(t), q(col)), (i,)).fetchone()

    def rows10(self, t, col, val):
        if t not in self.t10:
            return []
        self._index(self.d10, t, [col])
        return self.d10.execute('select * from %s where %s=? order by id' % (q(t), q(col)), (val,)).fetchall()

    def in12(self, t, i, col='id'):
        if t not in self.t12:
            return False
        self._index(self.d12, t, [col])
        return self.d12.execute('select 1 from %s where %s=? limit 1' % (q(t), q(col)), (i,)).fetchone() is not None

    def row12(self, t, i, col='id'):
        if t not in self.t12:
            return None
        self._index(self.d12, t, [col])
        return self.d12.execute('select * from %s where %s=? limit 1' % (q(t), q(col)), (i,)).fetchone()

    def planned(self, t, i):
        return t in self.rows and (i,) in self.rows[t]

    def known(self, t, i):
        """Row exists in 1.2 or is already part of the plan."""
        return self.in12(t, i) or self.planned(t, i)

    # ---- recording ---------------------------------------------------------------------------
    def _null_free(self, t):
        """{column: declared type} of the 1.2 columns that never hold NULL in 1.2's rows."""
        if t not in self._nullfree:
            info = self.d12.execute('pragma table_info("%s")' % t).fetchall() if t in self.t12 else []
            res = {}
            if info:
                counts = self.d12.execute('select %s from %s' % (
                    ', '.join('sum(%s is null)' % q(c['name']) for c in info), q(t))).fetchone()
                for c, n in zip(info, counts):
                    if not n:
                        res[c['name']] = (c['type'] or '').upper()
            self._nullfree[t] = res
        return self._nullfree[t]

    def translate(self, t, row):
        """Row restricted to the 1.2 columns of `t`.  Columns 1.2 has but 10.x lacks (or leaves
        NULL) get a neutral default (0, 'f', '') when 1.2 itself never stores NULL there, because
        the server loaders read such columns as non-optional."""
        c12 = self.cols12(t)
        keys = row.keys()
        out = collections.OrderedDict()
        nf = None
        for c in c12:
            v = row[c] if c in keys else None
            if v is None and t not in self.extra_cols:
                if nf is None:
                    nf = self._null_free(t)
                if c in nf:
                    ty = nf[c]
                    v = 0.0 if ty == 'REAL' else 'f' if ty == 'NUM' else '' if ty == 'TEXT' else 0
            if c in keys or v is not None:
                out[c] = v
        return out

    def exists_natural(self, t, rowd):
        """Is a row with this natural key already in the 1.2 database?"""
        if t not in self.t12:
            return False
        kc = self.key_cols(t)
        self._index(self.d12, t, kc)
        sql = 'select 1 from %s where %s limit 1' % (q(t), ' and '.join('%s is ?' % q(c) for c in kc))
        return self.d12.execute(sql, [rowd[c] for c in kc]).fetchone() is not None

    def add_row(self, t, rowd, link=None, content_check=False):
        """Record a translated row. Returns True when the row is new (callers then descend into
        its dependants), False when 1.2 already has it (1.2's version is kept).

        link: columns identifying the parent relation of a child row; a 1.2 row with the same id
        but another parent is a Collision.  content_check: for typed rows without a link, a 1.2
        row with the same id must have identical common-column values."""
        key = self.key_of(t, rowd)
        if None in key:
            raise Collision('%s row has a NULL key column %s: %r' % (t, self.key_cols(t), rowd))
        if t in self.rows and key in self.rows[t]:
            return False
        if self.exists_natural(t, rowd):
            old = self.row12(t, key[0]) if self.key_cols(t) == ('id',) else None
            if old is not None:
                if link:
                    for c in link:
                        if old[c] != rowd.get(c):
                            raise Collision('%s id %s: 1.2 has %s=%r, 10.x has %r' % (t, key, c, old[c], rowd.get(c)))
                elif content_check:
                    diff = [c for c in rowd if c in old.keys() and old[c] != rowd[c]]
                    if diff:
                        raise Collision('%s id %s exists in 1.2 with different %s' % (t, key, diff))
            self.kept[t] += 1
            return False
        self.rows.setdefault(t, collections.OrderedDict())[key] = rowd
        return True

    def take(self, t, row10, link=None, content_check=False, **override):
        """Translate a 10.x row to the 1.2 schema and record it (see add_row)."""
        d = self.translate(t, row10)
        d.update(override)
        return self.add_row(t, d, link, content_check)

    def drop(self, reason, t, i):
        self.dropped[reason].append((t, i))

    # ---- report ------------------------------------------------------------------------------
    def counts(self):
        return {t: len(v) for t, v in self.rows.items()}

    # ---- SQL ---------------------------------------------------------------------------------
    def emit_sql(self, header, order=None, batch=100, skip_tables=()):
        out = [header.rstrip('\n'), '']
        names = [t for t in (order or list(self.rows)) if t in self.rows and t not in skip_tables]
        names += [t for t in self.rows if t not in names and t not in skip_tables]
        temps = []
        for t in names:
            rows = list(self.rows[t].values())
            if not rows:
                continue
            cols = [c for c in self.cols12(t) if any(c in r for r in rows)]
            out.append('-- %s: %d rows' % (t, len(rows)))
            if t in FRESH_ID_TABLES:
                out += self._emit_fresh(t, rows, cols, batch, temps)
            else:
                out += self._emit_plain(t, rows, cols, batch)
            out.append('')
        for tmp in temps:
            out.append('DROP TABLE %s;' % tmp)
        return '\n'.join(out) + '\n'

    def _values(self, rows, cols, batch):
        for i in range(0, len(rows), batch):
            chunk = rows[i:i + batch]
            yield ',\n'.join('(' + ', '.join(lit(r.get(c)) for c in cols) + ')' for r in chunk)

    def _emit_plain(self, t, rows, cols, batch):
        kc = self.key_cols(t)
        qc = ', '.join(q(c) for c in cols)
        out = []
        for vals in self._values(rows, cols, batch):
            out.append('WITH v(%s) AS (VALUES\n%s)\nINSERT INTO %s (%s)\n%s;'
                       % (qc, vals, q(t), qc, self._anti_join(t, kc, cols)))
        return out

    def _anti_join(self, t, kc, cols):
        """SELECT of the CTE rows whose key is not in `t` yet. A LEFT JOIN on '=' lets SQLite build
        one automatic index over `t` (the tables have none; a correlated NOT EXISTS would scan the
        table once per row)."""
        on = ' AND '.join('x.%s = v.%s' % (q(c), q(c)) for c in kc)
        return 'SELECT %s FROM v LEFT JOIN %s x ON %s\nWHERE x.%s IS NULL' % (
            ', '.join('v.' + q(c) for c in cols), q(t), on, q(kc[0]))

    def _emit_fresh(self, t, rows, cols, batch, temps):
        """Rows keyed on a natural key with an id assigned at apply time."""
        kc = self.key_cols(t)
        data_cols = [c for c in cols if c != 'id']
        qc = ', '.join(q(c) for c in data_cols)
        new = '_aaemu_new_%s' % t
        base = '_aaemu_base_%s' % t
        temps += [new, base]
        out = ['CREATE TEMP TABLE %s AS SELECT * FROM %s WHERE 0;' % (new, q(t))]
        for vals in self._values(rows, data_cols, batch):
            out.append('WITH v(%s) AS (VALUES\n%s)\nINSERT INTO %s (%s)\n%s;'
                       % (qc, vals, new, qc, self._anti_join(t, kc, data_cols)))
        out.append('CREATE TEMP TABLE %s AS SELECT COALESCE(MAX(id), 0) AS b FROM %s;' % (base, q(t)))
        out.append('INSERT INTO %s (id, %s)\nSELECT %s.b + n.rowid, %s FROM %s n, %s ORDER BY n.rowid;'
                   % (q(t), qc, base, ', '.join('n.' + q(c) for c in data_cols), new, base))
        return out



class AssetRoots(object):
    """Things the planned rows reference in the pak: prefab refs, plain model files, icons."""

    def __init__(self):
        self.prefab_refs = collections.OrderedDict()   # 'prefab://...' -> via
        self.files = collections.OrderedDict()         # pak path (lower) -> via
        self.icons = collections.OrderedDict()         # filename -> via

    def prefab(self, ref, via):
        self.prefab_refs.setdefault(ref, via)

    def file(self, path, via):
        self.files.setdefault(path.lower().replace('\\', '/'), via)

    def icon(self, filename, via):
        self.icons.setdefault(filename, via)


def parse_prefab_ref(ref):
    """'prefab://Prefabs/housing_big.xml/housing_step_small.nuia_dead' ->
    ('prefabs', 'housing_big', 'housing_step_small.nuia_dead') or None."""
    m = re.match(r'prefab://(.*?)([^/]+)\.xml/(.+)$', ref, re.I)
    if not m:
        return None
    return m.group(1), m.group(2), m.group(3)


def eprint(*a):
    print(*a, file=sys.stderr)


def dump_json(path, obj):
    with open(path, 'w', encoding='utf-8', newline='\n') as f:
        json.dump(obj, f, indent=1, ensure_ascii=False, sort_keys=True)
        f.write('\n')
