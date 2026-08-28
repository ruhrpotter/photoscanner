.PHONY: build run test check audit pot locale release install-user uninstall-user

build:
	cargo build

run:
	cargo run --release

test:
	cargo test --all-targets

check:
	cargo fmt -- --check
	cargo clippy --all-targets -- -D warnings
	LC_ALL=C cargo test --all-targets
	LC_ALL=C cargo test --doc
	LC_ALL=C cargo run -- --help >/dev/null
	msgfmt --check po/de.po -o /dev/null
	msgcmp --use-fuzzy po/de.po po/photoscanner.pot
	desktop-file-validate data/de.martin.PhotoScanner.desktop
	appstreamcli validate --strict --no-net data/de.martin.PhotoScanner.metainfo.xml

pot:
	xtr src/main.rs src/lib.rs -k tr -k tr_args -k 'trn:1,2' --package-name photoscanner --package-version 0.2.0 -o po/photoscanner.pot

locale:
	mkdir -p target/locale/de/LC_MESSAGES
	msgfmt po/de.po -o target/locale/de/LC_MESSAGES/photoscanner.mo

audit:
	cargo audit

release:
	cargo build --release

install-user: release locale
	install -Dm755 target/release/photoscanner "$(HOME)/.local/bin/photoscanner"
	install -d "$(HOME)/.local/share/applications"
	desktop-file-install --dir="$(HOME)/.local/share/applications" --set-key=Exec --set-value="$(HOME)/.local/bin/photoscanner gui" data/de.martin.PhotoScanner.desktop
	install -Dm644 data/de.martin.PhotoScanner.metainfo.xml "$(HOME)/.local/share/metainfo/de.martin.PhotoScanner.metainfo.xml"
	install -Dm644 data/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg "$(HOME)/.local/share/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg"
	install -Dm644 target/locale/de/LC_MESSAGES/photoscanner.mo "$(HOME)/.local/share/locale/de/LC_MESSAGES/photoscanner.mo"
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true
	gtk-update-icon-cache -f -t "$(HOME)/.local/share/icons/hicolor" 2>/dev/null || true

uninstall-user:
	rm -f "$(HOME)/.local/bin/photoscanner"
	rm -f "$(HOME)/.local/share/applications/de.martin.PhotoScanner.desktop"
	rm -f "$(HOME)/.local/share/metainfo/de.martin.PhotoScanner.metainfo.xml"
	rm -f "$(HOME)/.local/share/icons/hicolor/scalable/apps/de.martin.PhotoScanner.svg"
	rm -f "$(HOME)/.local/share/locale/de/LC_MESSAGES/photoscanner.mo"
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true
	gtk-update-icon-cache -f -t "$(HOME)/.local/share/icons/hicolor" 2>/dev/null || true
