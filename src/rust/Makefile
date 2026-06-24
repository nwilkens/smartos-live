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

# Dependencies are pinned by Cargo.lock (--locked) and fetched from crates.io.
# vendor/ is not committed; for a fully offline build run `gmake vendor` and
# uncomment the vendored-sources redirect in .cargo/config.toml, then add
# --offline here.
CARGO_BUILD_FLAGS =	--release --target $(CARGO_TARGET) --locked

.PHONY: all check test clean vendor

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

# Regenerate the offline dependency tree (not committed). After this, enable
# the vendored-sources redirect in .cargo/config.toml for `--offline` builds.
vendor:
	$(CARGO) vendor vendor
