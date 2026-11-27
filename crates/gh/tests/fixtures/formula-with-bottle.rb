class Hgl < Formula
  desc "Compiler"
  homepage "https://github.com/o/source"
  url "https://github.com/o/source/releases/download/0.2.12/hgl-0.2.12-macos-arm64.tar.gz"
  sha256 "57996676dcf69f385fec023076be3a6b9a6405d37a49116701b6370eccd89672"
  license "MIT"

  bottle do
    root_url "https://github.com/o/source/releases/download/0.2.12"
    sha256 cellar: :any, arm64_tahoe: "e14fa4f10c6286176dd6fe221515e15cb5668ca10a2ab0359338f15d1035e2f3"
  end

  def install
    libexec.install Dir["*"]
  end
end
