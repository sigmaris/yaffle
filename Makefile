CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = x86_64-linux-gnu-gcc
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER

TOSHI_TYPES_SOURCES := $(shell find Toshi/toshi-types -type f -name '*.rs' -or -type f -name '*.toml')
TOSHI_SERVER_SOURCES := $(shell find Toshi/toshi-server -type f -name '*.rs' -or -type f -name '*.toml') $(TOSHI_TYPES_SOURCES)
TOSHI_CLIENT_SOURCES := $(shell find Toshi/toshi-client -type f -name '*.rs' -or -type f -name '*.toml') $(TOSHI_TYPES_SOURCES)
TOSHI_PARENT_SOURCES = Toshi/Cargo.lock Toshi/Cargo.toml

YAFFLE_SOURCES := $(shell find yaffle-server yaffle-macros -type f -name '*.rs' -or -type f -name '*.toml')
YAFFLE_PARENT_SOURCES := Cargo.lock Cargo.toml

.PHONY:all
all: image/.built

image/.built: Toshi/target/x86_64-unknown-linux-gnu/release/toshi target/x86_64-unknown-linux-gnu/release/yaffle-server image/*.*
	packer build -var deb_arch=amd64 -var container_tag="$$(git describe --tags --dirty --always)" image
	touch $@

Toshi/target/x86_64-unknown-linux-gnu/release/toshi: $(TOSHI_SERVER_SOURCES) $(TOSHI_PARENT_SOURCES)
	cd Toshi && cargo build --release --target x86_64-unknown-linux-gnu --bin toshi

target/x86_64-unknown-linux-gnu/release/yaffle-server: $(YAFFLE_SOURCES) $(YAFFLE_PARENT_SOURCES) $(TOSHI_CLIENT_SOURCES) $(TOSHI_PARENT_SOURCES)
	cargo build --release --target x86_64-unknown-linux-gnu --bin yaffle-server

clean:
	cargo clean
	cd Toshi; cargo clean
	rm image/.built
