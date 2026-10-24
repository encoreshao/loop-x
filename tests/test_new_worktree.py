"""Contract for `new_worktree.sh` / `loopx worktree new` (real local git repos).
Run against Rust with LOOPX_BIN=target/debug/loopx.
"""
import subprocess
from pathlib import Path

from cli_runner import run_cli


def run_git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True)


def make_repo(tmp_path):
    origin = tmp_path / "origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "staging", str(origin)], check=True, capture_output=True)

    repo = tmp_path / "repo"
    subprocess.run(["git", "clone", str(origin), str(repo)], check=True, capture_output=True)
    run_git(repo, "config", "user.email", "test@example.com")
    run_git(repo, "config", "user.name", "Test")
    (repo / "README.md").write_text("hello\n")
    run_git(repo, "add", "README.md")
    run_git(repo, "commit", "-m", "initial commit")
    run_git(repo, "push", "-u", "origin", "staging")
    return repo


def current_branch(path):
    return subprocess.run(
        ["git", "-C", str(path), "branch", "--show-current"],
        check=True, capture_output=True, text=True,
    ).stdout.strip()


def test_new_worktree_creates_branch_from_default(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(result.stdout.strip())

    assert worktree_path.exists()
    assert (worktree_path / "README.md").exists()
    assert current_branch(worktree_path) == "loop/issue-123"


def test_new_worktree_reuses_existing_branch(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    first_path = Path(first.stdout.strip())
    subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(first_path)], check=True)

    second = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(second.stdout.strip())

    assert worktree_path == first_path
    assert worktree_path.exists()
    assert current_branch(worktree_path) == "loop/issue-123"


def test_new_worktree_merges_latest_default_branch_into_existing_branch(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    first_path = Path(first.stdout.strip())

    # Simulate new work landing on staging after the issue branch was created.
    other_clone = tmp_path / "other-clone"
    subprocess.run(["git", "clone", str(tmp_path / "origin.git"), str(other_clone)], check=True, capture_output=True)
    run_git(other_clone, "config", "user.email", "test@example.com")
    run_git(other_clone, "config", "user.name", "Test")
    (other_clone / "NEWS.md").write_text("late-breaking change\n")
    run_git(other_clone, "add", "NEWS.md")
    run_git(other_clone, "commit", "-m", "new staging commit")
    run_git(other_clone, "push", "origin", "staging")

    subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(first_path)], check=True)

    second = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(second.stdout.strip())

    assert (worktree_path / "NEWS.md").exists()
    assert (worktree_path / "README.md").exists()


def test_new_worktree_refuses_when_worktree_has_uncommitted_changes(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(first.stdout.strip())
    (worktree_path / "README.md").write_text("uncommitted local edit\n")

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
    )

    assert result.returncode != 0
    assert "uncommitted changes" in result.stderr.lower()


def add_submodule(repo, tmp_path, path="vendor/gems/example"):
    sub_origin = tmp_path / "submodule-origin.git"
    subprocess.run(["git", "init", "--bare", "-b", "develop", str(sub_origin)], check=True, capture_output=True)

    sub_clone = tmp_path / "submodule-seed"
    subprocess.run(["git", "clone", str(sub_origin), str(sub_clone)], check=True, capture_output=True)
    run_git(sub_clone, "config", "user.email", "test@example.com")
    run_git(sub_clone, "config", "user.name", "Test")
    (sub_clone / "lib.rb").write_text("# gem code\n")
    run_git(sub_clone, "add", "lib.rb")
    run_git(sub_clone, "commit", "-m", "seed submodule")
    run_git(sub_clone, "push", "-u", "origin", "develop")

    run_git(repo, "-c", "protocol.file.allow=always", "submodule", "add", "-b", "develop", str(sub_origin), path)
    run_git(repo, "commit", "-m", "add submodule")
    run_git(repo, "push", "origin", "staging")


def test_new_worktree_copies_untracked_ruby_version_into_new_worktree(tmp_path):
    repo = make_repo(tmp_path)
    # .ruby-version is commonly gitignored (rbenv per-developer pin), so
    # `git worktree add` never copies it — it only checks out tracked files.
    # Without this, rbenv falls back to whatever `.ruby-version` it finds
    # further up the filesystem (e.g. a stale $HOME/.ruby-version), silently
    # running the wrong Ruby in the worktree.
    (repo / ".gitignore").write_text(".ruby-version\n")
    run_git(repo, "add", ".gitignore")
    run_git(repo, "commit", "-m", "gitignore ruby-version")
    run_git(repo, "push", "origin", "staging")
    (repo / ".ruby-version").write_text("3.4.7\n")
    worktree_root = tmp_path / "worktrees"

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(result.stdout.strip())

    assert (worktree_path / ".ruby-version").read_text() == "3.4.7\n"


def test_new_worktree_copies_untracked_ruby_version_into_existing_worktree(tmp_path):
    repo = make_repo(tmp_path)
    (repo / ".gitignore").write_text(".ruby-version\n")
    run_git(repo, "add", ".gitignore")
    run_git(repo, "commit", "-m", "gitignore ruby-version")
    run_git(repo, "push", "origin", "staging")
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(first.stdout.strip())
    assert not (worktree_path / ".ruby-version").exists()

    # The pin appears later (e.g. a developer bumps it locally); a follow-up
    # run on the same issue should pick it up too, not just first creation.
    (repo / ".ruby-version").write_text("3.4.7\n")

    second = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )

    assert (Path(second.stdout.strip()) / ".ruby-version").read_text() == "3.4.7\n"


def test_new_worktree_copies_untracked_database_yml_into_new_worktree(tmp_path):
    repo = make_repo(tmp_path)
    # config/database.yml is commonly gitignored (per-developer DB
    # credentials), so `git worktree add` never copies it — same gap as
    # .ruby-version above. Without it, `bundle exec rspec` can't even boot
    # (no database config), and rubocop-rails' Rails/BulkChangeTable cop
    # can't detect the DB adapter either, which makes any pre-existing
    # `# rubocop:disable Rails/BulkChangeTable` migration comment look like
    # a bogus "redundant disable" lint offense — a false positive purely
    # from the missing file, not real lint debt.
    (repo / "config").mkdir()
    (repo / "config" / "application.rb").write_text("# app config\n")
    run_git(repo, "add", "config/application.rb")
    (repo / ".gitignore").write_text("config/database.yml\n")
    run_git(repo, "add", ".gitignore")
    run_git(repo, "commit", "-m", "add tracked config, gitignore database.yml")
    run_git(repo, "push", "origin", "staging")
    (repo / "config" / "database.yml").write_text("development:\n  adapter: postgresql\n")
    worktree_root = tmp_path / "worktrees"

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(result.stdout.strip())

    assert (worktree_path / "config" / "database.yml").read_text() == "development:\n  adapter: postgresql\n"


def test_new_worktree_copies_untracked_database_yml_into_existing_worktree(tmp_path):
    repo = make_repo(tmp_path)
    (repo / "config").mkdir()
    (repo / "config" / "application.rb").write_text("# app config\n")
    run_git(repo, "add", "config/application.rb")
    (repo / ".gitignore").write_text("config/database.yml\n")
    run_git(repo, "add", ".gitignore")
    run_git(repo, "commit", "-m", "add tracked config, gitignore database.yml")
    run_git(repo, "push", "origin", "staging")
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(first.stdout.strip())
    assert not (worktree_path / "config" / "database.yml").exists()

    # The credentials file appears later (e.g. a developer sets up their
    # local DB after the worktree already exists); a follow-up run on the
    # same issue should pick it up too, not just first creation.
    (repo / "config" / "database.yml").write_text("development:\n  adapter: postgresql\n")

    second = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )

    assert (Path(second.stdout.strip()) / "config" / "database.yml").read_text() == "development:\n  adapter: postgresql\n"


def test_new_worktree_initializes_submodules_in_new_worktree(tmp_path):
    repo = make_repo(tmp_path)
    # `git worktree add` checks out the submodule's placeholder directory
    # (empty) but never runs `submodule update --init` for it — that's a
    # per-worktree step. Left undone, anything depending on the submodule's
    # actual contents (e.g. `bundle install` for a vendored gem) fails.
    add_submodule(repo, tmp_path)
    worktree_root = tmp_path / "worktrees"

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        # Local file:// submodule URLs need explicit opt-in (git disallows
        # them by default since CVE-2022-39253); only needed for this test's
        # local-path fixture, not real SSH/HTTPS submodule URLs.
        env={"GIT_ALLOW_PROTOCOL": "file"},
        check=True,
    )
    worktree_path = Path(result.stdout.strip())

    assert (worktree_path / "vendor/gems/example/lib.rb").read_text() == "# gem code\n"


def test_new_worktree_initializes_submodules_in_existing_worktree(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(first.stdout.strip())
    assert not (worktree_path / "vendor/gems/example").exists()

    # The submodule is added later (e.g. staging picks it up after the issue
    # branch was created); a follow-up run on the same issue, against the
    # same still-existing worktree directory, should init it too.
    add_submodule(repo, tmp_path)

    second = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        env={"GIT_ALLOW_PROTOCOL": "file"},
        check=True,
    )

    assert (Path(second.stdout.strip()) / "vendor/gems/example/lib.rb").read_text() == "# gem code\n"


def test_new_worktree_aborts_cleanly_on_merge_conflict(tmp_path):
    repo = make_repo(tmp_path)
    worktree_root = tmp_path / "worktrees"

    first = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
        check=True,
    )
    worktree_path = Path(first.stdout.strip())
    (worktree_path / "README.md").write_text("conflicting local change\n")
    run_git(worktree_path, "add", "README.md")
    run_git(worktree_path, "commit", "-m", "local conflicting commit")

    other_clone = tmp_path / "other-clone"
    subprocess.run(["git", "clone", str(tmp_path / "origin.git"), str(other_clone)], check=True, capture_output=True)
    run_git(other_clone, "config", "user.email", "test@example.com")
    run_git(other_clone, "config", "user.name", "Test")
    (other_clone / "README.md").write_text("origin-side conflicting change\n")
    run_git(other_clone, "add", "README.md")
    run_git(other_clone, "commit", "-m", "origin conflicting commit")
    run_git(other_clone, "push", "origin", "staging")

    subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(worktree_path)], check=True)

    result = run_cli(
        "new_worktree.sh", str(repo), "staging", "123", str(worktree_root),
    )

    assert result.returncode != 0
    assert "conflict" in result.stderr.lower()
    # MERGE_HEAD must not be left dangling after the abort
    merge_head_check = subprocess.run(
        ["git", "-C", str(worktree_path), "rev-parse", "-q", "--verify", "MERGE_HEAD"],
        capture_output=True, text=True,
    )
    assert merge_head_check.returncode != 0
