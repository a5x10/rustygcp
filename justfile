default:
    @just --list

# the whole gate: format, lints, every test (the Firestore ones on the emulator)
test:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    scripts/with-firestore-emulator.sh cargo test
