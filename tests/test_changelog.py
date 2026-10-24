import re
from pathlib import Path

CHANGELOG = Path(__file__).resolve().parent.parent / "CHANGELOG.md"
VERSION = re.compile(r"^## \[(\d+)\.(\d+)\.(\d+)\] - (\d{4}-\d{2}-\d{2})$")


def _headings():
    return [line for line in CHANGELOG.read_text().splitlines() if line.startswith("## ")]


def test_unreleased_comes_first():
    assert _headings()[0] == "## [Unreleased]"


def test_releases_are_dated_semver_in_descending_order():
    releases = _headings()[1:]
    assert releases, "at least one release"
    versions = []
    for heading in releases:
        m = VERSION.match(heading)
        assert m, f"not '## [X.Y.Z] - YYYY-MM-DD': {heading!r}"
        versions.append(tuple(int(p) for p in m.groups()[:3]))
    assert versions == sorted(versions, reverse=True) and len(set(versions)) == len(versions)


def test_every_heading_has_a_link_reference():
    text = CHANGELOG.read_text()
    for heading in _headings():
        name = heading[4:heading.index("]")]
        assert re.search(rf"^\[{re.escape(name)}\]: https://", text, re.M), name
