"""Runs the Lua tests (core_tests.lua, house_rebuild_tests.lua, ...) with the `lupa` package
(bundled Lua 5.1) when no `lua` binary is installed. Every file gets a fresh Lua state.

    python -I run_lua_tests.py [file_tests.lua ...]   # needs: pip install lupa (any stock Lua 5.1
                                                       # also works: lua core_tests.lua)
"""
import glob
import os
import sys

here = os.path.dirname(os.path.abspath(__file__))
extra = os.environ.get("LUPA_PATH")
if extra:
    sys.path.insert(0, extra)
try:
    from lupa import lua51 as lupa_lua
except ImportError:
    sys.exit("lupa (lua51) is not installed; run `lua core_tests.lua` with a stock Lua 5.1 instead")

names = sys.argv[1:] or sorted(os.path.basename(p) for p in glob.glob(os.path.join(here, "*_tests.lua")))
failed = False
for name in names:
    lua = lupa_lua.LuaRuntime(unpack_returned_tuples=True)
    lua.globals().EXT_DIR = here.replace("\\", "/")
    lua.execute("os.exit = function(c) error('Lua tests failed (exit ' .. tostring(c) .. ')', 0) end")
    with open(os.path.join(here, name), encoding="utf-8") as f:
        code = f.read()
    print(f"== {name}")
    try:
        lua.execute(code)
    except lupa_lua.LuaError as e:
        print(e)
        failed = True
sys.exit(1 if failed else 0)
