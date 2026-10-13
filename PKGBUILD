# Maintainer: YATAVO <https://github.com/YATAVO>
pkgname=yaplayer
pkgver=0.1.0
pkgrel=1
pkgdesc="A lightweight Rust & GTK4 music player with YouTube & Spotify streaming"
arch=('x86_64')
url="https://github.com/YATAVO/yaplayer"
license=('MIT')
depends=('gtk4' 'mpv' 'yt-dlp' 'curl')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz")
noextract=("$pkgname-$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/yaplayer" "$pkgdir/usr/bin/yaplayer"
    install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 "yaplayer.desktop" "$pkgdir/usr/share/applications/yaplayer.desktop"
    

    install -Dm644 "yaplayer.png" "$pkgdir/usr/share/pixmaps/yaplayer.png"
}