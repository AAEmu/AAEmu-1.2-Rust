"""Asset closure for backports: prefab library -> CGF/CGA/CDF/CHR -> MTL -> DDS, against two paks.

Pak access goes through the Rust tool: file listings come from
`aaemu-clientpatch pak list <pak>` (name, size, md5, offset) and file bytes are read straight from
the pak by offset (game_pak data is stored raw; only the file table is encrypted).  Nothing is
ever written to a pak here.
"""
import os
import re
import struct
import subprocess
import xml.etree.ElementTree as ET


def norm(p):
    return p.strip().replace('\\', '/').lower().lstrip('/')


def gp(p):
    """Pak path of a data reference ('Objects/x.cgf' and 'game/objects/x.cgf' both work)."""
    p = p.strip().replace('\\', '/')
    if not p.lower().startswith('game/'):
        p = 'game/' + p.lstrip('/')
    return p


class Pak(object):
    def __init__(self, pak_path, tool=None, listing=None):
        self.path = pak_path
        self.idx = {}   # lower path -> (stored name, size, md5 hex, offset)
        if listing is None:
            if tool is None:
                raise ValueError('pass the aaemu-clientpatch binary (tool=) or a listing file')
            out = subprocess.run([tool, 'pak', 'list', pak_path], check=True, capture_output=True).stdout
            lines = out.decode('utf-8', 'replace').splitlines()
        else:
            with open(listing, encoding='utf-8') as f:
                lines = f.read().splitlines()
        for line in lines:
            parts = line.split('\t')
            if len(parts) == 4:
                self.idx[norm(parts[0])] = (parts[0], int(parts[1]), parts[2], int(parts[3]))
        self._f = None

    def __contains__(self, name):
        return norm(name) in self.idx

    def get(self, name):
        return self.idx.get(norm(name))

    def read(self, name):
        e = self.idx.get(norm(name))
        if e is None:
            return None
        if self._f is None:
            self._f = open(self.path, 'rb')
        self._f.seek(e[3])
        return self._f.read(e[1])

    def read_head(self, name, n):
        e = self.idx.get(norm(name))
        if e is None:
            return None
        if self._f is None:
            self._f = open(self.path, 'rb')
        self._f.seek(e[3])
        return self._f.read(min(n, e[1]))


def cgf_mtl_names(d):
    """Material names (MtlName chunks) of a CGF/CGA, or raise ValueError."""
    if d[:6] != b'CryTek':
        raise ValueError('not a CryTek file')
    ftype, ver, tofs = struct.unpack_from('<III', d, 8)
    n = struct.unpack_from('<I', d, tofs)[0]
    ents = []
    if ver == 0x744:
        for i in range(n):
            t, v, o, cid = struct.unpack_from('<IIII', d, tofs + 4 + i * 16)
            ents.append((t, v, o))
    elif ver == 0x745:
        for i in range(n):
            t, v, o, cid, sz = struct.unpack_from('<IIIII', d, tofs + 4 + i * 20)
            ents.append((t, v, o))
    else:
        raise ValueError('unsupported CGF version %#x' % ver)
    names = []
    for t, v, o in ents:
        # MtlName chunk; sub-material chunks carry flag 0x10 and are skipped
        if t == 0xCCCC0014 and not (struct.unpack_from('<I', d, o + 16)[0] & 0x10):
            names.append(d[o + 24:o + 24 + 128].split(b'\0')[0].decode('latin1'))
    return ver, [n for n in names if n]


def cgf_helpers(d):
    """[(node name, parent id, object id, local pos(3), tm(16))] of the Node chunks (v0x823)."""
    ftype, ver, tofs = struct.unpack_from('<III', d, 8)
    n = struct.unpack_from('<I', d, tofs)[0]
    out = []
    for i in range(n):
        if ver == 0x744:
            t, v, o, cid = struct.unpack_from('<IIII', d, tofs + 4 + i * 16)
        else:
            t, v, o, cid, sz = struct.unpack_from('<IIIII', d, tofs + 4 + i * 20)
        if t != 0xCCCC000B:
            continue
        b = o + 16
        name = d[b:b + 64].split(b'\0')[0].decode('latin1')
        obj_id, parent_id, nchild, mat_id = struct.unpack_from('<iiii', d, b + 64)
        tm = struct.unpack_from('<16f', d, b + 84)
        pos = struct.unpack_from('<3f', d, b + 148)
        out.append((name, parent_id, obj_id, pos, tm))
    return out


def parse_mtl(data):
    try:
        root = ET.fromstring(data)
    except ET.ParseError:
        root = ET.fromstring(data.lstrip(b'\xef\xbb\xbf').strip(b'\x00'))
    shaders, tex, refs = set(), set(), set()
    for m in root.iter('Material'):
        sh = m.get('Shader')
        if sh:
            shaders.add(sh)
        mat = m.get('Material') or m.get('MaterialRef')
        if mat:
            refs.add(mat)
        for t in m.iter('Texture'):
            f = t.get('File')
            if f:
                tex.add(f)
    return shaders, tex, refs


class Closure(object):
    """Collects the files a set of prefabs / model files needs from the donor (10.x) pak."""

    def __init__(self, donor, base):
        self.donor, self.base = donor, base
        self.files = {}           # lower path -> dict(kind, via)
        self.missing = {}         # lower path -> via
        self.libs = {}            # lib path -> {prefab name: Element} or None
        self.used_prefabs = {}    # lib path (lower) -> {prefab Name (as written)}
        self.shaders = {}
        self.dds_fourcc = {}
        self.particles = set()
        self.notes = []

    def add(self, path, kind, via):
        k = norm(path)
        if k in self.files:
            return False
        if k not in self.donor:
            self.missing.setdefault(k, via)
            return False
        self.files[k] = dict(kind=kind, via=via)
        return True

    def lib(self, lib_path):
        k = norm(lib_path)
        if k not in self.libs:
            data = self.donor.read(k)
            if data is None:
                self.libs[k] = None
            else:
                root = ET.fromstring(data)
                self.libs[k] = {p.get('Name', ''): p for p in root.iter('Prefab')}
        return self.libs[k]

    def prefab(self, lib_path, name, via):
        """Walk one prefab of a donor library (case-sensitive prefab name match first)."""
        lk = norm(lib_path)
        lib = self.lib(lk)
        if lib is None:
            self.missing.setdefault(lk, via)
            return False
        el = lib.get(name)
        if el is None:
            for n, e in lib.items():
                if n.lower() == name.lower():
                    name, el = n, e
                    break
        if el is None:
            self.missing.setdefault(lk + '#' + name, via)
            return False
        used = self.used_prefabs.setdefault(lk, set())
        if name in used:
            return True
        used.add(name)
        for sub in el.iter():
            cand = []
            if sub.tag == 'Object':
                p = sub.get('Prefab', '')
                if p.lower().endswith('.cgf'):
                    cand.append(p)
                g = sub.get('Geometry', '')
                if g.lower().endswith(('.cga', '.cgf')):
                    cand.append(g)
                if sub.get('Material'):
                    m = sub.get('Material').strip()
                    cand.append(m if m.lower().endswith('.mtl') else m + '.mtl')
            elif sub.tag == 'Properties':
                om = sub.get('object_Model')
                if om:
                    cand.append(om)
                pe = sub.get('ParticleEffect')
                if pe:
                    self.particles.add(pe)
            for c in cand:
                self.walk(gp(c), 'prefab:%s' % name)
        return True

    def walk(self, path, via):
        k = norm(path)
        ext = os.path.splitext(k)[1]
        if k in self.files or k in self.missing:
            return
        if not self.add(k, ext.lstrip('.'), via):
            return
        if ext in ('.cgf', '.cga'):
            self._cgf(k, via)
        elif ext == '.mtl':
            self._mtl(k)
        elif ext == '.cdf':
            self._cdf(k)
        elif ext == '.chr':
            self._chr(k)
        elif ext == '.cal':
            self._cal(k)
        elif ext == '.dds':
            head = self.donor.read_head(k, 128)
            if head and head[:4] == b'DDS ':
                pf_flags, fourcc = struct.unpack_from('<I4s', head, 80)
                tag = fourcc.decode('latin1') if pf_flags & 4 else 'rgb'
                self.dds_fourcc[tag] = self.dds_fourcc.get(tag, 0) + 1

    def _cgf(self, k, via):
        data = self.donor.read(k)
        try:
            ver, names = cgf_mtl_names(data)
        except Exception as ex:
            self.notes.append('%s: cgf parse error %s' % (k, ex))
            return
        d = os.path.dirname(k)
        for n in sorted(set(names)):
            cand = '%s/%s.mtl' % (d, n.replace('\\', '/'))
            if norm(cand) in self.donor:
                self.walk(cand, k)
            elif norm(gp(n) + '.mtl') in self.donor:
                self.walk(gp(n) + '.mtl', k)
            else:
                self.missing.setdefault(norm(cand), '%s (mtl name %r)' % (k, n))
        ext = os.path.splitext(k)[1]
        base = os.path.splitext(os.path.basename(k))[0]
        for lod in range(1, 4):
            cand = '%s/%s_lod%d%s' % (d, base, lod, ext)
            if cand in self.donor.idx:
                self.walk(cand, k + '(lod)')
        if ext == '.cga':
            for q in self.donor.idx:
                if q.startswith(d + '/' + base) and q.endswith('.anm') and os.path.dirname(q) == d:
                    self.walk(q, k + '(anm?)')

    def _mtl(self, k):
        data = self.donor.read(k)
        try:
            shaders, tex, refs = parse_mtl(data)
        except Exception as ex:
            self.notes.append('%s: mtl parse error %s' % (k, ex))
            return
        for s in shaders:
            self.shaders.setdefault(s, set()).add(k)
        for t in tex:
            self.walk(gp(t), k)
        for r in refs:
            self.walk(gp(r if r.lower().endswith('.mtl') else r + '.mtl'), k)

    def _cdf(self, k):
        try:
            root = ET.fromstring(self.donor.read(k))
        except Exception:
            return
        for el in root.iter():
            for a in ('Binding', 'Material', 'Skin', 'Model'):
                v = el.get(a)
                if v and re.search(r'\.(chr|skin|cgf|cga|mtl)$', v, re.I):
                    self.walk(gp(v), k)

    def _chr(self, k):
        base = os.path.splitext(k)[0]
        for ext in ('.cal', '.mtl'):
            if base + ext in self.donor.idx:
                self.walk(base + ext, k)

    def _cal(self, k):
        """Animation list: `#filepath = <dir>`, `name = file.caf` lines, `$...Database = path`."""
        data = self.donor.read(k)
        text = data.decode('latin1')
        anim_dir = ''
        for line in text.splitlines():
            line = line.split('//')[0].strip()
            if line.lower().startswith('#filepath'):
                anim_dir = line.split('=', 1)[1].strip().replace('\\', '/')
            elif line.startswith('--') or not line or '=' not in line:
                continue
            else:
                name, val = [x.strip() for x in line.split('=', 1)]
                val = val.replace('\\', '/')
                if name.startswith('$'):
                    if val.lower().endswith('.animevents'):
                        self.walk_optional(gp(val), k)
                elif val.lower().endswith('.caf') and anim_dir:
                    self.walk_optional(gp(anim_dir + '/' + val), k)

    def walk_optional(self, path, via):
        """A file that may legitimately be absent (listed but unused animation)."""
        if norm(path) in self.donor:
            self.walk(path, via)
        else:
            self.notes.append('optional file not in the donor pak: %s (from %s)' % (norm(path), via))

    # ---- classification against the 1.2 pak ---------------------------------------------------
    def classify(self):
        """-> {path: 'new' | 'same' | 'differs'} for every collected file."""
        out = {}
        for k in self.files:
            e12 = self.base.get(k)
            if e12 is None:
                out[k] = 'new'
            else:
                # the md5 in original paks is often unset (zeros): compare the bytes
                e10 = self.donor.get(k)
                same = e12[1] == e10[1] and self.base.read(k) == self.donor.read(k)
                out[k] = 'same' if same else 'differs'
        return out


def subset_name(lib_stem, suffix):
    return lib_stem + suffix
