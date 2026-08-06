.PHONY: build run test check release install-user uninstall-user

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
	cargo run -- --help >/dev/null
	desktop-file-validate data/de.martin.PhotoScanner.desktop

release:
	cargo build --release

install-user: release
	install -Dm755 target/release/photoscanner "$(HOME)/.local/bin/photoscanner"
	install -d "$(HOME)/.local/share/applications"
	desktop-file-install --dir="$(HOME)/.local/share/applications" --set-key=Exec --set-value="$(HOME)/.local/bin/photoscanner gui" data/de.martin.PhotoScanner.desktop
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true

uninstall-user:
	rm -f "$(HOME)/.local/bin/photoscanner"
	rm -f "$(HOME)/.local/share/applications/de.martin.PhotoScanner.desktop"
	update-desktop-database "$(HOME)/.local/share/applications" 2>/dev/null || true
