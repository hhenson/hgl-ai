# frozen_string_literal: true

# Installs the hgl-ai command line from its release archives.
# Rendered by `hgl-ai gh tap formula` from packaging/hgl-ai.rb.tmpl in hhenson/hgl-ai.
class HglAi < Formula
  desc "Coordination tooling for a mixed fleet of coding agents"
  homepage "https://github.com/hhenson/hgl-ai"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hhenson/hgl-ai/releases/download/v0.1.0/hgl-ai-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    end
    on_intel do
      url "https://github.com/hhenson/hgl-ai/releases/download/v0.1.0/hgl-ai-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hhenson/hgl-ai/releases/download/v0.1.0/hgl-ai-0.1.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    end
    on_intel do
      url "https://github.com/hhenson/hgl-ai/releases/download/v0.1.0/hgl-ai-0.1.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
    end
  end

  def install
    bin.install "hgl-ai"
  end

  def caveats
    <<~EOS
      On the one host that runs the hub (it installs nats-server when it is missing):
        hgl-ai server install
      On every host, the hub's too:
        hgl-ai client install --hub nats://<hub>:4222 --token-file <token> --host <label>
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hgl-ai --version")
  end
end
