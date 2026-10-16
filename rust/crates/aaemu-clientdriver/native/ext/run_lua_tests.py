"""Runs core_tests.lua with the `lupa` package (bundled Lua 5.1) when no `lua` binary is installed.

    python -I run_lua_tests.py            # needs: pip install lupa (any stock Lua 5.1 also works:
                                          #        lua core_tests.lua)
"""
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

lua = lupa_lua.LuaRuntime(unpack_returned_tuples=True)
lua.globals().EXT_DIR = here.replace("\\", "/")
lua.execute("os.exit = function(c) error('Lua tests failed (exit ' .. tostring(c) .. ')', 0) end")
with open(os.path.join(here, "core_tests.lua"), encoding="utf-8") as f:
    code = f.read()
try:
    lua.execute(code)
except lupa_lua.LuaError as e:
    print(e)
    sys.exit(1)
