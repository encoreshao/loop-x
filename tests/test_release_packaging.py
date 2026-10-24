"""Release plumbing that can be checked without credentials: the changelog ->
release-notes extractor, the Homebrew formula template and its renderer, and
the shape of the release workflow."""
import re
import subprocess
from pathlib import Path

import pytest
import yaml

REPO = Path(__file__).resolve().parent.parent
NOTES = REPO / "bin" / "scripts" / "release_notes.sh"
RENDER = REPO / "packaging" / "homebrew" / "render_formula.sh"
FORMULA = REPO / "packaging" / "homebrew" / "loopx.rb"
WORKFLOW = REPO / ".github" / "workflows" / "release.yml"

SHA_A = "a" * 64
SHA_B = "b" * 64

CHANGELOG = """# Changelog

## [Unreleased]

### Added

- not released yet

## [1.2.0] - 2026-01-02

### Added

- one

### Fixed

- two

## [1.1.0] - 2026-01-01

### Added

- old
"""


def notes(tmp_path, version, text=CHANGELOG):
    cl = tmp_path / "CHANGELOG.md"
    cl.write_text(text)
    return subprocess.run(["bash", str(NOTES), version, "--changelog", str(cl)], capture_output=True, text=True)


def test_release_notes_are_exactly_the_versions_section(tmp_path):
    r = notes(tmp_path, "1.2.0")
    assert r.returncode == 0, r.stderr
    assert r.stdout == "### Added\n\n- one\n\n### Fixed\n\n- two\n"


def test_release_notes_accept_a_leading_v_and_the_last_section(tmp_path):
    r = notes(tmp_path, "v1.1.0")
    assert r.stdout == "### Added\n\n- old\n"


def test_release_notes_fail_for_missing_or_empty_sections(tmp_path):
    assert notes(tmp_path, "9.9.9").returncode == 1
    empty = CHANGELOG.replace("### Added\n\n- one\n\n### Fixed\n\n- two\n\n", "")
    r = notes(tmp_path, "1.2.0", empty)
    assert r.returncode == 1 and "no non-empty" in r.stderr


def test_the_real_changelog_has_notes_for_the_current_release():
    r = subprocess.run(["bash", str(NOTES), "0.4.0"], capture_output=True, text=True)
    assert r.returncode == 0 and r.stdout.strip()


def render(version="1.2.3", sha=SHA_A, support=SHA_B, *extra):
    return subprocess.run(["bash", str(RENDER), version, sha, support, *extra], capture_output=True, text=True)


def test_formula_template_has_service_test_and_placeholders():
    text = FORMULA.read_text()
    assert 'run [opt_bin/"loopx", "serve"' in text and "service do" in text
    assert 'shell_output("#{bin}/loopx --version")' in text
    assert "test do" in text and "bin.install \"loopx\"" in text
    for placeholder in ("@VERSION@", "@SHA256@", "@SUPPORT_SHA256@", "@REPO@"):
        assert placeholder in text


def test_render_formula_fills_every_placeholder():
    r = render("v1.2.3")
    assert r.returncode == 0, r.stderr
    assert not re.search(r"@[A-Z_0-9]+@", r.stdout)
    assert 'url "https://github.com/encoreshao/loop-engineering/releases/download/v1.2.3/loopx-darwin-universal.tar.gz"' in r.stdout
    assert f'sha256 "{SHA_A}"' in r.stdout and f'sha256 "{SHA_B}"' in r.stdout
    assert 'version "1.2.3"' in r.stdout


def test_render_formula_uses_the_given_repo():
    assert "github.com/acme/fork/releases" in render("1.0.0", SHA_A, SHA_B, "acme/fork").stdout


@pytest.mark.parametrize("args", [("nope", SHA_A, SHA_B), ("1.0.0", "short", SHA_B), ("1.0.0", SHA_A, "XYZ")])
def test_render_formula_rejects_bad_input(args):
    assert render(*args).returncode == 1


def test_formula_is_valid_ruby_when_ruby_is_available():
    r = subprocess.run(["ruby", "-c"], input=render().stdout, capture_output=True, text=True) if _has("ruby") else None
    if r is None:
        pytest.skip("no ruby")
    assert r.returncode == 0, r.stderr


def _has(cmd):
    return subprocess.run(["which", cmd], capture_output=True).returncode == 0


@pytest.fixture(scope="module")
def workflow():
    return yaml.safe_load(WORKFLOW.read_text())


def test_release_workflow_triggers_on_version_tags(workflow):
    triggers = workflow.get(True) or workflow["on"]  # PyYAML reads `on` as True
    assert triggers["push"]["tags"] == ["v*"]


def test_release_workflow_has_the_expected_jobs_and_ordering(workflow):
    jobs = workflow["jobs"]
    assert set(jobs) == {"binary", "app", "release", "homebrew"}
    assert jobs["app"]["needs"] == "binary"
    assert set(jobs["release"]["needs"]) == {"binary", "app"}
    assert jobs["homebrew"]["needs"] == "release"


def test_release_workflow_covers_the_checklist():
    text = WORKFLOW.read_text()
    for needle in ("lipo -create", "codesign", "notarytool submit", "stapler staple", "build_loopx_app.sh",
                   "SHA256SUMS", "release_notes.sh", "gh release", "generate_appcast", "render_formula.sh",
                   "loopx-darwin-universal.tar.gz", "loopx-support.tar.gz"):
        assert needle in text, needle


def test_every_signing_step_is_skippable_without_secrets():
    text = WORKFLOW.read_text()
    for needle in ("building UNSIGNED binaries", "skipping notarization", "skipping the Sparkle appcast",
                   "skipping the Homebrew tap update"):
        assert needle in text, needle


def test_release_assets_match_what_the_installer_downloads():
    install = (REPO / "bin" / "scripts" / "install-loopx.sh").read_text()
    wf = WORKFLOW.read_text()
    for asset in ("loopx-darwin-$ARCH.tar.gz", "loopx-darwin-universal.tar.gz", "loopx-support.tar.gz", "SHA256SUMS"):
        assert asset in install
    for asset in ("loopx-darwin-$arch.tar.gz", "loopx-darwin-universal.tar.gz", "loopx-support.tar.gz", "SHA256SUMS"):
        assert asset in wf or asset.replace("$arch", "") in wf
