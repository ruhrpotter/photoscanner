.PHONY: build run test check audit release install-user uninstall-user

build:
	cargo build

run:
	cargo run --release

test:
	cargo test --all-targets

check:
	cargo fmt -- --check
	cargo clippy --all-targets -- -D warnings
	cargo test --all-targets
	cargo test --doc
	cargo run -- --help >/dev/null
	desktop-file-validate data/de.martin.PhotoScanner.desktop
	appstreamcli validate --strict --no-net --override=url-homepage-missing=pedantic data/de.martin.PhotoScanner.metainfo.xml

audit:
	cargo audit

release:
	cargo build --release

install-user: release
	install -Dm755 target/release/photoscanner "$(HOME)/.local/bin/photoscanner"
	install -d "$(HOME)/.local/share/applications"
	desktop-file-install --dir="$(HOME)/.local/share/applications" --set-key=Exec --set-value="$(HOME)/.local/bin/photoscanner gui" data/de.martin.PhotoScanner.desktop
	install -Dm644 data/de.martin.PhotoScanner.metainfo.xml "$(HOME)/.local/share/metainfo/de.martin.PhotoScanner.metainfo.xml"
	install -Dm644 data/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg "$(HOME)/.local/share/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg"
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true
	gtk-update-icon-cache -f -t "$(HOME)/.local/share/icons/hicolor" 2>/dev/null || true

uninstall-user:
	rm -f "$(HOME)/.local/bin/photoscanner"
	rm -f "$(HOME)/.local/share/applications/de.martin.PhotoScanner.desktop"
	rm -f "$(HOME)/.local/share/metainfo/de.martin.PhotoScanner.metainfo.xml"
	rm -f "$(HOME)/.local/share/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg"
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true
	gtk-update-icon-cache -f -t "$(HOME)/.local/share/icons/hicolor" 2>/dev/null || true
