# Maintainer: loeclos <loeclos@users.noreply.github.com>

pkgname=omarchy-recipe
pkgver=0.2.0
pkgrel=1
pkgdesc='Export, import, and bake portable Omarchy configuration recipes'
arch=('x86_64')
url='https://github.com/loeclos/omarchy-recipe'
license=('MIT')
depends=('gcc-libs' 'glibc' 'git' 'pacman' 'systemd' 'tar' 'zstd')
makedepends=('python' 'rust')
optdepends=(
  'rsync: per-file backups when restoring dotfiles'
  'sudo: install packages and services as a non-root user'
  'yay: install AUR packages from network-mode recipes'
  'paru: alternative AUR helper'
  'docker: build custom Omarchy ISOs'
  'qemu-system-x86: boot-test baked ISOs'
)
source=(
  "$pkgname-$pkgver.tar.gz::https://github.com/loeclos/omarchy-recipe/archive/3dc58ff56d7ab875e11b483a98d95752744bbf2f.tar.gz"
  'LICENSE'
)
sha256sums=(
  '4017496f5f48c1d52e112ab1b1c7ba0507761c82a62e042181fbf451b3ff4ec0'
  '0f91b67d18b43c46937d8e278f412abe4481954ea539939da67a02fb08beb904'
)

build() {
  cd "$srcdir/$pkgname-3dc58ff56d7ab875e11b483a98d95752744bbf2f"
  CARGO_TARGET_DIR=target cargo build --release --locked
}

check() {
  cd "$srcdir/$pkgname-3dc58ff56d7ab875e11b483a98d95752744bbf2f"
  cargo test --locked
}

package() {
  cd "$srcdir/$pkgname-3dc58ff56d7ab875e11b483a98d95752744bbf2f"

  install -Dm755 target/release/omarchy-recipe \
    "$pkgdir/usr/bin/omarchy-recipe"

  for sub in export import validate build-iso; do
    ln -s omarchy-recipe "$pkgdir/usr/bin/omarchy-recipe-$sub"
  done

  install -Dm644 "$srcdir/LICENSE" \
    "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md \
    "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 schema/recipe.schema.json \
    "$pkgdir/usr/share/$pkgname/recipe.schema.json"
}
