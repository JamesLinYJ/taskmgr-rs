from pathlib import Path


ACTIONS = Path("src/pages/processes/actions.rs")
text = ACTIONS.read_text(encoding="utf-8")

helper = "pub(super) fn normalize_debugger_command_with<F>("
if text.count(helper) != 1:
    raise RuntimeError(f"expected one debugger compatibility helper, found {text.count(helper)}")
text = text.replace(helper, "#[cfg(test)]\n" + helper, 1)

module_start = "#[cfg(test)]\nmod debugger_tests {"
production_marker = (
    "\n// 先构建完整的已验证句柄集合，再把不可逆的终止阶段与 UI 结果呈现分离。"
)
start = text.find(module_start)
if start < 0:
    raise RuntimeError("debugger test module start not found")
end = text.find(production_marker, start)
if end < 0:
    raise RuntimeError("production marker after debugger tests not found")
if text.find(module_start, start + len(module_start)) >= 0:
    raise RuntimeError("debugger test module start is not unique")

test_module = text[start:end].rstrip()
text = text[:start] + text[end:]
text = text.rstrip() + "\n\n" + test_module + "\n"
ACTIONS.write_text(text, encoding="utf-8", newline="\n")

print("Adjusted staged debugger tests for strict Clippy.")
