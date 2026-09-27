from pathlib import Path


ROOT = Path(__file__).parent

for relative_path in [
    "SECURITY.md",
    "CONTRIBUTING.md",
    "CHANGELOG.md",
    "docs/THREAT_MODEL.md",
    "docs/PRODUCT_STATUS.md",
    "docs/SECURITY_ADVISORIES.md",
    ".cargo/audit.toml",
    ".github/workflows/ci.yml",
    ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/ISSUE_TEMPLATE/security.yml",
    ".github/pull_request_template.md",
]:
    assert (ROOT / relative_path).is_file(), f"missing {relative_path}"

readme = (ROOT / "README.md").read_text()
for marker in [
    "https://girasolbot.com",
    "https://x.com/girasolbot",
    "No official Girasol token exists yet",
    "not a guarantee",
    "docs/PRODUCT_STATUS.md",
]:
    assert marker in readme, f"README missing {marker}"

print("repository trust verification: PASS")
