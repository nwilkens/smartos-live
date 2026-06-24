#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#

# gmake wrapper that drives cargo for the platform build. The top-level
# smartos-live build invokes `gmake -C src/rust all`; the install step
# copies the resulting binaries out of $(RUST_RELEASE_DIR).

CARGO ?=		cargo
CARGO_TARGET ?=		x86_64-unknown-illumos
RUST_RELEASE_DIR =	target/$(CARGO_TARGET)/release

# Hermetic build: vendor/ is committed and .cargo/config.toml redirects
# crates.io to it. --offline forbids any network access; --locked pins
# Cargo.lock (commit it with every dependency change).
CARGO_BUILD_FLAGS =	--release --target $(CARGO_TARGET) --offline --locked

.PHONY: all check test clean

all:
	$(CARGO) build $(CARGO_BUILD_FLAGS)

# Lints + unit tests run on the host (default target) so they also work on
# a developer's non-illumos machine via the trait-backed fake host layer.
check:
	$(CARGO) clippy -- -D warnings
	$(CARGO) test

test:
	$(CARGO) test

clean:
	$(CARGO) clean
