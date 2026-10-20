# Project Environment

This is a Python library with a standalone Rust terminal receiver under
`rust/receiver`, with no React Native, native iOS, or native Android app. Use Jujutsu
for repository operations; both `.jj` and `.git` are present.

```json
{
  "project_type": "Python library and Rust terminal receiver",
  "is_react_native": false,
  "is_native_ios": false,
  "is_native_android": false,
  "python_requires": ">=3.11",
  "local_test_python": "3.13.3",
  "environment": ".venv",
  "install": "uv pip install --python .venv/bin/python -e '.[dev]'",
  "test": ".venv/bin/pytest tests/unit",
  "receiver": "cargo run --release -- SPEAKER_IP (or ./target/release/wiim-receiver SPEAKER_IP)",
  "rust_build": "cargo build --release --locked",
  "rust_test": "cargo test --locked",
  "quality": ["black", "isort", "ruff", "mypy"],
  "receiver_notes": "Native Rust WMRM 4.3 macOS group follower; Python reference remains available via uv run wiim-receiver. See docs/user/MAC_RECEIVER.md"
}
```
