import importlib.util, sys, os
spec = importlib.util.spec_from_file_location("aot", "/exts/worv.core.warp_compat/tools/aot_compile.py")
m = importlib.util.module_from_spec(spec); sys.modules["aot"] = m; spec.loader.exec_module(m)
import warp as wp
wp.init()
name, src, out = [f for f in m.FLAT_MODULES if f[0] == "manure_mpm_kernels"][0]
s2 = importlib.util.spec_from_file_location(name, src)
mod = importlib.util.module_from_spec(s2); sys.modules[name] = mod; s2.loader.exec_module(mod)
sys.exit(0 if m._compile(mod, out, "wp_" + name) else 1)
