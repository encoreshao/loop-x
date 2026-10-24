from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def test_task_doc_exists_with_fixed_safety_boundary():
    text = (REPO_ROOT / "docs" / "tasks" / "inbox-triage-loop.md").read_text()
    for needle in ("## Safety boundary", "Never sends mail", "Mail.Send", "Keychain", "inboxes.json", "loops.json"):
        assert needle in text


def test_task_index_and_readme_mention_the_loop():
    assert "docs/tasks/inbox-triage-loop.md" in (REPO_ROOT / "TASK.md").read_text()
    assert "inbox-triage-loop" in (REPO_ROOT / "README.md").read_text()


def test_claude_md_documents_keychain_sandbox_rule():
    text = (REPO_ROOT / "CLAUDE.md").read_text()
    assert "loop-engineering.mail" in text and ".sandbox-<hash>" in text


def test_docs_state_claude_only_stdin_keychain_and_loop_label_rules():
    task = (REPO_ROOT / "docs" / "tasks" / "inbox-triage-loop.md").read_text()
    readme = (REPO_ROOT / "README.md").read_text()
    for text in (task, readme):
        assert "requires the Claude CLI" in text
        assert "security -i" in text
        assert "must start with `Loop/`" in text
    assert "codex exec\n  --sandbox" not in task and "Known gap" not in task
    assert "briefly puts the token on" not in task
