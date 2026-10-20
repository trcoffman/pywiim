# Project Environment

Environment inspection confirmed this is a Python library and terminal-tool
project, with no React Native, native iOS, or native Android app. Use Jujutsu
for repository operations; both `.jj` and `.git` are present.

```json
{
  "project_type": "Python",
  "is_react_native": false,
  "is_native_ios": false,
  "is_native_android": false,
  "python_requires": ">=3.11",
  "local_test_python": "3.13.3",
  "environment": ".venv",
  "install": "uv pip install --python .venv/bin/python -e '.[dev]'",
  "test": ".venv/bin/pytest tests/unit",
  "receiver": "uv run wiim-receiver SPEAKER_IP",
  "quality": ["black", "isort", "ruff", "mypy"],
  "receiver_notes": "WMRM 4.3 macOS group follower with dependencies installed by default; see docs/user/MAC_RECEIVER.md"
}
```
