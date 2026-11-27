import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None

SCRIPT_PATH = Path(__file__).with_name("mcp-config.py")
SPEC = importlib.util.spec_from_file_location("mcp_config", SCRIPT_PATH)
mcp_config = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(mcp_config)


class McpConfigTests(unittest.TestCase):
    def test_claude_register_update_idempotent_and_remove(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / ".claude.json"
            mcp_config.update_claude_config(
                cfg, "wsl.exe", ["-d", "Ubuntu", "-e", "/usr/local/bin/focusstage", "mcp"]
            )
            data = json.loads(cfg.read_text(encoding="utf-8"))
            self.assertEqual(
                data["mcpServers"]["focusstage"],
                {
                    "type": "stdio",
                    "command": "wsl.exe",
                    "args": ["-d", "Ubuntu", "-e", "/usr/local/bin/focusstage", "mcp"],
                },
            )

            mtime = cfg.stat().st_mtime_ns
            mcp_config.update_claude_config(
                cfg, "wsl.exe", ["-d", "Ubuntu", "-e", "/usr/local/bin/focusstage", "mcp"]
            )
            self.assertEqual(cfg.stat().st_mtime_ns, mtime)

            # Update to native and preserve sibling servers
            data["mcpServers"]["other"] = {"type": "stdio", "command": "other", "args": []}
            cfg.write_text(json.dumps(data), encoding="utf-8")
            mcp_config.update_claude_config(cfg, "/usr/local/bin/focusstage", ["mcp"])
            updated = json.loads(cfg.read_text(encoding="utf-8"))
            self.assertEqual(updated["mcpServers"]["focusstage"]["command"], "/usr/local/bin/focusstage")
            self.assertIn("other", updated["mcpServers"])

            mcp_config.update_claude_config(cfg, None, None)
            removed = json.loads(cfg.read_text(encoding="utf-8"))
            self.assertNotIn("focusstage", removed["mcpServers"])
            self.assertIn("other", removed["mcpServers"])

    def test_codex_preserves_comments_quotes_and_neighbors(self):
        sample = (
            '# top comment\n'
            'model = "gpt-6"\n'
            '\n'
            '[projects.\'c:\\users\\alice\\app\']\n'
            'trust_level = "trusted"\n'
            '\n'
            '[mcp_servers.other]\n'
            'command = \'C:\\tools\\other.exe\'\n'
            'args = ["mcp"]\n'
            '[apps.connector]\n'
            'approval_mode = "approve"\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / ".codex" / "config.toml"
            cfg.parent.mkdir(parents=True)
            cfg.write_text(sample, encoding="utf-8")

            mcp_config.update_codex_config(
                cfg, "wsl.exe", ["-d", "Ubuntu-24.04", "-e", "/usr/local/bin/focusstage", "mcp"]
            )
            text = cfg.read_text(encoding="utf-8")
            self.assertIn("# top comment\n", text)
            self.assertIn("[projects.'c:\\users\\alice\\app']\n", text)
            self.assertIn("[mcp_servers.focusstage]\n", text)
            if tomllib is not None:
                parsed = tomllib.loads(text)
                self.assertEqual(
                    parsed["mcp_servers"]["focusstage"],
                    {
                        "command": "wsl.exe",
                        "args": ["-d", "Ubuntu-24.04", "-e", "/usr/local/bin/focusstage", "mcp"],
                    },
                )

            # Idempotent re-run does not rewrite file
            mtime = cfg.stat().st_mtime_ns
            mcp_config.update_codex_config(
                cfg, "wsl.exe", ["-d", "Ubuntu-24.04", "-e", "/usr/local/bin/focusstage", "mcp"]
            )
            self.assertEqual(cfg.stat().st_mtime_ns, mtime)

            # Remove restores original content cleanly
            mcp_config.update_codex_config(cfg, None, None)
            self.assertEqual(cfg.read_text(encoding="utf-8"), sample)

    def test_codex_replaces_existing_and_preserves_next_section_comment(self):
        sample = (
            '[desktop]\n'
            'locale = "zh-CN"\n'
            '\n'
            '[mcp_servers.focusstage]\n'
            'command = "old-cmd"\n'
            'args = ["old"]\n'
            '\n'
            '[mcp_servers.focusstage.env]\n'
            'OLD = "1"\n'
            '\n'
            '# belongs to next section\n'
            '[model_providers.demo]\n'
            'name = "demo"\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / "config.toml"
            cfg.write_text(sample, encoding="utf-8")

            mcp_config.update_codex_config(cfg, "/usr/local/bin/focusstage", ["mcp"])
            text = cfg.read_text(encoding="utf-8")
            self.assertNotIn("[mcp_servers.focusstage.env]", text)
            self.assertIn("# belongs to next section\n[model_providers.demo]\n", text)
            if tomllib is not None:
                parsed = tomllib.loads(text)
                self.assertEqual(
                    parsed["mcp_servers"]["focusstage"],
                    {"command": "/usr/local/bin/focusstage", "args": ["mcp"]},
                )

            mcp_config.update_codex_config(cfg, None, None)
            removed = cfg.read_text(encoding="utf-8")
            self.assertNotIn("focusstage", removed)
            self.assertIn("# belongs to next section\n[model_providers.demo]\n", removed)

    def test_codex_ignores_header_inside_multiline_string(self):
        sample = (
            'note = """\n'
            '[mcp_servers.focusstage]\n'
            'command = "not-a-header"\n'
            '"""\n'
        )
        with tempfile.TemporaryDirectory() as tmp:
            cfg = Path(tmp) / "config.toml"
            cfg.write_text(sample, encoding="utf-8")

            mcp_config.update_codex_config(cfg, "/usr/local/bin/focusstage", ["mcp"])
            text = cfg.read_text(encoding="utf-8")
            self.assertIn('command = "not-a-header"', text)
            if tomllib is not None:
                parsed = tomllib.loads(text)
                self.assertIn("[mcp_servers.focusstage]", parsed["note"])
                self.assertEqual(parsed["mcp_servers"]["focusstage"]["command"], "/usr/local/bin/focusstage")

    def test_update_all_dual_side_register_and_remove(self):
        with tempfile.TemporaryDirectory() as tmp:
            win_home = Path(tmp) / "win"
            wsl_home = Path(tmp) / "wsl"
            win_home.mkdir()
            wsl_home.mkdir()

            mcp_config.update_all(win_home, wsl_home, "Ubuntu", remove=False)

            win_claude = json.loads((win_home / ".claude.json").read_text(encoding="utf-8"))
            wsl_claude = json.loads((wsl_home / ".claude.json").read_text(encoding="utf-8"))
            self.assertEqual(win_claude["mcpServers"]["focusstage"]["command"], "wsl.exe")
            self.assertEqual(
                win_claude["mcpServers"]["focusstage"]["args"],
                ["-d", "Ubuntu", "-e", "/usr/local/bin/focusstage", "mcp"],
            )
            self.assertEqual(wsl_claude["mcpServers"]["focusstage"]["command"], "/usr/local/bin/focusstage")
            self.assertEqual(wsl_claude["mcpServers"]["focusstage"]["args"], ["mcp"])

            win_codex = (win_home / ".codex" / "config.toml").read_text(encoding="utf-8")
            wsl_codex = (wsl_home / ".codex" / "config.toml").read_text(encoding="utf-8")
            self.assertIn('command = "wsl.exe"', win_codex)
            self.assertIn('command = "/usr/local/bin/focusstage"', wsl_codex)

            mcp_config.update_all(win_home, wsl_home, None, remove=True)
            self.assertNotIn(
                "focusstage",
                json.loads((win_home / ".claude.json").read_text(encoding="utf-8"))["mcpServers"],
            )
            self.assertNotIn(
                "focusstage",
                json.loads((wsl_home / ".claude.json").read_text(encoding="utf-8"))["mcpServers"],
            )
            self.assertEqual((win_home / ".codex" / "config.toml").read_text(encoding="utf-8"), "")
            self.assertEqual((wsl_home / ".codex" / "config.toml").read_text(encoding="utf-8"), "")


if __name__ == "__main__":
    unittest.main()
