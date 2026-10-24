# Homebrew formula template for the tap (encoreshao/homebrew-tap, Formula/loopx.rb).
# The @...@ placeholders are filled in by packaging/homebrew/render_formula.sh,
# which the release workflow (.github/workflows/release.yml) runs after the
# GitHub Release exists, so the checksums are those of the released assets.
class Loopx < Formula
  desc "Loop engineering: scheduler, CLI and dashboard backend in one binary"
  homepage "https://github.com/@REPO@"
  url "https://github.com/@REPO@/releases/download/v@VERSION@/loopx-darwin-universal.tar.gz"
  version "@VERSION@"
  sha256 "@SHA256@"
  license "MIT"

  # Config and launchd templates (what install-loopx.sh scaffolds from).
  resource "support" do
    url "https://github.com/@REPO@/releases/download/v@VERSION@/loopx-support.tar.gz"
    sha256 "@SUPPORT_SHA256@"
  end

  depends_on :macos

  def install
    bin.install "loopx"
    resource("support").stage { pkgshare.install "config", "launchd" }
  end

  # `brew services start loopx` runs the dashboard backend. Homebrew allows a
  # single service per formula, so the scheduler agent is set up by hand (see
  # the caveats).
  service do
    run [opt_bin/"loopx", "serve", "--port", "8420", "--no-legacy"]
    keep_alive true
    log_path var/"log/loopx.log"
    error_log_path var/"log/loopx.err.log"
    environment_variables PATH: std_service_path_env
  end

  def caveats
    <<~EOS
      First run: create your config skeleton (existing files are kept):

        mkdir -p ~/.loopx
        for f in projects topics loops ai_cli; do
          [ -f ~/.loopx/$f.json ] || sed "s|{{HOME}}|$HOME|g" #{pkgshare}/config/$f.json.template > ~/.loopx/$f.json
        done

      Then check what is left to set up (AI CLI, GitLab/Slack credentials):

        loopx doctor

      Start the dashboard backend with `brew services start loopx`.
      The scheduler (`loopx scheduler`, every 15 minutes) is a second launchd
      job; render and load it from #{pkgshare}/launchd/loopx/ or use the
      installer: https://github.com/@REPO@/blob/main/docs/RELEASING.md
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/loopx --version")
  end
end
